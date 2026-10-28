//! Durable run records, bounded Job templates, and immutable artifacts.
//! Authorization of the supplied team and actor is the caller's responsibility.

mod artifacts;
mod jobs;
mod storage;

pub use artifacts::{ArtifactRef, Artifacts};
pub use jobs::{
    build_job, verify_owned_job, verify_recoverable_job, JobTemplate, ResultUpload, Templates,
};
pub use storage::{Run, RunCursor, Store, Transition, WorkerStore};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid run: {0}")]
    Invalid(String),
    #[error("idempotency key already used for a different request")]
    IdempotencyConflict,
    #[error("run not found")]
    NotFound,
    #[error("invalid run state transition")]
    InvalidTransition,
    #[error("job is not owned by this run")]
    ForeignJob,
    #[error("artifact integrity mismatch")]
    ArtifactIntegrity,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    ObjectStore(#[from] object_store::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunKind {
    Validation,
    Benchmark,
    Debug,
}

/// Pinned identities, not merely tags or mutable PVC names. The producer must resolve
/// actual runtime identity and ensure its content manifests are immutable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceIdentity {
    pub desired_configuration: String,
    pub observed_runtime: String,
    pub test_definition: String,
    pub conditions: RunConditions,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunConditions {
    pub hardware: String,
    pub nodes: Vec<String>,
    pub replicas: u32,
    pub cache_state: String,
    pub competing_traffic: String,
    pub observed_at: chrono::DateTime<chrono::Utc>,
}

/// Parameters are deliberately numeric and bounded; neither command nor pod spec is user supplied.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Workload {
    pub name: String,
    pub seed: u64,
    pub concurrency: u32,
    pub duration_seconds: u32,
    pub warmup_seconds: u32,
    pub max_requests: u32,
    pub model_budget_tokens: u32,
    pub tool_budget_calls: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRequest {
    pub project_id: Uuid,
    pub kind: RunKind,
    pub service: String,
    pub deployment: String,
    pub runner: String,
    pub workload: Workload,
    pub evidence: EvidenceIdentity,
}

fn text(value: &str, field: &str, max: usize) -> Result<()> {
    if value.is_empty() || value.len() > max || value.chars().any(char::is_control) {
        return Err(Error::Invalid(format!("invalid {field}")));
    }
    Ok(())
}

impl RunRequest {
    pub fn validate(&self) -> Result<()> {
        if self.project_id.is_nil() {
            return Err(Error::Invalid("invalid project id".into()));
        }
        for (s, name) in [
            (&self.service, "service"),
            (&self.deployment, "deployment"),
            (&self.runner, "runner"),
            (&self.workload.name, "workload"),
            (
                &self.evidence.desired_configuration,
                "desired configuration",
            ),
            (&self.evidence.observed_runtime, "observed runtime"),
            (&self.evidence.test_definition, "test definition"),
            (&self.evidence.conditions.hardware, "hardware"),
            (&self.evidence.conditions.cache_state, "cache state"),
            (
                &self.evidence.conditions.competing_traffic,
                "competing traffic",
            ),
        ] {
            text(s, name, 1024)?;
        }
        let w = &self.workload;
        if !(1..=256).contains(&w.concurrency)
            || !(1..=3600).contains(&w.duration_seconds)
            || w.warmup_seconds > w.duration_seconds
            || !(1..=1_000_000).contains(&w.max_requests)
            || w.model_budget_tokens > 1_000_000
            || w.tool_budget_calls > 1000
        {
            return Err(Error::Invalid("workload outside permitted bounds".into()));
        }
        if matches!(self.kind, RunKind::Debug)
            && (w.model_budget_tokens == 0 || w.tool_budget_calls == 0)
        {
            return Err(Error::Invalid(
                "debug requires model and tool budgets".into(),
            ));
        }
        let c = &self.evidence.conditions;
        if !(1..=1000).contains(&c.replicas) || c.nodes.is_empty() || c.nodes.len() > 1000 {
            return Err(Error::Invalid("invalid run conditions".into()));
        }
        for node in &c.nodes {
            text(node, "node", 253)?;
        }
        Ok(())
    }
    pub fn fingerprint(&self) -> Result<String> {
        self.validate()?;
        Ok(hex(&Sha256::digest(serde_json::to_vec(self)?)))
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(&mut out, "{b:02x}").expect("writing to string");
    }
    out
}

pub fn job_name(id: Uuid) -> String {
    format!("thalamus-run-{id}")
}
