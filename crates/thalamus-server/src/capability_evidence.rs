//! Capability evidence from independently observed routing and immutable run results.
//!
//! The observer is an operator-controlled trust boundary. It must read the live
//! routing configuration and runtime, not the desired catalog or a run request.
//! It also attests the identity under which each selected run actually executed.
//! Without such an observer and the worker's artifact store, no provider should be
//! installed. In particular, run requests alone are not observations.
use std::{collections::BTreeSet, future::Future, pin::Pin, sync::Arc};

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use sqlx::{PgPool, Row};
use thalamus_core::{
    published_capabilities, CapabilityContract, EligibleDeployment, EngineProtocol,
    EvidenceIdentity, PublishedService, ValidatedCapabilities,
};
use thalamus_runs::{ArtifactRef, Artifacts, RunRequest};
use uuid::Uuid;

use crate::client_config::{CapabilityEvidence, CapabilityEvidenceProvider};

/// A fresh, independently collected complete routing observation. All targets that
/// can receive traffic, including failover and zero-weight targets, are required.
#[derive(Clone, Debug, PartialEq)]
pub struct RoutingObservation {
    pub observed_at: DateTime<Utc>,
    pub team_id: Uuid,
    pub project_id: Uuid,
    pub service: PublishedService,
    pub deployments: Vec<DeploymentObservation>,
}

/// Runtime and run attestation supplied by the trusted observer, not the caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeploymentObservation {
    pub observed_at: DateTime<Utc>,
    pub deployment: String,
    pub revision: String,
    /// The backend identity independently observed for the actual serving revision.
    pub backend: String,
    /// The four independently established current identities.
    pub current_identity: EvidenceIdentity,
    /// The independently established identities at execution of `run_id`.
    pub run_identity: EvidenceIdentity,
    pub run_id: Uuid,
    /// Original run request attested by the observer at execution, not supplied by
    /// the caller of client config. It must match the immutable DB record and result.
    pub run_request: RunRequest,
}

/// An implementation must observe live state and independently authenticate run
/// attestations. It must return None if any part cannot be observed. It must not
/// construct attestations by echoing the run's request or declared capabilities.
pub trait ObservationSource: Send + Sync {
    fn observe<'a>(
        &'a self,
        service: &'a str,
    ) -> Pin<Box<dyn Future<Output = Option<RoutingObservation>> + Send + 'a>>;
}

/// Provider backed by live observations, Postgres run records and the worker's
/// content-addressed artifact store. Instantiate only with a trusted observer.
pub struct Provider {
    db: PgPool,
    artifacts: Artifacts,
    observer: Arc<dyn ObservationSource>,
    max_age: Duration,
}

impl Provider {
    pub fn new(
        db: PgPool,
        artifacts: Artifacts,
        observer: Arc<dyn ObservationSource>,
        max_age_seconds: i64,
    ) -> Result<Self, &'static str> {
        let max_age = Duration::try_seconds(max_age_seconds)
            .filter(|_| max_age_seconds > 0)
            .ok_or("invalid evidence freshness limit")?;
        Ok(Self {
            db,
            artifacts,
            observer,
            max_age,
        })
    }

    async fn collect_inner(&self, service: &str, user: Uuid) -> Option<CapabilityEvidence> {
        let observation = self.observer.observe(service).await?;
        let now = Utc::now();
        if observation.service.name != service
            || observation.team_id.is_nil()
            || observation.project_id.is_nil()
            || !fresh(observation.observed_at, now, self.max_age)
        {
            return None;
        }
        // Resolve ownership and current membership from the identity database, not
        // from the caller or a service name supplied by the caller.
        let authorized: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM identity_projects p JOIN identity_users u ON u.id=$3 AND NOT u.disabled LEFT JOIN identity_memberships m ON m.team_id=p.team_id AND m.user_id=u.id WHERE p.id=$1 AND p.team_id=$2 AND (u.platform_admin OR m.role IS NOT NULL))")
            .bind(observation.project_id).bind(observation.team_id).bind(user)
            .fetch_one(&self.db).await.ok()?;
        if !authorized {
            return None;
        }
        let mut eligible = Vec::with_capacity(observation.deployments.len());
        let mut validations = Vec::with_capacity(observation.deployments.len());
        for target in &observation.deployments {
            if !valid_target(target, now, self.max_age) {
                return None;
            }
            // Do not ignore newer failed, pending, or incompatible functional runs.
            let row = sqlx::query("SELECT id, state, request, artifact, job_uid, created_at, updated_at FROM thalamus_runs WHERE team=$1 AND request->>'project_id'=$2 AND request->>'service'=$3 AND request->>'deployment'=$4 AND request->>'kind'='validation' AND request->'workload'->>'name'='functional' ORDER BY created_at DESC,id DESC LIMIT 1")
                .bind(observation.team_id.to_string()).bind(observation.project_id.to_string())
                .bind(service).bind(&target.deployment).fetch_optional(&self.db).await.ok()??;
            let id: Uuid = row.try_get("id").ok()?;
            let state: String = row.try_get("state").ok()?;
            let job_uid: Option<String> = row.try_get("job_uid").ok()?;
            let created: DateTime<Utc> = row.try_get("created_at").ok()?;
            let updated: DateTime<Utc> = row.try_get("updated_at").ok()?;
            let request: RunRequest = serde_json::from_value(row.try_get("request").ok()?).ok()?;
            if id != target.run_id
                || state != "succeeded"
                || job_uid.as_deref().is_none_or(|s| s.trim().is_empty())
                || !fresh(created, now, self.max_age)
                || !fresh(updated, now, self.max_age)
                || request != target.run_request
            {
                return None;
            }
            let refs: Vec<ArtifactRef> =
                serde_json::from_value(row.try_get::<Option<Value>, _>("artifact").ok()??).ok()?;
            if refs.len() != 1 {
                return None;
            }
            let bytes = self.artifacts.read(id, &refs[0]).await.ok()?;
            let summary: Value = serde_json::from_slice(&bytes).ok()?;
            let contract = validated_contract(&summary, id, &request, &target.backend)?;
            eligible.push(EligibleDeployment {
                deployment: target.deployment.clone(),
                revision: target.revision.clone(),
            });
            validations.push(ValidatedCapabilities {
                deployment: target.deployment.clone(),
                revision: target.revision.clone(),
                run_id: id.to_string(),
                contract,
            });
        }
        published_capabilities(&observation.service, &eligible, &validations).ok()?;
        // Discard results if routing or any observed identity changed during reads.
        // Collection timestamps naturally advance on a fresh observation.
        let mut again = self.observer.observe(service).await?;
        let finished = Utc::now();
        if !fresh(again.observed_at, finished, self.max_age)
            || again
                .deployments
                .iter()
                .any(|target| !valid_target(target, finished, self.max_age))
            || again.deployments.len() != observation.deployments.len()
        {
            return None;
        }
        again.observed_at = observation.observed_at;
        for (new, old) in again.deployments.iter_mut().zip(&observation.deployments) {
            new.observed_at = old.observed_at;
        }
        if again != observation {
            return None;
        }
        Some(CapabilityEvidence {
            service: observation.service,
            eligible,
            validations,
        })
    }
}

impl CapabilityEvidenceProvider for Provider {
    fn collect<'a>(
        &'a self,
        service: &'a str,
        user: Uuid,
    ) -> Pin<Box<dyn Future<Output = Option<CapabilityEvidence>> + Send + 'a>> {
        Box::pin(self.collect_inner(service, user))
    }
}

fn fresh(at: DateTime<Utc>, now: DateTime<Utc>, max_age: Duration) -> bool {
    at <= now && now.signed_duration_since(at) <= max_age
}

fn digest(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn valid_target(target: &DeploymentObservation, now: DateTime<Utc>, max_age: Duration) -> bool {
    let runtime = &target.current_identity.runtime;
    !target.deployment.trim().is_empty()
        && !target.revision.trim().is_empty()
        && !target.backend.trim().is_empty()
        && !target.run_id.is_nil()
        && fresh(target.observed_at, now, max_age)
        && fresh(
            target.run_request.evidence.conditions.observed_at,
            now,
            max_age,
        )
        && digest(&runtime.model_content)
        && digest(&runtime.tokenizer_content)
        && digest(&runtime.effective_configuration)
        && target
            .current_identity
            .permits_reuse_of(&target.run_identity)
}

/// Publish a protocol only when its own authentication, authorization, streaming,
/// tool loop and context checks passed. Continuation alone is not such evidence.
fn validated_contract(
    summary: &Value,
    id: Uuid,
    request: &RunRequest,
    backend: &str,
) -> Option<CapabilityContract> {
    if summary.get("run_id")?.as_str()? != id.to_string()
        || !summary.get("passed")?.as_bool()?
        || summary.get("suite")?.as_str()? != "candidate"
        || serde_json::from_value::<RunRequest>(summary.get("request")?.clone()).ok()? != *request
        || summary.get("runner_version")?.as_str()?.trim().is_empty()
    {
        return None;
    }
    let backends = summary.get("answering_backends")?.as_array()?;
    if backends.len() != 1 || backends[0].as_str()? != backend {
        return None;
    }
    let checks = summary.get("checks")?.as_array()?;
    if checks
        .iter()
        .any(|c| c.get("status").and_then(Value::as_str) == Some("failed"))
    {
        return None;
    }
    let check = |name: &str| -> Option<&Value> {
        let mut matches = checks
            .iter()
            .filter(|v| v.get("name").and_then(Value::as_str) == Some(name));
        let result = matches.next()?;
        if matches.next().is_some() || result.get("status")?.as_str()? != "passed" {
            return None;
        }
        Some(result)
    };
    for name in [
        "authentication",
        "authorization",
        "chat",
        "streaming",
        "tool_loop",
    ] {
        check(name)?;
    }
    let context_for = |name: &str| -> Option<u32> {
        let detail = check(name)?.get("detail")?.as_str()?;
        let context: u32 = detail
            .strip_prefix("accepted ")?
            .strip_suffix(" prompt tokens; quality under load not measured")?
            .parse()
            .ok()?;
        (context > 0 && context <= request.workload.model_budget_tokens).then_some(context)
    };
    let mut context = context_for("context_limit")?;
    let mut protocols = BTreeSet::from([EngineProtocol::OpenAiChat]);
    for (prefix, protocol) in [
        (
            "responses",
            EngineProtocol::Other("openai_responses".into()),
        ),
        ("anthropic", EngineProtocol::AnthropicMessages),
    ] {
        let names = [
            prefix.to_owned(),
            format!("{prefix}_authentication"),
            format!("{prefix}_authorization"),
            format!("{prefix}_streaming"),
            format!("{prefix}_tool_loop"),
        ];
        if names.iter().all(|name| check(name).is_some()) {
            if let Some(limit) = context_for(&format!("{prefix}_context_limit")) {
                context = context.min(limit);
                protocols.insert(protocol);
            }
        }
    }
    Some(CapabilityContract {
        context_tokens: context,
        features: BTreeSet::from(["tools".into(), "streaming".into()]),
        protocols,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request() -> RunRequest {
        serde_json::from_value(json!({
            "project_id": Uuid::from_u128(1),
            "kind": "validation", "service": "svc", "deployment": "dep", "runner": "runner",
            "workload": {"name":"functional","seed":1,"concurrency":1,"duration_seconds":30,
                "warmup_seconds":0,"max_requests":20,"model_budget_tokens":4096,"tool_budget_calls":1},
            "evidence": {"desired_configuration":"d","observed_runtime":"r","test_definition":"t",
                "conditions":{"hardware":"gpu","nodes":["n"],"replicas":1,"cache_state":"cold",
                    "competing_traffic":"none","observed_at":"2026-01-01T00:00:00Z"}}
        })).unwrap()
    }

    fn result(id: Uuid, request: &RunRequest) -> Value {
        let checks: Vec<_> = ["authentication", "authorization", "chat", "streaming", "tool_loop", "context_limit"]
            .into_iter().map(|name| json!({"name":name,"status":"passed",
                "detail": if name == "context_limit" { "accepted 2048 prompt tokens; quality under load not measured" } else { "ok" }})).collect();
        json!({"run_id":id.to_string(),"request":request,"suite":"candidate","runner_version":"1",
            "answering_backends":["dep-revision"],"checks":checks,"passed":true})
    }

    #[test]
    fn contract_requires_real_successful_chat_evidence() {
        let id = Uuid::from_u128(2);
        let request = request();
        let summary = result(id, &request);
        let contract = validated_contract(&summary, id, &request, "dep-revision").unwrap();
        assert_eq!(contract.context_tokens, 2048);
        assert_eq!(
            contract.protocols,
            BTreeSet::from([EngineProtocol::OpenAiChat])
        );
        assert!(contract.features.contains("tools") && contract.features.contains("streaming"));
        assert!(validated_contract(&summary, id, &request, "other-revision").is_none());
        let mut missing = summary.clone();
        missing["checks"][4]["status"] = json!("unsupported");
        assert!(validated_contract(&missing, id, &request, "dep-revision").is_none());
        let mut duplicate = summary.clone();
        duplicate["checks"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":"chat","status":"passed"}));
        assert!(validated_contract(&duplicate, id, &request, "dep-revision").is_none());
        let mut failed = summary.clone();
        failed["checks"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":"other","status":"failed"}));
        assert!(validated_contract(&failed, id, &request, "dep-revision").is_none());
        let mut public = summary;
        public["suite"] = json!("public");
        assert!(validated_contract(&public, id, &request, "dep-revision").is_none());
    }

    #[test]
    fn protocol_requires_its_own_complete_evidence() {
        let id = Uuid::from_u128(2);
        let request = request();
        let mut summary = result(id, &request);
        for name in [
            "responses",
            "responses_authentication",
            "responses_authorization",
            "responses_streaming",
            "responses_tool_loop",
            "responses_context_limit",
        ] {
            summary["checks"]
                .as_array_mut()
                .unwrap()
                .push(json!({"name":name,"status":"passed",
                "detail":"accepted 1024 prompt tokens; quality under load not measured"}));
        }
        let contract = validated_contract(&summary, id, &request, "dep-revision").unwrap();
        assert!(contract
            .protocols
            .contains(&EngineProtocol::Other("openai_responses".into())));
        assert_eq!(contract.context_tokens, 1024);
        summary["checks"][10]["status"] = json!("unsupported");
        let contract = validated_contract(&summary, id, &request, "dep-revision").unwrap();
        assert!(!contract
            .protocols
            .contains(&EngineProtocol::Other("openai_responses".into())));
        assert_eq!(contract.context_tokens, 2048);
    }

    #[test]
    fn freshness_and_content_digest_are_strict() {
        let now = Utc::now();
        let age = Duration::seconds(10);
        assert!(fresh(now - Duration::seconds(10), now, age));
        assert!(!fresh(now - Duration::seconds(11), now, age));
        assert!(!fresh(now + Duration::seconds(1), now, age));
        assert!(digest(&format!("sha256:{}", "a".repeat(64))));
        assert!(!digest("model:latest"));
        assert!(!digest("sha256:1234"));
    }
}
