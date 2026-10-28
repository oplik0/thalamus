//! MCP resource-server bridge to the authenticated application services.
use crate::AppState;
use async_trait::async_trait;
use sqlx::PgPool;
use std::sync::Arc;
use thalamus_identity::VerifiedPrincipal;
use thalamus_mcp::{Action, ControlPlane, Identity};
use thalamus_oidc_resource::ResourceVerifier;
use uuid::Uuid;

pub(crate) struct McpControlPlane {
    state: Arc<AppState>,
    verifier: Arc<ResourceVerifier>,
    audience: String,
}

impl McpControlPlane {
    pub(crate) fn new(state: Arc<AppState>, verifier: Arc<ResourceVerifier>) -> Self {
        let audience = verifier.metadata().resource;
        Self {
            state,
            verifier,
            audience,
        }
    }
    async fn create_run(
        &self,
        principal: VerifiedPrincipal<'_>,
        run: thalamus_mcp::Run,
        kind: thalamus_runs::RunKind,
    ) -> Result<serde_json::Value, String> {
        let (input, key) = run_input(run, kind)?;
        let result =
            crate::workflows::create_run_for(&self.state, principal, input, &key, Some("mcp"))
                .await
                .map_err(|_| "control-plane operation failed".to_owned())?;
        serde_json::to_value(result).map_err(|_| "control-plane operation failed".to_owned())
    }

    async fn authorize_debug_call(
        &self,
        identity: &Identity,
        binding: &str,
        action: &Action,
    ) -> Result<(), String> {
        let denied = || "debug tool authorization failed".to_owned();
        let credential = Uuid::parse_str(binding).map_err(|_| denied())?;
        let session = thalamus_debug::store::DebugStore::new(self.state.db.clone())
            .consume_tool_call_by_credential(credential, &self.audience)
            .await
            .map_err(|_| denied())?;
        if active_user(&self.state.db, identity)
            .await
            .map_err(|_| denied())?
            != session.user_id
        {
            return Err(denied());
        }
        let service = match action {
            Action::ServiceContext(s) | Action::QueryMetrics(s) | Action::CheckCompatibility(s) => {
                &s.service
            }
            Action::CompareRevisions(s) => &s.service,
            Action::GetLogs(s) => &s.service,
            Action::ProposeChange(s) => &s.service,
            Action::RunValidation(r) | Action::RunBenchmark(r) => {
                if Uuid::parse_str(&r.project_id).ok() != Some(session.project_id) {
                    return Err(denied());
                }
                &r.service
            }
            Action::GetRun(r) => {
                let id = Uuid::parse_str(&r.id).map_err(|_| denied())?;
                let (team, request): (String, serde_json::Value) =
                    sqlx::query_as("SELECT team, request FROM thalamus_runs WHERE id=$1")
                        .bind(id)
                        .fetch_optional(&self.state.db)
                        .await
                        .map_err(|_| denied())?
                        .ok_or_else(denied)?;
                let request: thalamus_runs::RunRequest =
                    serde_json::from_value(request).map_err(|_| denied())?;
                if team != session.team_id.to_string()
                    || request.project_id != session.project_id
                    || request.service != session.service_id
                {
                    return Err(denied());
                }
                return Ok(());
            }
        };
        if service != &session.service_id {
            return Err(denied());
        }
        Ok(())
    }
}

fn run_input(
    run: thalamus_mcp::Run,
    kind: thalamus_runs::RunKind,
) -> Result<(crate::workflows::CreateRun, String), String> {
    let project_id =
        Uuid::parse_str(&run.project_id).map_err(|_| "invalid project id".to_owned())?;
    if project_id.is_nil() {
        return Err("invalid project id".to_owned());
    }
    let workload = run.workload;
    Ok((
        crate::workflows::CreateRun {
            service: run.service,
            deployment: run.deployment,
            kind,
            project_id: Some(project_id),
            expected_runner: Some(run.runner),
            expected_workload: Some(thalamus_runs::Workload {
                name: workload.name,
                seed: workload.seed,
                concurrency: workload.concurrency,
                duration_seconds: workload.duration_seconds,
                warmup_seconds: workload.warmup_seconds,
                max_requests: workload.max_requests,
                model_budget_tokens: workload.model_budget_tokens,
                tool_budget_calls: workload.tool_budget_calls,
            }),
        },
        run.idempotency_key,
    ))
}

async fn active_user(db: &PgPool, identity: &Identity) -> Result<Uuid, ()> {
    sqlx::query_scalar(
        "SELECT id FROM identity_users WHERE issuer=$1 AND subject=$2 AND NOT disabled",
    )
    .bind(&identity.issuer)
    .bind(&identity.subject)
    .fetch_optional(db)
    .await
    .map_err(|_| ())?
    .ok_or(())
}

async fn authenticate(
    verifier: &ResourceVerifier,
    db: &PgPool,
    bearer: &str,
    audience: &str,
) -> Result<Identity, ()> {
    if bearer.starts_with("thld_") {
        let metadata = verifier.metadata();
        if metadata.resource != audience {
            return Err(());
        }
        let binding = thalamus_debug::store::DebugStore::new(db.clone())
            .lookup_tool_credential(bearer, audience)
            .await
            .map_err(|_| ())?;
        if metadata.authorization_servers.first() != Some(&binding.issuer) {
            return Err(());
        }
        let identity = Identity {
            issuer: binding.issuer,
            subject: binding.subject,
            debug_session: Some(binding.credential_id.to_string()),
        };
        if active_user(db, &identity).await? != binding.identity.user_id {
            return Err(());
        }
        return Ok(identity);
    }
    let subject = verifier.verify(bearer, audience).await.map_err(|_| ())?;
    let identity = Identity {
        issuer: subject.issuer,
        subject: subject.subject,
        debug_session: None,
    };
    active_user(db, &identity).await?;
    Ok(identity)
}

// There is deliberately no session creation or call to browser HTTP handlers here:
// those handlers authenticate a browser session, not a verified MCP principal.
// In particular the mutation services must authorize current roles and record both
// the local actor and agent attribution atomically with the operation.
#[async_trait]
impl ControlPlane for McpControlPlane {
    async fn authenticate(&self, bearer: &str, audience: &str) -> Result<Identity, ()> {
        authenticate(&self.verifier, &self.state.db, bearer, audience).await
    }

    async fn execute(
        &self,
        identity: &Identity,
        action: Action,
    ) -> Result<serde_json::Value, String> {
        if let Some(binding) = &identity.debug_session {
            self.authorize_debug_call(identity, binding, &action)
                .await?;
        }
        let principal = VerifiedPrincipal::Oidc {
            issuer: &identity.issuer,
            subject: &identity.subject,
        };
        match action {
            Action::ServiceContext(input) => {
                let result =
                    crate::catalog::get_service_context(&self.state, principal, &input.service)
                        .await
                        .map_err(|_| "control-plane operation failed".to_owned())?;
                serde_json::to_value(result)
                    .map_err(|_| "control-plane operation failed".to_owned())
            }
            Action::QueryMetrics(input) => {
                let result =
                    crate::catalog::query_service_metrics(&self.state, principal, &input.service)
                        .await
                        .map_err(|_| "control-plane operation failed".to_owned())?;
                serde_json::to_value(result)
                    .map_err(|_| "control-plane operation failed".to_owned())
            }
            Action::CompareRevisions(input) => {
                let result = crate::inspection::compare_revisions(
                    &self.state,
                    principal,
                    &input.service,
                    &input.from,
                    &input.to,
                )
                .await
                .map_err(|_| "control-plane operation failed".to_owned())?;
                serde_json::to_value(result)
                    .map_err(|_| "control-plane operation failed".to_owned())
            }
            Action::CheckCompatibility(input) => {
                let result =
                    crate::inspection::check_compatibility(&self.state, principal, &input.service)
                        .await
                        .map_err(|_| "control-plane operation failed".to_owned())?;
                serde_json::to_value(result)
                    .map_err(|_| "control-plane operation failed".to_owned())
            }
            Action::RunValidation(input) => {
                self.create_run(principal, input, thalamus_runs::RunKind::Validation)
                    .await
            }
            Action::RunBenchmark(input) => {
                self.create_run(principal, input, thalamus_runs::RunKind::Benchmark)
                    .await
            }
            Action::GetRun(input) => {
                let id = Uuid::parse_str(&input.id).map_err(|_| "invalid run id".to_owned())?;
                let result = crate::workflows::get_run_for(&self.state, principal, id)
                    .await
                    .map_err(|_| "control-plane operation failed".to_owned())?;
                serde_json::to_value(result)
                    .map_err(|_| "control-plane operation failed".to_owned())
            }
            Action::ProposeChange(input) => {
                let patch = input
                    .patch
                    .into_iter()
                    .map(|p| crate::workflows::PatchInput {
                        path: p.path,
                        operation: match p.operation {
                            thalamus_mcp::PatchOperation::Set(value) => {
                                crate::workflows::PatchOperation::Set(value)
                            }
                            thalamus_mcp::PatchOperation::Remove => {
                                crate::workflows::PatchOperation::Remove
                            }
                        },
                    })
                    .collect();
                let result = crate::workflows::propose_for(
                    &self.state,
                    principal,
                    input.service,
                    input.base_commit,
                    patch,
                    &input.idempotency_key,
                    Some("mcp"),
                )
                .await
                .map_err(|_| "control-plane operation failed".to_owned())?;
                serde_json::to_value(result)
                    .map_err(|_| "control-plane operation failed".to_owned())
            }
            Action::GetLogs(input) => {
                let result = crate::inspection::get_logs(
                    &self.state,
                    principal,
                    &input.service,
                    &input.deployment,
                    input.since_seconds,
                    input.limit,
                )
                .await
                .map_err(|_| "control-plane operation failed".to_owned())?;
                serde_json::to_value(result)
                    .map_err(|_| "control-plane operation failed".to_owned())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thalamus_mcp::{Run, Workload};
    use thalamus_oidc_resource::ResourceConfig;

    #[tokio::test]
    async fn rejects_wrong_resource_without_contacting_provider_or_database() {
        let verifier = ResourceVerifier::new(ResourceConfig {
            introspection_endpoint: "https://issuer.example/introspect".into(),
            client_id: "client".into(),
            client_secret: "secret".into(),
            issuer: "https://issuer.example".into(),
            audience: "https://api.example/mcp".into(),
            required_scopes: vec![],
        })
        .unwrap();
        let db = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@127.0.0.1/unused")
            .unwrap();
        assert!(
            authenticate(&verifier, &db, "token", "https://api.example/other")
                .await
                .is_err()
        );
        assert!(
            authenticate(&verifier, &db, "thld_fake", "https://api.example/other")
                .await
                .is_err()
        );
    }

    #[test]
    fn run_input_preserves_profile_constraints() {
        let run = || Run {
            project_id: "01234567-89ab-cdef-0123-456789abcdef".into(),
            service: "svc".into(),
            deployment: "dep".into(),
            runner: "runner".into(),
            workload: Workload {
                name: "workload".into(),
                seed: 0,
                concurrency: 1,
                duration_seconds: 1,
                warmup_seconds: 0,
                max_requests: 1,
                model_budget_tokens: 0,
                tool_budget_calls: 0,
            },
            idempotency_key: "key".into(),
        };
        let (input, key) = run_input(run(), thalamus_runs::RunKind::Validation).unwrap();
        assert_eq!(
            input.project_id,
            Some(Uuid::parse_str("01234567-89ab-cdef-0123-456789abcdef").unwrap())
        );
        assert_eq!(input.expected_runner.as_deref(), Some("runner"));
        assert_eq!(input.expected_workload.unwrap().max_requests, 1);
        assert_eq!(key, "key");
        let mut invalid = run();
        invalid.project_id = Uuid::nil().to_string();
        assert!(run_input(invalid, thalamus_runs::RunKind::Benchmark).is_err());
    }
}
