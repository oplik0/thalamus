use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::{job_name, text, ArtifactRef, Error, Result, RunRequest};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Run {
    pub id: Uuid,
    pub team: String,
    pub actor: String,
    pub idempotency_key: String,
    pub fingerprint: String,
    pub request: RunRequest,
    pub job_name: String,
    pub job_uid: Option<String>,
    /// Recovery relies on namespace RBAC and admission protecting reserved Jobs.
    pub ownership_nonce: Uuid,
    pub job_spec: Option<serde_json::Value>,
    pub job_namespace: Option<String>,
    pub state: String,
    pub artifacts: Option<Vec<ArtifactRef>>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

fn decode(row: &sqlx::postgres::PgRow) -> Result<Run> {
    let artifact: Option<serde_json::Value> = row.try_get("artifact")?;
    Ok(Run {
        id: row.try_get("id")?,
        team: row.try_get("team")?,
        actor: row.try_get("actor")?,
        idempotency_key: row.try_get("idempotency_key")?,
        fingerprint: row.try_get("fingerprint")?,
        request: serde_json::from_value(row.try_get("request")?)?,
        job_name: row.try_get("job_name")?,
        job_uid: row.try_get("job_uid")?,
        state: row.try_get("state")?,
        ownership_nonce: row.try_get("ownership_nonce")?,
        job_spec: row.try_get("job_spec")?,
        job_namespace: row.try_get("job_namespace")?,
        artifacts: artifact.map(serde_json::from_value).transpose()?,
        error: row.try_get("error")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

#[derive(Clone, Debug)]
pub enum Transition {
    Started,
    JobSucceeded,
    JobFailed { error: String },
    IngestionSucceeded { artifacts: Vec<ArtifactRef> },
    IngestionFailed { error: String },
    RetryIngestion,
}

type TransitionOutcome<'a> = (&'static str, Option<&'a str>, Option<&'a Vec<ArtifactRef>>);

impl Transition {
    fn next(&self, state: &str) -> Result<TransitionOutcome<'_>> {
        match (state, self) {
            ("pending", Self::Started) => Ok(("running", None, None)),
            ("running", Self::JobSucceeded) => Ok(("ingesting", None, None)),
            ("pending" | "running", Self::JobFailed { error }) => Ok(("failed", Some(error), None)),
            ("ingesting", Self::IngestionSucceeded { artifacts }) => {
                Ok(("succeeded", None, Some(artifacts)))
            }
            ("ingesting", Self::IngestionFailed { error }) => {
                Ok(("ingestion_failed", Some(error), None))
            }
            ("ingestion_failed", Self::RetryIngestion) => Ok(("ingesting", None, None)),
            _ => Err(Error::InvalidTransition),
        }
    }
}

#[derive(Clone)]
pub struct Store {
    pool: PgPool,
}

impl Store {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Install the schema before starting server or worker. Deployment migrations may
    /// instead execute the bundled SQL directly.
    pub async fn migrate(&self) -> Result<()> {
        sqlx::raw_sql(include_str!("../migrations/0001_runs.sql"))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn create(
        &self,
        team: &str,
        actor: &str,
        idempotency_key: &str,
        request: &RunRequest,
    ) -> Result<Run> {
        text(team, "team", 256)?;
        text(actor, "actor", 256)?;
        text(idempotency_key, "idempotency key", 256)?;
        let fingerprint = request.fingerprint()?;
        let id = Uuid::new_v4();
        let mut tx = self.pool.begin().await?;
        let inserted = sqlx::query("INSERT INTO thalamus_runs (id,team,actor,idempotency_key,fingerprint,request,job_name,state,ownership_nonce) VALUES ($1,$2,$3,$4,$5,$6,$7,'pending',$8) ON CONFLICT (team,idempotency_key) DO NOTHING RETURNING *")
            .bind(id).bind(team).bind(actor).bind(idempotency_key).bind(&fingerprint)
            .bind(serde_json::to_value(request)?).bind(job_name(id)).bind(Uuid::new_v4()).fetch_optional(&mut *tx).await?;
        let row = if let Some(row) = inserted {
            sqlx::query("INSERT INTO thalamus_run_audit (run_id,team,actor,action,detail) VALUES ($1,$2,$3,'created',$4)")
                .bind(id).bind(team).bind(actor).bind(serde_json::json!({"fingerprint": fingerprint})).execute(&mut *tx).await?;
            row
        } else {
            let row =
                sqlx::query("SELECT * FROM thalamus_runs WHERE team=$1 AND idempotency_key=$2")
                    .bind(team)
                    .bind(idempotency_key)
                    .fetch_one(&mut *tx)
                    .await?;
            if row.try_get::<String, _>("fingerprint")? != fingerprint
                || row.try_get::<String, _>("actor")? != actor
            {
                return Err(Error::IdempotencyConflict);
            }
            row
        };
        let run = decode(&row)?;
        tx.commit().await?;
        Ok(run)
    }

    pub async fn get(&self, team: &str, id: Uuid) -> Result<Run> {
        let row = sqlx::query("SELECT * FROM thalamus_runs WHERE team=$1 AND id=$2")
            .bind(team)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(Error::NotFound)?;
        decode(&row)
    }

    pub async fn list(&self, team: &str, limit: u32) -> Result<Vec<Run>> {
        if !(1..=100).contains(&limit) {
            return Err(Error::Invalid("limit must be 1..=100".into()));
        }
        let rows = sqlx::query(
            "SELECT * FROM thalamus_runs WHERE team=$1 ORDER BY created_at DESC, id DESC LIMIT $2",
        )
        .bind(team)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(decode).collect()
    }

    /// Persist the desired Job before CREATE. Repeated calls return the original
    /// spec even if operator templates have since changed.
    pub async fn prepare_job(
        &self,
        team: &str,
        actor: &str,
        id: Uuid,
        templates: &crate::Templates,
    ) -> Result<k8s_openapi::api::batch::v1::Job> {
        text(actor, "actor", 256)?;
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT * FROM thalamus_runs WHERE team=$1 AND id=$2 FOR UPDATE")
            .bind(team)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Error::NotFound)?;
        let run = decode(&row)?;
        let job = if let (Some(spec), Some(namespace)) = (&run.job_spec, &run.job_namespace) {
            serde_json::from_value(serde_json::json!({
                "apiVersion":"batch/v1", "kind":"Job",
                "metadata":{"name":run.job_name,"namespace":namespace,
                    "labels":{"app.kubernetes.io/managed-by":"thalamus","thalamus.io/run-id":run.id.to_string()},
                    "annotations":{"thalamus.io/ownership-nonce":run.ownership_nonce.to_string()}},
                "spec":spec
            }))?
        } else {
            if run.state != "pending" {
                return Err(Error::InvalidTransition);
            }
            let job = crate::build_job(&run, templates)?;
            let spec = serde_json::to_value(job.spec.as_ref().ok_or(Error::ForeignJob)?)?;
            let namespace = job.metadata.namespace.as_deref().ok_or(Error::ForeignJob)?;
            sqlx::query("UPDATE thalamus_runs SET job_spec=$1,job_namespace=$2,updated_at=now() WHERE id=$3")
                .bind(spec).bind(namespace).bind(id).execute(&mut *tx).await?;
            audit(
                &mut tx,
                id,
                team,
                actor,
                "job_prepared",
                serde_json::json!({"namespace":namespace,"name":run.job_name}),
            )
            .await?;
            job
        };
        tx.commit().await?;
        Ok(job)
    }

    /// Accept a successful CREATE response or a GET after CREATE conflict, only
    /// after verifying nonce, namespace, labels, and the persisted exact spec.
    /// Namespace RBAC/admission must reserve Thalamus names and ownership fields.
    pub async fn record_created_job(
        &self,
        team: &str,
        actor: &str,
        id: Uuid,
        job: &k8s_openapi::api::batch::v1::Job,
    ) -> Result<Run> {
        text(actor, "actor", 256)?;
        let uid = job.metadata.uid.as_deref().ok_or(Error::ForeignJob)?;
        text(uid, "job uid", 256)?;
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT * FROM thalamus_runs WHERE team=$1 AND id=$2 FOR UPDATE")
            .bind(team)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Error::NotFound)?;
        let run = decode(&row)?;
        if run.job_uid.is_none() && run.state != "pending" {
            return Err(Error::ForeignJob);
        }
        crate::jobs::verify_recoverable_job(&run, job)?;
        if run.job_uid.is_none() {
            sqlx::query("UPDATE thalamus_runs SET job_uid=$1,updated_at=now() WHERE id=$2")
                .bind(uid)
                .bind(id)
                .execute(&mut *tx)
                .await?;
            audit(
                &mut tx,
                id,
                team,
                actor,
                "job_created",
                serde_json::json!({"uid": uid}),
            )
            .await?;
        }
        let row = sqlx::query("SELECT * FROM thalamus_runs WHERE id=$1")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
        let result = decode(&row)?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn transition(
        &self,
        team: &str,
        actor: &str,
        id: Uuid,
        transition: Transition,
    ) -> Result<Run> {
        text(actor, "actor", 256)?;
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT * FROM thalamus_runs WHERE team=$1 AND id=$2 FOR UPDATE")
            .bind(team)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Error::NotFound)?;
        let run = decode(&row)?;
        let repeated = match &transition {
            Transition::Started => matches!(
                run.state.as_str(),
                "running" | "ingesting" | "ingestion_failed" | "succeeded"
            ),
            Transition::JobSucceeded => matches!(
                run.state.as_str(),
                "ingesting" | "ingestion_failed" | "succeeded"
            ),
            Transition::JobFailed { error } => {
                run.state == "failed" && run.error.as_deref() == Some(error)
            }
            Transition::IngestionSucceeded { artifacts } => {
                run.state == "succeeded" && run.artifacts.as_ref() == Some(artifacts)
            }
            Transition::IngestionFailed { error } => {
                run.state == "ingestion_failed" && run.error.as_deref() == Some(error)
            }
            Transition::RetryIngestion => run.state == "ingesting",
        };
        if repeated {
            tx.commit().await?;
            return Ok(run);
        }
        let (state, error, artifact) = transition.next(&run.state)?;
        if matches!(transition, Transition::Started | Transition::JobSucceeded)
            && run.job_uid.is_none()
        {
            return Err(Error::ForeignJob);
        }
        if let Some(error) = error {
            text(error, "error", 4096)?;
        }
        if let Some(artifacts) = artifact {
            if artifacts.is_empty() || artifacts.len() > 128 {
                return Err(Error::Invalid("expected 1..=128 artifacts".into()));
            }
            for artifact in artifacts {
                artifact.validate_for(id)?;
            }
        }
        let artifact = artifact.map(serde_json::to_value).transpose()?;
        let row = sqlx::query("UPDATE thalamus_runs SET state=$1,error=$2,artifact=COALESCE($3,artifact),updated_at=now() WHERE id=$4 RETURNING *")
            .bind(state).bind(error).bind(artifact).bind(id).fetch_one(&mut *tx).await?;
        audit(
            &mut tx,
            id,
            team,
            actor,
            "state_changed",
            serde_json::json!({"from": run.state, "to": state, "error": error}),
        )
        .await?;
        let result = decode(&row)?;
        tx.commit().await?;
        Ok(result)
    }
}

async fn audit(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: Uuid,
    team: &str,
    actor: &str,
    action: &str,
    detail: serde_json::Value,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO thalamus_run_audit (run_id,team,actor,action,detail) VALUES ($1,$2,$3,$4,$5)",
    )
    .bind(id)
    .bind(team)
    .bind(actor)
    .bind(action)
    .bind(detail)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ingestion_failure_is_retryable_but_terminal_states_are_not() {
        assert_eq!(
            Transition::IngestionFailed {
                error: "unavailable".into()
            }
            .next("ingesting")
            .unwrap()
            .0,
            "ingestion_failed"
        );
        assert_eq!(
            Transition::RetryIngestion
                .next("ingestion_failed")
                .unwrap()
                .0,
            "ingesting"
        );
        assert!(Transition::Started.next("succeeded").is_err());
        assert!(Transition::JobSucceeded.next("pending").is_err());
    }
}
