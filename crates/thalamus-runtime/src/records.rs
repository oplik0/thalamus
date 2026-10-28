//! Append-only evidence for a run, collected independently of the run request.
//!
//! The worker must persist a live prelaunch observation before creating or
//! adopting a Job. On recovery, an already-existing Job without prelaunch
//! evidence must not be adopted. An ingestion observation and an independently
//! observed owned runner Pod are required before accepting successful results.
//! Neither timestamps nor the request's asserted evidence are observations.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::{RouteSnapshot, RunnerObservation};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid run evidence: {0}")]
    Invalid(String),
    #[error("run evidence conflicts with the immutable prelaunch record: {0}")]
    Conflict(String),
    #[error("required run evidence is missing: {0}")]
    Missing(String),
    #[error("run evidence was permanently rejected: {0}")]
    Rejected(String),
    #[error("run evidence or runner identity was replayed")]
    Replay,
    #[error("run not found")]
    NotFound,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrelaunchEvidence {
    pub run_id: Uuid,
    pub request_fingerprint: String,
    pub snapshot: RouteSnapshot,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifiedEvidence {
    pub run_id: Uuid,
    pub request_fingerprint: String,
    pub prelaunch: RouteSnapshot,
    pub ingestion: RouteSnapshot,
    pub runner: RunnerObservation,
}

/// Independently bound failed-Job diagnostics, never a successful validation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureEvidence {
    pub run_id: Uuid,
    pub request_fingerprint: String,
    pub prelaunch: RouteSnapshot,
    pub ingestion: RouteSnapshot,
    pub runner: RunnerObservation,
}

impl From<VerifiedEvidence> for FailureEvidence {
    fn from(evidence: VerifiedEvidence) -> Self {
        Self {
            run_id: evidence.run_id,
            request_fingerprint: evidence.request_fingerprint,
            prelaunch: evidence.prelaunch,
            ingestion: evidence.ingestion,
            runner: evidence.runner,
        }
    }
}

#[derive(Clone)]
pub struct Records {
    pool: PgPool,
}

impl Records {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The thalamus-runs migrations must already have been applied.
    pub async fn migrate(&self) -> Result<()> {
        sqlx::raw_sql(include_str!("../migrations/0001_run_evidence.sql"))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Persist the live observation before Job CREATE, or return the original
    /// observation for a retry with the same identity. A timestamp-only change
    /// is not a changed identity and does not overwrite the original timestamp.
    pub async fn prelaunch(
        &self,
        run_id: Uuid,
        fingerprint: &str,
        snapshot: &RouteSnapshot,
    ) -> Result<PrelaunchEvidence> {
        let mut tx = self.pool.begin().await?;
        let binding = lock_run(&mut tx, run_id).await?;
        binding.check_fingerprint(fingerprint)?;
        validate_snapshot_for_run(snapshot, &binding)?;

        if let Some(existing) = load_prelaunch(&mut tx, run_id).await? {
            validate_prelaunch(&existing, &binding)?;
            if !existing.snapshot.same_identity(snapshot) {
                return Err(Error::Conflict(
                    "route identity changed before launch".into(),
                ));
            }
            tx.commit().await?;
            return Ok(existing);
        }
        if binding.state != "pending" || binding.job_uid.is_some() {
            return Err(Error::Missing(
                "prelaunch observation was not recorded before the Job".into(),
            ));
        }
        sqlx::query("INSERT INTO thalamus_run_prelaunch_evidence (run_id,request_fingerprint,snapshot) VALUES ($1,$2,$3)")
            .bind(run_id)
            .bind(fingerprint)
            .bind(serde_json::to_value(snapshot)?)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(PrelaunchEvidence {
            run_id,
            request_fingerprint: fingerprint.to_owned(),
            snapshot: snapshot.clone(),
        })
    }

    /// Fetch the original prelaunch record for recovery; this does not by
    /// itself prove that a run completed or that its results were verified.
    pub async fn get_prelaunch(&self, run_id: Uuid) -> Result<Option<PrelaunchEvidence>> {
        let row = sqlx::query(
            "SELECT r.fingerprint,r.request,r.state,r.job_name,r.job_uid,r.job_spec, \
             p.request_fingerprint AS pre_fingerprint,p.snapshot AS pre_snapshot \
             FROM thalamus_runs r JOIN thalamus_run_prelaunch_evidence p ON p.run_id=r.id WHERE r.id=$1",
        )
        .bind(run_id)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else { return Ok(None) };
        let binding = RunBinding::decode(&row)?;
        let evidence = PrelaunchEvidence {
            run_id,
            request_fingerprint: row.try_get("pre_fingerprint")?,
            snapshot: serde_json::from_value(row.try_get("pre_snapshot")?)?,
        };
        validate_prelaunch(&evidence, &binding)?;
        Ok(Some(evidence))
    }

    /// Permanently reject a run after an independently observed binding
    /// mismatch outside this module. Supply the observations available at the
    /// failure; do not synthesize observations that could not be collected.
    /// Transient API or database errors alone are not evidence of a mismatch.
    pub async fn reject_ingestion(
        &self,
        run_id: Uuid,
        fingerprint: &str,
        reason: &str,
        snapshot: Option<&RouteSnapshot>,
        runner: Option<&RunnerObservation>,
    ) -> Result<()> {
        self.reject_for_state(run_id, fingerprint, reason, snapshot, runner, "ingesting")
            .await
    }

    /// Permanently reject failed-Job diagnostics after the worker independently
    /// established the Job failure and observed a binding mismatch.
    pub async fn reject_failure(
        &self,
        run_id: Uuid,
        fingerprint: &str,
        reason: &str,
        snapshot: Option<&RouteSnapshot>,
        runner: Option<&RunnerObservation>,
    ) -> Result<()> {
        self.reject_for_state(run_id, fingerprint, reason, snapshot, runner, "running")
            .await
    }

    async fn reject_for_state(
        &self,
        run_id: Uuid,
        fingerprint: &str,
        reason: &str,
        snapshot: Option<&RouteSnapshot>,
        runner: Option<&RunnerObservation>,
        expected_state: &str,
    ) -> Result<()> {
        if !valid_text(reason) || reason.len() > 4096 {
            return Err(Error::Invalid("invalid rejection reason".into()));
        }
        let mut tx = self.pool.begin().await?;
        let binding = lock_run(&mut tx, run_id).await?;
        binding.check_fingerprint(fingerprint)?;
        let start = load_prelaunch(&mut tx, run_id)
            .await?
            .ok_or_else(|| Error::Missing("prelaunch observation".into()))?;
        validate_prelaunch(&start, &binding)?;
        if load_ingestion(&mut tx, run_id, &start).await?.is_some() {
            return Err(Error::Replay);
        }
        if let Some(old) = load_rejection(&mut tx, run_id).await? {
            validate_rejection_retry(&old, reason, snapshot, runner)?;
            tx.commit().await?;
            return Ok(());
        }
        if binding.state != expected_state {
            return Err(Error::Invalid(
                "rejection is not valid in this run state".into(),
            ));
        }
        insert_rejection(&mut tx, run_id, fingerprint, reason, snapshot, runner).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Verify and persist an independent post-execution observation. The run
    /// must already be in the ingesting state with its Job UID recorded.
    /// Previously observed mismatches are permanent; an exact retry returns the
    /// original historical evidence and cannot change either snapshot.
    pub async fn ingestion(
        &self,
        run_id: Uuid,
        fingerprint: &str,
        snapshot: &RouteSnapshot,
        runner: &RunnerObservation,
    ) -> Result<VerifiedEvidence> {
        self.record(
            run_id,
            fingerprint,
            snapshot,
            runner,
            "succeeded",
            "ingesting",
        )
        .await
    }

    /// Bind a failed Job's diagnostic result. The worker must first establish
    /// the actual Job failure; this evidence never makes a validation pass.
    pub async fn failure(
        &self,
        run_id: Uuid,
        fingerprint: &str,
        snapshot: &RouteSnapshot,
        runner: &RunnerObservation,
    ) -> Result<FailureEvidence> {
        self.record(run_id, fingerprint, snapshot, runner, "failed", "running")
            .await
            .map(FailureEvidence::from)
    }

    async fn record(
        &self,
        run_id: Uuid,
        fingerprint: &str,
        snapshot: &RouteSnapshot,
        runner: &RunnerObservation,
        outcome: &str,
        expected_state: &str,
    ) -> Result<VerifiedEvidence> {
        let mut tx = self.pool.begin().await?;
        let binding = lock_run(&mut tx, run_id).await?;
        binding.check_fingerprint(fingerprint)?;
        let start = load_prelaunch(&mut tx, run_id)
            .await?
            .ok_or_else(|| Error::Missing("prelaunch observation".into()))?;
        validate_prelaunch(&start, &binding)?;

        if let Some((existing_outcome, existing)) = load_ingestion(&mut tx, run_id, &start).await? {
            validate_verified(&existing, &binding)?;
            validate_retry(&existing_outcome, outcome, &existing, snapshot, runner)?;
            tx.commit().await?;
            return Ok(existing);
        }
        if let Some(rejection) = load_rejection(&mut tx, run_id).await? {
            return Err(Error::Rejected(rejection.reason));
        }
        if binding.state != expected_state {
            return Err(Error::Invalid(
                "verification is not valid in this run state".into(),
            ));
        }

        let verification = validate_snapshot_for_run(snapshot, &binding)
            .and_then(|()| validate_against_run(&start.snapshot, snapshot, runner, &binding));
        if let Err(error) = verification {
            let reason = error.to_string();
            insert_rejection(
                &mut tx,
                run_id,
                fingerprint,
                &reason,
                Some(snapshot),
                Some(runner),
            )
            .await?;
            tx.commit().await?;
            return Err(Error::Rejected(reason));
        }
        let inserted = sqlx::query(
            "INSERT INTO thalamus_run_ingestion_evidence \
             (run_id,request_fingerprint,outcome,snapshot,job_uid,pod_uid,runner) VALUES ($1,$2,$3,$4,$5,$6,$7) \
             ON CONFLICT DO NOTHING RETURNING run_id",
        )
        .bind(run_id)
        .bind(fingerprint)
        .bind(outcome)
        .bind(serde_json::to_value(snapshot)?)
        .bind(&runner.job_uid)
        .bind(&runner.pod_uid)
        .bind(serde_json::to_value(runner)?)
        .fetch_optional(&mut *tx)
        .await?;
        if inserted.is_none() {
            insert_rejection(
                &mut tx,
                run_id,
                fingerprint,
                "runner Job or Pod UID was already used by another run",
                Some(snapshot),
                Some(runner),
            )
            .await?;
            tx.commit().await?;
            return Err(Error::Replay);
        }
        tx.commit().await?;
        Ok(VerifiedEvidence {
            run_id,
            request_fingerprint: fingerprint.to_owned(),
            prelaunch: start.snapshot,
            ingestion: snapshot.clone(),
            runner: runner.clone(),
        })
    }

    /// Fetch verified evidence for an exact run. Callers must select the latest
    /// relevant run first and must not fall back to an older verified run.
    pub async fn get_verified(&self, run_id: Uuid) -> Result<Option<VerifiedEvidence>> {
        self.get_for_outcome(run_id, "succeeded").await
    }

    /// Read through the caller's authorization/selection transaction, without
    /// acquiring another connection. The caller owns the transaction and locks.
    pub async fn get_verified_in_transaction(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        run_id: Uuid,
    ) -> Result<Option<VerifiedEvidence>> {
        let row = sqlx::query(EVIDENCE_QUERY)
            .bind(run_id)
            .bind("succeeded")
            .fetch_optional(&mut **tx)
            .await?;
        row.map(|row| decode_record(&row, run_id)).transpose()
    }

    /// Read bound diagnostics for a failed Job without treating them as a pass.
    pub async fn get_failure(&self, run_id: Uuid) -> Result<Option<FailureEvidence>> {
        Ok(self
            .get_for_outcome(run_id, "failed")
            .await?
            .map(FailureEvidence::from))
    }

    /// Read failed-Job diagnostics in the caller's authorization transaction.
    pub async fn get_failure_in_transaction(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        run_id: Uuid,
    ) -> Result<Option<FailureEvidence>> {
        let row = sqlx::query(EVIDENCE_QUERY)
            .bind(run_id)
            .bind("failed")
            .fetch_optional(&mut **tx)
            .await?;
        Ok(row
            .map(|row| decode_record(&row, run_id))
            .transpose()?
            .map(FailureEvidence::from))
    }

    /// Trusted worker recovery of an already recorded successful-Job
    /// observation, including before artifact persistence finishes. This is a
    /// historical record, not a new observation or proof of current routing.
    pub async fn get_ingesting(&self, run_id: Uuid) -> Result<Option<VerifiedEvidence>> {
        self.get_for_recovery(
            run_id,
            "succeeded",
            &["ingesting", "ingestion_failed", "succeeded"],
        )
        .await
    }

    /// Trusted worker recovery of an already recorded failed-Job observation.
    pub async fn get_failed_job(&self, run_id: Uuid) -> Result<Option<FailureEvidence>> {
        Ok(self
            .get_for_recovery(run_id, "failed", &["running", "failed"])
            .await?
            .map(FailureEvidence::from))
    }

    async fn get_for_recovery(
        &self,
        run_id: Uuid,
        outcome: &str,
        allowed_states: &[&str],
    ) -> Result<Option<VerifiedEvidence>> {
        let row = sqlx::query(RECOVERY_QUERY)
            .bind(run_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(Error::NotFound)?;
        if let Some(reason) = row.try_get::<Option<String>, _>("rejection_reason")? {
            return Err(Error::Rejected(reason));
        }
        let Some(stored_outcome) = row.try_get::<Option<String>, _>("evidence_outcome")? else {
            return Ok(None);
        };
        if stored_outcome != outcome {
            return Err(Error::Replay);
        }
        let state: String = row.try_get("state")?;
        if !allowed_states.contains(&state.as_str()) {
            return Err(Error::Invalid(
                "stored evidence is incompatible with the run state".into(),
            ));
        }
        Ok(Some(decode_record(&row, run_id)?))
    }

    async fn get_for_outcome(
        &self,
        run_id: Uuid,
        outcome: &str,
    ) -> Result<Option<VerifiedEvidence>> {
        let row = sqlx::query(EVIDENCE_QUERY)
            .bind(run_id)
            .bind(outcome)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| decode_record(&row, run_id)).transpose()
    }
}

const EVIDENCE_QUERY: &str = "SELECT r.fingerprint,r.request,r.state,r.job_name,r.job_uid,r.job_spec, \
     p.request_fingerprint AS pre_fingerprint,p.snapshot AS pre_snapshot, \
     i.request_fingerprint AS ingestion_fingerprint,i.snapshot AS ingestion_snapshot, \
     i.job_uid AS evidence_job_uid,i.pod_uid AS evidence_pod_uid,i.runner, \
     x.reason AS rejection_reason \
     FROM thalamus_runs r JOIN thalamus_run_prelaunch_evidence p ON p.run_id=r.id \
     JOIN thalamus_run_ingestion_evidence i ON i.run_id=r.id \
     LEFT JOIN thalamus_run_evidence_rejections x ON x.run_id=r.id \
     WHERE r.id=$1 AND i.outcome=$2 \
       AND ((i.outcome='succeeded' AND r.state='succeeded') OR (i.outcome='failed' AND r.state='failed'))";

const RECOVERY_QUERY: &str = "SELECT r.fingerprint,r.request,r.state,r.job_name,r.job_uid,r.job_spec, \
     p.request_fingerprint AS pre_fingerprint,p.snapshot AS pre_snapshot, \
     i.request_fingerprint AS ingestion_fingerprint,i.snapshot AS ingestion_snapshot, \
     i.job_uid AS evidence_job_uid,i.pod_uid AS evidence_pod_uid,i.runner, \
     i.outcome AS evidence_outcome,x.reason AS rejection_reason \
     FROM thalamus_runs r LEFT JOIN thalamus_run_prelaunch_evidence p ON p.run_id=r.id \
     LEFT JOIN thalamus_run_ingestion_evidence i ON i.run_id=r.id \
     LEFT JOIN thalamus_run_evidence_rejections x ON x.run_id=r.id WHERE r.id=$1";

fn decode_record(row: &sqlx::postgres::PgRow, run_id: Uuid) -> Result<VerifiedEvidence> {
    let binding = RunBinding::decode(row)?;
    if row
        .try_get::<Option<String>, _>("rejection_reason")?
        .is_some()
    {
        return Err(Error::Replay);
    }
    let start = PrelaunchEvidence {
        run_id,
        request_fingerprint: row.try_get("pre_fingerprint")?,
        snapshot: serde_json::from_value(row.try_get("pre_snapshot")?)?,
    };
    validate_prelaunch(&start, &binding)?;
    let evidence = decode_ingestion(row, run_id, &start)?;
    validate_verified(&evidence, &binding)?;
    Ok(evidence)
}

struct RunBinding {
    fingerprint: String,
    state: String,
    service: String,
    deployment: String,
    functional: bool,
    job_name: String,
    job_uid: Option<String>,
    job_spec: Option<Value>,
}

impl RunBinding {
    fn decode(row: &sqlx::postgres::PgRow) -> Result<Self> {
        let request: Value = row.try_get("request")?;
        let service = required_request_text(&request, "/service")?;
        let deployment = required_request_text(&request, "/deployment")?;
        let kind = required_request_text(&request, "/kind")?;
        let workload = required_request_text(&request, "/workload/name")?;
        let fingerprint: String = row.try_get("fingerprint")?;
        if !raw_digest(&fingerprint) {
            return Err(Error::Invalid("invalid stored request fingerprint".into()));
        }
        Ok(Self {
            fingerprint,
            state: row.try_get("state")?,
            service: service.to_owned(),
            deployment: deployment.to_owned(),
            functional: kind == "validation" && workload == "functional",
            job_name: row.try_get("job_name")?,
            job_uid: row.try_get("job_uid")?,
            job_spec: row.try_get("job_spec")?,
        })
    }

    fn check_fingerprint(&self, supplied: &str) -> Result<()> {
        if !raw_digest(supplied) || self.fingerprint != supplied {
            return Err(Error::Conflict("request fingerprint mismatch".into()));
        }
        Ok(())
    }
}

fn required_request_text<'a>(request: &'a Value, pointer: &str) -> Result<&'a str> {
    request
        .pointer(pointer)
        .and_then(Value::as_str)
        .filter(|s| valid_text(s))
        .ok_or_else(|| Error::Invalid(format!("missing immutable run request field {pointer}")))
}

async fn lock_run(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: Uuid,
) -> Result<RunBinding> {
    if run_id.is_nil() {
        return Err(Error::Invalid("nil run ID".into()));
    }
    let row = sqlx::query("SELECT fingerprint,request,state,job_name,job_uid,job_spec FROM thalamus_runs WHERE id=$1 FOR UPDATE")
        .bind(run_id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(Error::NotFound)?;
    RunBinding::decode(&row)
}

async fn load_prelaunch(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: Uuid,
) -> Result<Option<PrelaunchEvidence>> {
    let row = sqlx::query(
        "SELECT request_fingerprint,snapshot FROM thalamus_run_prelaunch_evidence WHERE run_id=$1",
    )
    .bind(run_id)
    .fetch_optional(&mut **tx)
    .await?;
    row.map(|row| {
        Ok(PrelaunchEvidence {
            run_id,
            request_fingerprint: row.try_get("request_fingerprint")?,
            snapshot: serde_json::from_value(row.try_get("snapshot")?)?,
        })
    })
    .transpose()
}

async fn load_ingestion(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: Uuid,
    start: &PrelaunchEvidence,
) -> Result<Option<(String, VerifiedEvidence)>> {
    let row = sqlx::query("SELECT outcome,request_fingerprint AS ingestion_fingerprint,snapshot AS ingestion_snapshot,job_uid AS evidence_job_uid,pod_uid AS evidence_pod_uid,runner FROM thalamus_run_ingestion_evidence WHERE run_id=$1")
        .bind(run_id).fetch_optional(&mut **tx).await?;
    row.map(|row| {
        Ok((
            row.try_get("outcome")?,
            decode_ingestion(&row, run_id, start)?,
        ))
    })
    .transpose()
}

fn decode_ingestion(
    row: &sqlx::postgres::PgRow,
    run_id: Uuid,
    start: &PrelaunchEvidence,
) -> Result<VerifiedEvidence> {
    let fingerprint: String = row.try_get("ingestion_fingerprint")?;
    let runner: RunnerObservation = serde_json::from_value(row.try_get("runner")?)?;
    if fingerprint != start.request_fingerprint
        || row.try_get::<String, _>("evidence_job_uid")? != runner.job_uid
        || row.try_get::<String, _>("evidence_pod_uid")? != runner.pod_uid
    {
        return Err(Error::Conflict(
            "stored runner or fingerprint mismatch".into(),
        ));
    }
    Ok(VerifiedEvidence {
        run_id,
        request_fingerprint: fingerprint,
        prelaunch: start.snapshot.clone(),
        ingestion: serde_json::from_value(row.try_get("ingestion_snapshot")?)?,
        runner,
    })
}

struct Rejection {
    reason: String,
    snapshot: Option<RouteSnapshot>,
    runner: Option<RunnerObservation>,
}

async fn load_rejection(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: Uuid,
) -> Result<Option<Rejection>> {
    let row = sqlx::query(
        "SELECT reason,snapshot,runner FROM thalamus_run_evidence_rejections WHERE run_id=$1",
    )
    .bind(run_id)
    .fetch_optional(&mut **tx)
    .await?;
    row.map(|row| {
        let snapshot: Option<Value> = row.try_get("snapshot")?;
        let runner: Option<Value> = row.try_get("runner")?;
        Ok(Rejection {
            reason: row.try_get("reason")?,
            snapshot: snapshot.map(serde_json::from_value).transpose()?,
            runner: runner.map(serde_json::from_value).transpose()?,
        })
    })
    .transpose()
}

async fn insert_rejection(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: Uuid,
    fingerprint: &str,
    reason: &str,
    snapshot: Option<&RouteSnapshot>,
    runner: Option<&RunnerObservation>,
) -> Result<()> {
    sqlx::query("INSERT INTO thalamus_run_evidence_rejections (run_id,request_fingerprint,reason,snapshot,runner) VALUES ($1,$2,$3,$4,$5)")
        .bind(run_id).bind(fingerprint).bind(reason)
        .bind(safe_rejection_snapshot(snapshot).map(serde_json::to_value).transpose()?)
        .bind(runner.map(serde_json::to_value).transpose()?)
        .execute(&mut **tx).await?;
    Ok(())
}

// Rejection metadata must not become a back door for persisting an unsafe raw
// engine projection. The reason and any safe runner observation are retained.
fn safe_rejection_snapshot(snapshot: Option<&RouteSnapshot>) -> Option<&RouteSnapshot> {
    snapshot.filter(|snapshot| {
        snapshot
            .deployments
            .iter()
            .all(|target| crate::observe::validate_engine_spec(&target.engine_spec).is_ok())
    })
}

fn same_optional_snapshot(a: Option<&RouteSnapshot>, b: Option<&RouteSnapshot>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.same_identity(b),
        _ => false,
    }
}

fn validate_rejection_retry(
    old: &Rejection,
    reason: &str,
    snapshot: Option<&RouteSnapshot>,
    runner: Option<&RunnerObservation>,
) -> Result<()> {
    if old.reason != reason
        || !same_optional_snapshot(old.snapshot.as_ref(), safe_rejection_snapshot(snapshot))
        || old.runner.as_ref() != runner
    {
        return Err(Error::Replay);
    }
    Ok(())
}

fn validate_retry(
    existing_outcome: &str,
    requested_outcome: &str,
    existing: &VerifiedEvidence,
    snapshot: &RouteSnapshot,
    runner: &RunnerObservation,
) -> Result<()> {
    if existing_outcome != requested_outcome
        || !existing.ingestion.same_identity(snapshot)
        || existing.runner != *runner
    {
        return Err(Error::Replay);
    }
    Ok(())
}

fn valid_text(value: &str) -> bool {
    !value.is_empty() && value.trim() == value && !value.chars().any(char::is_control)
}

fn raw_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(raw_digest)
}

fn valid_flux_revision(value: &str) -> bool {
    let Some((reference, revision)) = value.rsplit_once('@') else {
        return false;
    };
    if !valid_text(reference) {
        return false;
    }
    revision.strip_prefix("sha1:").is_some_and(|hash| {
        hash.len() == 40
            && hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }) || digest(revision)
}

fn valid_runtime(runtime: &thalamus_core::RuntimeIdentity) -> bool {
    digest(&runtime.model_content)
        && digest(&runtime.tokenizer_content)
        && digest(&runtime.effective_configuration)
        && !runtime.image_digests.is_empty()
        && !runtime.component_versions.is_empty()
        && runtime
            .image_digests
            .iter()
            .all(|(component, value)| valid_text(component) && digest(value))
        && runtime
            .component_versions
            .iter()
            .all(|(component, value)| valid_text(component) && valid_text(value))
}

fn validate_snapshot(snapshot: &RouteSnapshot) -> Result<()> {
    if !valid_text(&snapshot.service)
        || !valid_text(&snapshot.route)
        || snapshot.hostnames.is_empty()
        || snapshot.paths.is_empty()
        || snapshot.deployments.is_empty()
        || snapshot.ports.is_empty()
        || snapshot.listeners.is_empty()
    {
        return Err(Error::Missing(
            "route, hostname, path, listener or deployment".into(),
        ));
    }
    let mut names = BTreeSet::new();
    if snapshot
        .hostnames
        .iter()
        .any(|v| !valid_text(v) || !names.insert(v))
    {
        return Err(Error::Invalid("invalid or duplicate route hostname".into()));
    }
    names.clear();
    if snapshot
        .paths
        .iter()
        .any(|v| !valid_text(v) || !v.starts_with('/') || !names.insert(v))
    {
        return Err(Error::Invalid("invalid or duplicate route path".into()));
    }
    if snapshot.ports.contains(&0)
        || snapshot.ports.windows(2).any(|pair| pair[0] >= pair[1])
        || snapshot.listeners.windows(2).any(|pair| pair[0] >= pair[1])
        || snapshot.listeners.iter().any(|listener| {
            !snapshot.hostnames.contains(&listener.hostname)
                || !snapshot.ports.contains(&listener.port)
                || !matches!(listener.protocol.as_str(), "HTTP" | "HTTPS")
        })
        || snapshot.hostnames.iter().any(|host| {
            !snapshot
                .listeners
                .iter()
                .any(|listener| &listener.hostname == host)
        })
        || snapshot.ports.iter().any(|port| {
            !snapshot
                .listeners
                .iter()
                .any(|listener| listener.port == *port)
        })
    {
        return Err(Error::Invalid(
            "invalid or ambiguous route listeners".into(),
        ));
    }
    if snapshot.weights.len() != snapshot.deployments.len() {
        return Err(Error::Missing("route backend weights".into()));
    }
    names.clear();
    for target in &snapshot.deployments {
        if !valid_text(&target.deployment)
            || !names.insert(&target.deployment)
            || !snapshot.weights.contains_key(&target.deployment)
            || !valid_text(&target.revision)
            || !valid_flux_revision(&target.flux_revision)
            || !valid_text(&target.backend)
            || crate::observe::validate_engine_spec(&target.engine_spec).is_err()
            || target.nodes.is_empty()
            || target.nodes.iter().any(|node| !valid_text(node))
            || !digest(&target.desired.rendered_resources)
            || !digest(&target.desired.resolved_dependencies)
            || !digest(&target.rollout_configuration)
            || !valid_runtime(&target.runtime)
            || snapshot.agentic != target.underlying_model.is_some()
            || target.underlying_model.as_ref().is_some_and(|model| {
                !digest(&model.rollout_configuration)
                    || !valid_runtime(&model.runtime)
                    || model.runtime.image_digests != target.runtime.image_digests
                    || model.runtime.model_content != target.runtime.model_content
                    || model.runtime.tokenizer_content != target.runtime.tokenizer_content
            })
        {
            return Err(Error::Invalid(
                "incomplete, duplicate, or invalid deployment identity".into(),
            ));
        }
    }
    Ok(())
}

fn validate_snapshot_for_run(snapshot: &RouteSnapshot, run: &RunBinding) -> Result<()> {
    validate_snapshot(snapshot)?;
    if snapshot.service != run.service
        || snapshot
            .deployments
            .iter()
            .filter(|d| d.deployment == run.deployment)
            .count()
            != 1
        || (run.functional && snapshot.deployments.len() != 1)
    {
        return Err(Error::Conflict(
            "route does not uniquely include the run target".into(),
        ));
    }
    Ok(())
}

fn validate_prelaunch(evidence: &PrelaunchEvidence, run: &RunBinding) -> Result<()> {
    if evidence.run_id.is_nil() || evidence.request_fingerprint != run.fingerprint {
        return Err(Error::Conflict("prelaunch run fingerprint mismatch".into()));
    }
    validate_snapshot_for_run(&evidence.snapshot, run)
}

fn validate_against_run(
    start: &RouteSnapshot,
    end: &RouteSnapshot,
    runner: &RunnerObservation,
    run: &RunBinding,
) -> Result<()> {
    let uid = run
        .job_uid
        .as_deref()
        .filter(|uid| valid_text(uid))
        .ok_or_else(|| Error::Missing("recorded Job UID".into()))?;
    let spec = run
        .job_spec
        .as_ref()
        .ok_or_else(|| Error::Missing("prepared Job spec".into()))?;
    verify_observations(start, end, runner, &run.job_name, uid, spec)
}

fn validate_verified(evidence: &VerifiedEvidence, run: &RunBinding) -> Result<()> {
    if evidence.run_id.is_nil() || evidence.request_fingerprint != run.fingerprint {
        return Err(Error::Conflict("verified run fingerprint mismatch".into()));
    }
    validate_snapshot_for_run(&evidence.prelaunch, run)?;
    validate_snapshot_for_run(&evidence.ingestion, run)?;
    validate_against_run(
        &evidence.prelaunch,
        &evidence.ingestion,
        &evidence.runner,
        run,
    )
}

/// Pure validation of two independently observed route snapshots and an owned
/// runner against the persisted Job identity/spec. Supplying request assertions
/// or fabricated observations here cannot establish evidence.
pub fn verify_observations(
    prelaunch: &RouteSnapshot,
    ingestion: &RouteSnapshot,
    runner: &RunnerObservation,
    job_name: &str,
    job_uid: &str,
    job_spec: &Value,
) -> Result<()> {
    validate_snapshot(prelaunch)?;
    validate_snapshot(ingestion)?;
    if !prelaunch.same_identity(ingestion) {
        return Err(Error::Conflict(
            "route or runtime identity changed during the run".into(),
        ));
    }
    if !valid_text(job_name)
        || !valid_text(job_uid)
        || !valid_text(&runner.pod)
        || !valid_text(&runner.pod_uid)
        || runner.job != job_name
        || runner.job_uid != job_uid
        || runner.pod_owner_job_uid != job_uid
    {
        return Err(Error::Conflict(
            "runner Job, Pod, or controller ownership mismatch".into(),
        ));
    }
    let containers = job_spec
        .pointer("/template/spec/containers")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Missing("prepared runner container".into()))?;
    let mut runners = containers
        .iter()
        .filter(|v| v.get("name").and_then(Value::as_str) == Some("runner"));
    let image = runners
        .next()
        .and_then(|v| v.get("image"))
        .and_then(Value::as_str)
        .filter(|v| valid_text(v))
        .ok_or_else(|| Error::Missing("prepared runner image".into()))?;
    if runners.next().is_some() {
        return Err(Error::Invalid(
            "duplicate prepared runner containers".into(),
        ));
    }
    let (repository, expected_digest) = image
        .rsplit_once('@')
        .filter(|(repository, d)| valid_text(repository) && digest(d))
        .ok_or_else(|| Error::Invalid("runner image must be digest pinned".into()))?;
    if !valid_text(repository) || runner.image != image || runner.image_id != expected_digest {
        return Err(Error::Conflict(
            "actual runner image digest mismatch".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DeploymentSnapshot, RouteListener};
    use chrono::{Duration, TimeZone, Utc};
    use serde_json::json;
    use thalamus_core::{DesiredIdentity, RuntimeIdentity};

    fn hash(ch: char) -> String {
        format!("sha256:{}", ch.to_string().repeat(64))
    }

    fn route() -> RouteSnapshot {
        RouteSnapshot {
            observed_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            service: "service".into(),
            route: "candidate".into(),
            agentic: false,
            hostnames: vec!["example.test".into()],
            paths: vec!["/candidates/model/v1/chat/completions".into()],
            ports: vec![443],
            listeners: vec![RouteListener {
                hostname: "example.test".into(),
                port: 443,
                protocol: "HTTPS".into(),
            }],
            weights: [("model".into(), 1)].into_iter().collect(),
            deployments: vec![DeploymentSnapshot {
                deployment: "model".into(),
                revision: "revision-1".into(),
                flux_revision: "main@sha1:0123456789012345678901234567890123456789".into(),
                rollout_configuration: hash('0'),
                backend: "service/model/revision-1".into(),
                engine_spec: json!({
                    "image": format!("registry.test/engine@{}", hash('c')),
                    "command": ["vllm", "serve", "/opt/thalamus/model"],
                    "args": ["--tokenizer", "/opt/thalamus/model", "--served-model-name", "service"]
                }),
                desired: DesiredIdentity {
                    rendered_resources: hash('a'),
                    resolved_dependencies: hash('b'),
                },
                runtime: RuntimeIdentity {
                    image_digests: [("engine".into(), hash('c'))].into_iter().collect(),
                    model_content: hash('d'),
                    tokenizer_content: hash('e'),
                    effective_configuration: hash('f'),
                    component_versions: [("engine".into(), hash('c'))].into_iter().collect(),
                },
                underlying_model: None,
                nodes: vec!["node-1".into()],
            }],
        }
    }

    fn runner() -> RunnerObservation {
        RunnerObservation {
            job: "job-1".into(),
            job_uid: "job-uid-1".into(),
            pod: "job-1-pod".into(),
            pod_uid: "pod-uid-1".into(),
            pod_owner_job_uid: "job-uid-1".into(),
            image: format!("registry.test/runner@{}", hash('1')),
            image_id: hash('1'),
        }
    }

    fn job_spec() -> Value {
        json!({"template":{"spec":{"containers":[
            {"name":"runner","image":runner().image}
        ]}}})
    }

    fn binding() -> RunBinding {
        RunBinding {
            fingerprint: "1".repeat(64),
            state: "ingesting".into(),
            service: "service".into(),
            deployment: "model".into(),
            functional: true,
            job_name: "job-1".into(),
            job_uid: Some("job-uid-1".into()),
            job_spec: Some(job_spec()),
        }
    }

    fn verified() -> VerifiedEvidence {
        VerifiedEvidence {
            run_id: Uuid::from_u128(1),
            request_fingerprint: "1".repeat(64),
            prelaunch: route(),
            ingestion: route(),
            runner: runner(),
        }
    }

    #[test]
    fn timestamp_is_historical_but_all_route_and_runtime_identity_is_compared() {
        let start = route();
        let mut end = start.clone();
        end.observed_at += Duration::seconds(30);
        assert!(
            verify_observations(&start, &end, &runner(), "job-1", "job-uid-1", &job_spec()).is_ok()
        );
        end.deployments[0].runtime.model_content = hash('9');
        assert!(matches!(
            verify_observations(&start, &end, &runner(), "job-1", "job-uid-1", &job_spec()),
            Err(Error::Conflict(_))
        ));
        end = start.clone();
        end.deployments[0]
            .runtime
            .image_digests
            .insert("engine".into(), hash('9'));
        assert!(
            verify_observations(&start, &end, &runner(), "job-1", "job-uid-1", &job_spec())
                .is_err()
        );
        end = start.clone();
        end.deployments[0].desired.resolved_dependencies = hash('9');
        assert!(
            verify_observations(&start, &end, &runner(), "job-1", "job-uid-1", &job_spec())
                .is_err()
        );
        end = start.clone();
        end.deployments[0].rollout_configuration = hash('9');
        assert!(
            verify_observations(&start, &end, &runner(), "job-1", "job-uid-1", &job_spec())
                .is_err()
        );
        end = start.clone();
        end.listeners[0].protocol = "HTTP".into();
        assert!(
            verify_observations(&start, &end, &runner(), "job-1", "job-uid-1", &job_spec())
                .is_err()
        );
        end = start.clone();
        end.deployments[0].nodes.push("node-2".into());
        assert!(
            verify_observations(&start, &end, &runner(), "job-1", "job-uid-1", &job_spec())
                .is_err()
        );
        end = start.clone();
        end.weights.insert("model".into(), 0);
        assert!(
            verify_observations(&start, &end, &runner(), "job-1", "job-uid-1", &job_spec())
                .is_err()
        );
        end = start.clone();
        end.deployments[0].engine_spec["args"] = json!([
            "--tokenizer",
            "/opt/thalamus/model",
            "--served-model-name",
            "service",
            "--max-model-len",
            "4096"
        ]);
        assert!(
            verify_observations(&start, &end, &runner(), "job-1", "job-uid-1", &job_spec())
                .is_err()
        );
    }

    #[test]
    fn missing_or_ambiguous_evidence_never_verifies() {
        let start = route();
        let mut missing = start.clone();
        missing.deployments.clear();
        assert!(
            verify_observations(
                &start,
                &missing,
                &runner(),
                "job-1",
                "job-uid-1",
                &job_spec()
            )
            .is_err()
        );
        missing = start.clone();
        missing.listeners.clear();
        assert!(
            verify_observations(
                &start,
                &missing,
                &runner(),
                "job-1",
                "job-uid-1",
                &job_spec()
            )
            .is_err()
        );
        missing = start.clone();
        missing.deployments[0].runtime.image_digests.clear();
        assert!(
            verify_observations(
                &start,
                &missing,
                &runner(),
                "job-1",
                "job-uid-1",
                &job_spec()
            )
            .is_err()
        );
        missing = start.clone();
        missing.weights.clear();
        assert!(
            verify_observations(
                &start,
                &missing,
                &runner(),
                "job-1",
                "job-uid-1",
                &job_spec()
            )
            .is_err()
        );
        missing = start.clone();
        missing.deployments[0].engine_spec = json!({});
        assert!(
            verify_observations(
                &start,
                &missing,
                &runner(),
                "job-1",
                "job-uid-1",
                &job_spec()
            )
            .is_err()
        );
        missing = start.clone();
        missing.deployments[0].flux_revision = "unresolved".into();
        assert!(
            verify_observations(
                &start,
                &missing,
                &runner(),
                "job-1",
                "job-uid-1",
                &job_spec()
            )
            .is_err()
        );
        missing = start.clone();
        missing.deployments.push(missing.deployments[0].clone());
        assert!(validate_snapshot(&missing).is_err());
        missing = start.clone();
        missing.listeners.push(missing.listeners[0].clone());
        assert!(validate_snapshot(&missing).is_err());
        assert!(verify_observations(&start, &start, &runner(), "job-1", "", &job_spec()).is_err());
        assert!(
            verify_observations(&start, &start, &runner(), "job-1", "job-uid-1", &json!({}))
                .is_err()
        );
    }

    #[test]
    fn agentic_records_require_the_complete_underlying_model_identity() {
        let mut observed = route();
        observed.agentic = true;
        assert!(validate_snapshot(&observed).is_err());
        let target = &mut observed.deployments[0];
        target.underlying_model = Some(crate::UnderlyingModelIdentity {
            rollout_configuration: target.rollout_configuration.clone(),
            runtime: target.runtime.clone(),
        });
        assert!(validate_snapshot(&observed).is_ok());
        observed.deployments[0]
            .underlying_model
            .as_mut()
            .unwrap()
            .runtime
            .model_content = hash('9');
        assert!(validate_snapshot(&observed).is_err());
        observed.agentic = false;
        assert!(validate_snapshot(&observed).is_err());
    }

    #[test]
    fn runner_must_match_the_recorded_job_owned_pod_and_actual_image() {
        let start = route();
        let mut observed = runner();
        observed.job_uid = "other-job".into();
        assert!(
            verify_observations(&start, &start, &observed, "job-1", "job-uid-1", &job_spec())
                .is_err()
        );
        observed = runner();
        observed.pod_owner_job_uid = "other-job".into();
        assert!(
            verify_observations(&start, &start, &observed, "job-1", "job-uid-1", &job_spec())
                .is_err()
        );
        observed = runner();
        observed.pod_uid.clear();
        assert!(
            verify_observations(&start, &start, &observed, "job-1", "job-uid-1", &job_spec())
                .is_err()
        );
        observed = runner();
        observed.image_id = hash('2');
        assert!(
            verify_observations(&start, &start, &observed, "job-1", "job-uid-1", &job_spec())
                .is_err()
        );
        observed = runner();
        observed.image = format!("other.test/runner@{}", hash('1'));
        assert!(
            verify_observations(&start, &start, &observed, "job-1", "job-uid-1", &job_spec())
                .is_err()
        );
        let duplicate = json!({"template":{"spec":{"containers":[
            {"name":"runner","image":runner().image},
            {"name":"runner","image":runner().image}
        ]}}});
        assert!(
            verify_observations(&start, &start, &runner(), "job-1", "job-uid-1", &duplicate)
                .is_err()
        );
    }

    #[test]
    fn exact_retry_is_idempotent_but_changed_runner_or_route_is_replay() {
        let existing = verified();
        let mut repeated = existing.ingestion.clone();
        repeated.observed_at += Duration::seconds(10);
        assert!(validate_retry("succeeded", "succeeded", &existing, &repeated, &runner()).is_ok());
        assert!(matches!(
            validate_retry("failed", "succeeded", &existing, &repeated, &runner()),
            Err(Error::Replay)
        ));
        let mut other_runner = runner();
        other_runner.pod_uid = "new-pod".into();
        assert!(matches!(
            validate_retry(
                "succeeded",
                "succeeded",
                &existing,
                &repeated,
                &other_runner
            ),
            Err(Error::Replay)
        ));
        repeated.deployments[0].revision = "revision-2".into();
        assert!(matches!(
            validate_retry("succeeded", "succeeded", &existing, &repeated, &runner()),
            Err(Error::Replay)
        ));
        assert!(same_optional_snapshot(
            Some(&existing.ingestion),
            Some(&existing.prelaunch)
        ));
        assert!(!same_optional_snapshot(Some(&existing.ingestion), None));
    }

    #[test]
    fn rejection_is_immutable_and_retry_safe() {
        let rejected = Rejection {
            reason: "identity changed".into(),
            snapshot: Some(route()),
            runner: Some(runner()),
        };
        let mut same = route();
        same.observed_at += Duration::seconds(5);
        assert!(
            validate_rejection_retry(&rejected, "identity changed", Some(&same), Some(&runner()))
                .is_ok()
        );
        assert!(matches!(
            validate_rejection_retry(&rejected, "different reason", Some(&same), Some(&runner())),
            Err(Error::Replay)
        ));
        assert!(matches!(
            validate_rejection_retry(&rejected, "identity changed", None, Some(&runner())),
            Err(Error::Replay)
        ));
        same.weights.insert("model".into(), 0);
        assert!(matches!(
            validate_rejection_retry(&rejected, "identity changed", Some(&same), Some(&runner())),
            Err(Error::Replay)
        ));
    }

    #[test]
    fn rejected_snapshot_cannot_persist_unreviewed_engine_configuration() {
        let mut unsafe_route = route();
        unsafe_route.deployments[0].engine_spec["env"] = json!([
            {"name": "UNREVIEWED_SECRET", "value": "sensitive"}
        ]);
        assert!(safe_rejection_snapshot(Some(&unsafe_route)).is_none());
        assert!(validate_snapshot(&unsafe_route).is_err());
        let original = Rejection {
            reason: "unsafe engine".into(),
            snapshot: None,
            runner: None,
        };
        assert!(
            validate_rejection_retry(&original, "unsafe engine", Some(&unsafe_route), None).is_ok()
        );
    }

    #[test]
    fn functional_run_is_bound_to_one_observed_target_and_fingerprint() {
        let mut run = binding();
        let mut observed = route();
        assert!(validate_snapshot_for_run(&observed, &run).is_ok());
        assert!(run.check_fingerprint(&"1".repeat(64)).is_ok());
        assert!(run.check_fingerprint(&"2".repeat(64)).is_err());
        let mut other = observed.deployments[0].clone();
        other.deployment = "other".into();
        observed.deployments.push(other);
        observed.weights.insert("other".into(), 0);
        assert!(validate_snapshot_for_run(&observed, &run).is_err());
        run.functional = false;
        assert!(validate_snapshot_for_run(&observed, &run).is_ok());
        run.deployment = "absent".into();
        assert!(validate_snapshot_for_run(&observed, &run).is_err());
    }
}
