//! Worker-only authorization of rollout-created validation runs. The route is
//! derived from durable identities, never from an untrusted API request.
use super::*;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuthorizedValidation {
    pub change_id: Uuid,
    pub owner_team: Uuid,
    pub project_id: Uuid,
    pub service: String,
    pub deployment: String,
    pub revision: String,
    pub catalog_commit: String,
    pub route: Option<String>,
    pub old_deployment: Option<String>,
    pub stage: ValidationStage,
    pub run_id: Option<Uuid>,
    /// False for a previously bound run after the rollout has advanced.
    pub active: bool,
}
fn stage_name(stage: ValidationStage) -> &'static str {
    match stage {
        ValidationStage::Candidate => "candidate",
        ValidationStage::Public => "public",
    }
}
fn active_stage(stage: ValidationStage) -> &'static str {
    match stage {
        ValidationStage::Candidate => "candidate_validation",
        ValidationStage::Public => "public_validation",
    }
}
fn run_key(change: Uuid, stage: ValidationStage, commit: &str) -> String {
    format!("rollout:{change}:{}:{commit}", stage_name(stage))
}
fn valid_deployment(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 40
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && name
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && name
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
}

impl Store {
    async fn authorize_validation_in_transaction(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        requirement: &ValidationRequirement,
        project_id: Uuid,
    ) -> Result<AuthorizedValidation> {
        if !valid_deployment(&requirement.deployment) || project_id.is_nil() {
            return Err(Error::Invalid("invalid validation identity"));
        }
        let row = sqlx::query("SELECT r.*,c.actor,c.request FROM thalamus_rollouts r JOIN thalamus_changes c ON c.id=r.change_id JOIN identity_projects p ON p.id=$2 AND p.team_id=r.owner_team WHERE r.change_id=$1 FOR UPDATE OF r")
            .bind(requirement.change_id).bind(project_id).fetch_optional(&mut **tx).await?
            .ok_or(Error::Invalid("rollout project is not authorized"))?;
        let actor: String = row.try_get("actor")?;
        let proposal: Proposal = serde_json::from_value(row.try_get("request")?)?;
        let owner: Option<Uuid> = row.try_get("owner_team")?;
        let deployment: Option<String> = row.try_get("candidate_deployment")?;
        let revision: Option<String> = row.try_get("candidate_revision")?;
        let state: String = row.try_get("stage")?;
        if state != active_stage(requirement.stage)
            || owner != Some(requirement.owner_team)
            || actor != requirement.actor
            || proposal.service != requirement.service
            || deployment.as_deref() != Some(requirement.deployment.as_str())
            || revision.as_deref() != Some(requirement.revision.as_str())
        {
            return Err(Error::Invalid("rollout validation identity is not current"));
        }
        let source = match (&proposal.rollout, requirement.stage) {
            (Rollout::Gated, ValidationStage::Candidate) => "candidate_pr",
            (Rollout::Gated, ValidationStage::Public) => "promotion_pr",
            (Rollout::Rolling, ValidationStage::Public) => "change_pr",
            (Rollout::InPlace { .. }, ValidationStage::Public) => "scheduled_pr",
            _ => return Err(Error::Invalid("invalid rollout validation stage")),
        };
        let commit: Option<String> = sqlx::query_scalar(
            "SELECT merged_commit FROM thalamus_rollout_stages WHERE change_id=$1 AND stage=$2",
        )
        .bind(requirement.change_id)
        .bind(source)
        .fetch_optional(&mut **tx)
        .await?
        .flatten();
        if commit.as_deref() != Some(requirement.catalog_commit.as_str()) {
            return Err(Error::Invalid("rollout validation commit changed"));
        }
        if matches!(
            (&proposal.rollout, requirement.stage),
            (Rollout::Gated, ValidationStage::Public)
        ) {
            let candidate: Option<Uuid> = row.try_get("candidate_validation_run")?;
            let candidate_commit: Option<String> = sqlx::query_scalar("SELECT merged_commit FROM thalamus_rollout_stages WHERE change_id=$1 AND stage='candidate_pr'")
                .bind(requirement.change_id).fetch_optional(&mut **tx).await?.flatten();
            if candidate.is_none()
                || candidate != requirement.candidate_run_id
                || candidate_commit != requirement.candidate_catalog_commit
            {
                return Err(Error::Invalid("candidate validation anchor changed"));
            }
        } else if requirement.candidate_run_id.is_some()
            || requirement.candidate_catalog_commit.is_some()
        {
            return Err(Error::Invalid("unexpected candidate validation anchor"));
        }
        Ok(AuthorizedValidation {
            change_id: requirement.change_id,
            owner_team: requirement.owner_team,
            project_id,
            service: requirement.service.clone(),
            deployment: requirement.deployment.clone(),
            revision: requirement.revision.clone(),
            catalog_commit: requirement.catalog_commit.clone(),
            route: (requirement.stage == ValidationStage::Candidate)
                .then(|| format!("{}-candidate", requirement.deployment)),
            old_deployment: row.try_get("old_deployment")?,
            stage: requirement.stage,
            run_id: None,
            active: true,
        })
    }
    /// Trusted workflow gate calls this before observing a dynamically named route.
    pub async fn authorize_validation(
        &self,
        requirement: &ValidationRequirement,
        project_id: Uuid,
    ) -> Result<AuthorizedValidation> {
        let mut tx = self.pool.begin().await?;
        let value = self
            .authorize_validation_in_transaction(&mut tx, requirement, project_id)
            .await?;
        tx.commit().await?;
        Ok(value)
    }
    /// Bind a run in the same transaction that created it, before any worker can
    /// see the run. The immutable binding survives rollout advancement.
    pub async fn bind_validation_run_in_transaction(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        requirement: &ValidationRequirement,
        project_id: Uuid,
        run_id: Uuid,
    ) -> Result<AuthorizedValidation> {
        let mut value = self
            .authorize_validation_in_transaction(tx, requirement, project_id)
            .await?;
        let row = sqlx::query("SELECT team,actor,idempotency_key,request,state FROM thalamus_runs WHERE id=$1 FOR SHARE")
            .bind(run_id).fetch_optional(&mut **tx).await?.ok_or(Error::Invalid("validation run not found"))?;
        let request: serde_json::Value = row.try_get("request")?;
        let team: String = row.try_get("team")?;
        let actor: String = row.try_get("actor")?;
        let key: String = row.try_get("idempotency_key")?;
        let state: String = row.try_get("state")?;
        if team != requirement.owner_team.to_string()
            || actor != requirement.actor
            || key
                != run_key(
                    requirement.change_id,
                    requirement.stage,
                    &requirement.catalog_commit,
                )
            || request["project_id"].as_str() != Some(project_id.to_string().as_str())
            || request["kind"].as_str() != Some("validation")
            || request["service"].as_str() != Some(requirement.service.as_str())
            || request["deployment"].as_str() != Some(requirement.deployment.as_str())
            || !matches!(
                state.as_str(),
                "pending" | "running" | "ingesting" | "succeeded" | "failed" | "ingestion_failed"
            )
        {
            return Err(Error::Invalid("validation run identity mismatch"));
        }
        sqlx::query("INSERT INTO thalamus_rollout_runs(run_id,change_id,stage,owner_team,project_id,actor,service,deployment,revision,catalog_commit,old_deployment) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) ON CONFLICT(run_id) DO NOTHING")
            .bind(run_id).bind(value.change_id).bind(stage_name(value.stage)).bind(value.owner_team).bind(value.project_id)
            .bind(&requirement.actor).bind(&value.service).bind(&value.deployment).bind(&value.revision).bind(&value.catalog_commit)
            .bind(&value.old_deployment).execute(&mut **tx).await?;
        let matches: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM thalamus_rollout_runs WHERE run_id=$1 AND change_id=$2 AND stage=$3 AND owner_team=$4 AND project_id=$5 AND actor=$6 AND service=$7 AND deployment=$8 AND revision=$9 AND catalog_commit=$10)")
            .bind(run_id).bind(value.change_id).bind(stage_name(value.stage)).bind(value.owner_team).bind(value.project_id)
            .bind(&requirement.actor).bind(&value.service).bind(&value.deployment).bind(&value.revision).bind(&value.catalog_commit)
            .fetch_one(&mut **tx).await?;
        if !matches {
            return Err(Error::Invalid("validation run already bound elsewhere"));
        }
        let sql = match value.stage {
            ValidationStage::Candidate => "UPDATE thalamus_rollouts SET candidate_validation_run=$2 WHERE change_id=$1 AND (candidate_validation_run IS NULL OR candidate_validation_run=$2)",
            ValidationStage::Public => "UPDATE thalamus_rollouts SET public_validation_run=$2 WHERE change_id=$1 AND (public_validation_run IS NULL OR public_validation_run=$2)",
        };
        if sqlx::query(sql)
            .bind(value.change_id)
            .bind(run_id)
            .execute(&mut **tx)
            .await?
            .rows_affected()
            != 1
        {
            return Err(Error::Invalid("another validation run is already bound"));
        }
        value.run_id = Some(run_id);
        Ok(value)
    }
    /// Worker-only: returns historical immutable binding for an existing run.
    /// Callers must require `active` before preparing or creating a new Job;
    /// historical bindings may only be used to follow/ingest existing Jobs.
    pub async fn authorize_validation_run(
        &self,
        run_id: Uuid,
    ) -> Result<Option<AuthorizedValidation>> {
        let row = sqlx::query("SELECT b.*,r.stage AS current_stage,r.owner_team AS current_team,r.candidate_deployment AS current_deployment,r.candidate_revision AS current_revision,s.merged_commit AS current_commit,x.team AS run_team,x.actor AS run_actor,x.idempotency_key,x.request AS run_request FROM thalamus_rollout_runs b JOIN thalamus_rollouts r ON r.change_id=b.change_id JOIN thalamus_changes c ON c.id=b.change_id JOIN thalamus_runs x ON x.id=b.run_id LEFT JOIN thalamus_rollout_stages s ON s.change_id=b.change_id AND s.stage=CASE WHEN b.stage='candidate' THEN 'candidate_pr' WHEN c.request->'rollout'->>'mode'='gated' THEN 'promotion_pr' WHEN c.request->'rollout'->>'mode'='in_place' THEN 'scheduled_pr' ELSE 'change_pr' END WHERE b.run_id=$1")
            .bind(run_id).fetch_optional(&self.pool).await?;
        let Some(row) = row else { return Ok(None) };
        let stage = match row.try_get::<String, _>("stage")?.as_str() {
            "candidate" => ValidationStage::Candidate,
            "public" => ValidationStage::Public,
            _ => return Err(Error::Invalid("invalid validation stage")),
        };
        let change_id: Uuid = row.try_get("change_id")?;
        let owner_team: Uuid = row.try_get("owner_team")?;
        let project_id: Uuid = row.try_get("project_id")?;
        let actor: String = row.try_get("actor")?;
        let service: String = row.try_get("service")?;
        let deployment: String = row.try_get("deployment")?;
        let revision: String = row.try_get("revision")?;
        let commit: String = row.try_get("catalog_commit")?;
        let request: serde_json::Value = row.try_get("run_request")?;
        if !valid_deployment(&deployment)
            || row.try_get::<String, _>("run_team")? != owner_team.to_string()
            || row.try_get::<String, _>("run_actor")? != actor
            || row.try_get::<String, _>("idempotency_key")? != run_key(change_id, stage, &commit)
            || request["project_id"].as_str() != Some(project_id.to_string().as_str())
            || request["kind"].as_str() != Some("validation")
            || request["service"].as_str() != Some(service.as_str())
            || request["deployment"].as_str() != Some(deployment.as_str())
        {
            return Err(Error::Invalid("bound validation run identity changed"));
        }
        let active = row.try_get::<String, _>("current_stage")? == active_stage(stage)
            && row.try_get::<Option<Uuid>, _>("current_team")? == Some(owner_team)
            && row
                .try_get::<Option<String>, _>("current_deployment")?
                .as_deref()
                == Some(deployment.as_str())
            && row
                .try_get::<Option<String>, _>("current_revision")?
                .as_deref()
                == Some(revision.as_str())
            && row
                .try_get::<Option<String>, _>("current_commit")?
                .as_deref()
                == Some(commit.as_str());
        Ok(Some(AuthorizedValidation {
            change_id,
            owner_team,
            project_id,
            service,
            revision,
            catalog_commit: commit,
            route: (stage == ValidationStage::Candidate).then(|| format!("{deployment}-candidate")),
            deployment,
            old_deployment: row.try_get("old_deployment")?,
            stage,
            run_id: Some(run_id),
            active,
        }))
    }
}

/// Active generated candidate, authorized for this exact service/team/project.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizedCandidate {
    pub change_id: Uuid,
    pub owner_team: Uuid,
    pub project_id: Uuid,
    pub service: String,
    pub deployment: String,
    pub revision: String,
    pub catalog_commit: String,
    pub old_deployment: Option<String>,
}
/// Immutable admission binding. `active` is required for a new Job; false
/// bindings remain available for following and ingesting an already-created Job.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuthorizedCandidateRun {
    pub run_id: Uuid,
    pub change_id: Uuid,
    pub owner_team: Uuid,
    pub project_id: Uuid,
    pub service: String,
    pub deployment: String,
    pub revision: String,
    pub catalog_commit: String,
    pub old_deployment: Option<String>,
    pub route: String,
    pub active: bool,
}

impl Store {
    pub async fn authorize_active_candidate_in_transaction(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        service: &str,
        deployment: &str,
        owner_team: Uuid,
        project_id: Uuid,
    ) -> Result<Option<AuthorizedCandidate>> {
        if !valid_deployment(deployment) || owner_team.is_nil() || project_id.is_nil() {
            return Err(Error::Invalid("invalid candidate identity"));
        }
        let rows = sqlx::query("SELECT r.change_id,r.owner_team,r.candidate_deployment,r.candidate_revision,r.old_deployment,s.merged_commit FROM thalamus_rollouts r JOIN thalamus_changes c ON c.id=r.change_id JOIN identity_projects p ON p.id=$4 AND p.team_id=r.owner_team JOIN thalamus_rollout_stages s ON s.change_id=r.change_id AND s.stage='candidate_pr' WHERE r.stage IN ('candidate_validation','promotion_pr') AND r.owner_team=$3 AND c.request->>'service'=$1 AND r.candidate_deployment=$2 AND s.merged_commit IS NOT NULL FOR UPDATE OF r")
            .bind(service).bind(deployment).bind(owner_team).bind(project_id).fetch_all(&mut **tx).await?;
        if rows.len() > 1 {
            return Err(Error::Conflict("candidate identity is ambiguous"));
        }
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let revision: Option<String> = row.try_get("candidate_revision")?;
        let commit: Option<String> = row.try_get("merged_commit")?;
        let revision = revision.ok_or(Error::Invalid("candidate revision missing"))?;
        let catalog_commit = commit.ok_or(Error::Invalid("candidate merged revision missing"))?;
        Ok(Some(AuthorizedCandidate {
            change_id: row.try_get("change_id")?,
            owner_team,
            project_id,
            service: service.to_owned(),
            deployment: deployment.to_owned(),
            revision,
            catalog_commit,
            old_deployment: row.try_get("old_deployment")?,
        }))
    }
    pub async fn authorize_active_candidate(
        &self,
        service: &str,
        deployment: &str,
        owner_team: Uuid,
        project_id: Uuid,
    ) -> Result<Option<AuthorizedCandidate>> {
        let mut tx = self.pool.begin().await?;
        let candidate = self
            .authorize_active_candidate_in_transaction(
                &mut tx, service, deployment, owner_team, project_id,
            )
            .await?;
        tx.commit().await?;
        Ok(candidate)
    }
    /// The run insert and this binding must commit in the same authorized
    /// transaction; every candidate identity is rechecked under a row lock.
    pub async fn bind_candidate_run_in_transaction(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        run_id: Uuid,
        candidate: &AuthorizedCandidate,
    ) -> Result<AuthorizedCandidateRun> {
        let current = self
            .authorize_active_candidate_in_transaction(
                tx,
                &candidate.service,
                &candidate.deployment,
                candidate.owner_team,
                candidate.project_id,
            )
            .await?
            .ok_or(Error::Invalid("candidate rollout is no longer active"))?;
        if &current != candidate {
            return Err(Error::Conflict("candidate identity changed"));
        }
        let row = sqlx::query("SELECT team,request FROM thalamus_runs WHERE id=$1 FOR SHARE")
            .bind(run_id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(Error::Invalid("candidate run not found"))?;
        let team: String = row.try_get("team")?;
        let request: serde_json::Value = row.try_get("request")?;
        let kind = request["kind"]
            .as_str()
            .ok_or(Error::Invalid("candidate run kind missing"))?;
        if team != candidate.owner_team.to_string()
            || request["project_id"].as_str() != Some(candidate.project_id.to_string().as_str())
            || request["service"].as_str() != Some(candidate.service.as_str())
            || request["deployment"].as_str() != Some(candidate.deployment.as_str())
            || !matches!(kind, "validation" | "benchmark")
        {
            return Err(Error::Invalid("candidate run identity mismatch"));
        }
        sqlx::query("INSERT INTO thalamus_candidate_runs(run_id,change_id,owner_team,project_id,service,deployment,revision,catalog_commit,old_deployment,kind) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) ON CONFLICT(run_id) DO NOTHING")
            .bind(run_id).bind(candidate.change_id).bind(candidate.owner_team).bind(candidate.project_id)
            .bind(&candidate.service).bind(&candidate.deployment).bind(&candidate.revision).bind(&candidate.catalog_commit)
            .bind(&candidate.old_deployment).bind(kind).execute(&mut **tx).await?;
        let matches: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM thalamus_candidate_runs WHERE run_id=$1 AND change_id=$2 AND owner_team=$3 AND project_id=$4 AND service=$5 AND deployment=$6 AND revision=$7 AND catalog_commit=$8 AND kind=$9 AND old_deployment IS NOT DISTINCT FROM $10)")
            .bind(run_id).bind(candidate.change_id).bind(candidate.owner_team).bind(candidate.project_id)
            .bind(&candidate.service).bind(&candidate.deployment).bind(&candidate.revision).bind(&candidate.catalog_commit)
            .bind(kind).bind(&candidate.old_deployment).fetch_one(&mut **tx).await?;
        if !matches {
            return Err(Error::Conflict("candidate run already bound elsewhere"));
        }
        Ok(AuthorizedCandidateRun {
            run_id,
            change_id: candidate.change_id,
            owner_team: candidate.owner_team,
            project_id: candidate.project_id,
            service: candidate.service.clone(),
            deployment: candidate.deployment.clone(),
            revision: candidate.revision.clone(),
            catalog_commit: candidate.catalog_commit.clone(),
            old_deployment: candidate.old_deployment.clone(),
            route: format!(
                "{}-{}",
                candidate.deployment,
                if kind == "benchmark" {
                    "benchmark"
                } else {
                    "candidate"
                }
            ),
            active: true,
        })
    }
    pub async fn authorize_candidate_run(
        &self,
        run_id: Uuid,
    ) -> Result<Option<AuthorizedCandidateRun>> {
        let row = sqlx::query("SELECT b.*,r.stage AS current_stage,r.owner_team AS current_team,r.candidate_deployment AS current_deployment,r.candidate_revision AS current_revision,s.merged_commit AS current_commit,x.team AS run_team,x.request AS run_request FROM thalamus_candidate_runs b JOIN thalamus_rollouts r ON r.change_id=b.change_id JOIN thalamus_runs x ON x.id=b.run_id LEFT JOIN thalamus_rollout_stages s ON s.change_id=b.change_id AND s.stage='candidate_pr' WHERE b.run_id=$1")
            .bind(run_id).fetch_optional(&self.pool).await?;
        let Some(row) = row else { return Ok(None) };
        let change_id: Uuid = row.try_get("change_id")?;
        let owner_team: Uuid = row.try_get("owner_team")?;
        let project_id: Uuid = row.try_get("project_id")?;
        let service: String = row.try_get("service")?;
        let deployment: String = row.try_get("deployment")?;
        let revision: String = row.try_get("revision")?;
        let catalog_commit: String = row.try_get("catalog_commit")?;
        let kind: String = row.try_get("kind")?;
        let request: serde_json::Value = row.try_get("run_request")?;
        if !valid_deployment(&deployment)
            || !matches!(kind.as_str(), "validation" | "benchmark")
            || row.try_get::<String, _>("run_team")? != owner_team.to_string()
            || request["project_id"].as_str() != Some(project_id.to_string().as_str())
            || request["kind"].as_str() != Some(kind.as_str())
            || request["service"].as_str() != Some(service.as_str())
            || request["deployment"].as_str() != Some(deployment.as_str())
        {
            return Err(Error::Invalid("bound candidate run identity changed"));
        }
        let stage: String = row.try_get("current_stage")?;
        let active = matches!(stage.as_str(), "candidate_validation" | "promotion_pr")
            && row.try_get::<Option<Uuid>, _>("current_team")? == Some(owner_team)
            && row
                .try_get::<Option<String>, _>("current_deployment")?
                .as_deref()
                == Some(deployment.as_str())
            && row
                .try_get::<Option<String>, _>("current_revision")?
                .as_deref()
                == Some(revision.as_str())
            && row
                .try_get::<Option<String>, _>("current_commit")?
                .as_deref()
                == Some(catalog_commit.as_str());
        Ok(Some(AuthorizedCandidateRun {
            run_id,
            change_id,
            owner_team,
            project_id,
            service,
            route: format!(
                "{}-{}",
                deployment,
                if kind == "benchmark" {
                    "benchmark"
                } else {
                    "candidate"
                }
            ),
            deployment,
            revision,
            catalog_commit,
            old_deployment: row.try_get("old_deployment")?,
            active,
        }))
    }
}
