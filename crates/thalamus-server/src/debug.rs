//! Owner- and team-scoped control and persisted transcripts for debug sessions.
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Extension, Json, Router,
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thalamus_debug::{
    proxy::{Actor, ProxyError, ServiceCidr},
    store::{
        DebugEvent, DebugIdentity, DebugSession, DebugStore, NewDebugSession, SessionState,
        StoreError,
    },
};
use thalamus_identity::VerifiedPrincipal;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{ApiError, AppState};

/// Operator-owned debug admission defaults. Worker configuration independently
/// enforces the same namespace, checkout revision, allowed models and ceilings.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DebugConfig {
    #[serde(default = "default_namespace")]
    pub namespace: String,
    pub checkout_revision: String,
    pub allowed_models: Vec<String>,
    pub duration_seconds: i64,
    pub model_budget: i64,
    pub tool_budget: i64,
    pub service_cidrs: Vec<String>,
    /// Trusted internal Flux Operator MCP endpoint, never a request value.
    pub flux_mcp_url: String,
}

fn default_namespace() -> String {
    "thalamus-inference".to_owned()
}

impl DebugConfig {
    pub fn from_env() -> Result<Option<Self>, String> {
        let value = match std::env::var("THALAMUS_DEBUG_SERVER_CONFIG") {
            Ok(value) if !value.trim().is_empty() => value,
            Ok(_) => return Err("THALAMUS_DEBUG_SERVER_CONFIG must not be empty".into()),
            Err(std::env::VarError::NotPresent) => return Ok(None),
            Err(_) => return Err("THALAMUS_DEBUG_SERVER_CONFIG is not Unicode".into()),
        };
        let config: Self = serde_json::from_str(&value)
            .map_err(|_| "THALAMUS_DEBUG_SERVER_CONFIG has invalid fields or JSON".to_owned())?;
        crate::flux_mcp::validate_upstream_url(&config.flux_mcp_url).map_err(str::to_owned)?;
        let namespace = config.namespace.as_bytes();
        let dns_char = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
        let revision = config.checkout_revision.as_bytes();
        if namespace.is_empty()
            || namespace.len() > 63
            || !dns_char(namespace[0])
            || !dns_char(namespace[namespace.len() - 1])
            || !namespace.iter().all(|b| dns_char(*b) || *b == b'-')
            || !matches!(revision.len(), 40 | 64)
            || !revision
                .iter()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
            || config.allowed_models.len() != 1
            || config.allowed_models.iter().any(|s| {
                s.is_empty() || s.len() > 256 || !s.bytes().all(|b| (b'!'..=b'~').contains(&b))
            })
            || !(1..=86_400).contains(&config.duration_seconds)
            || config.model_budget <= 0
            || config.tool_budget <= 0
            || config.service_cidrs.is_empty()
            || config.service_cidrs.len() > 64
            || config
                .service_cidrs
                .iter()
                .any(|cidr| cidr.parse::<ServiceCidr>().is_err())
        {
            return Err("THALAMUS_DEBUG_SERVER_CONFIG violates debug limits".into());
        }
        Ok(Some(config))
    }
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/debug/models", get(models))
        .route("/debug/sessions", post(create_session))
        .route(
            "/debug/sessions/{id}",
            get(get_session).delete(cancel_session),
        )
        .route("/debug/sessions/{id}/events", get(get_events))
        .route("/debug/sessions/{id}/reply", post(reply))
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DebugSessionCreateRequest {
    pub project_id: Uuid,
    pub service_id: String,
    #[serde(default)]
    pub deployment: Option<String>,
    #[serde(default)]
    pub failed_run_id: Option<Uuid>,
    pub model: String,
    pub client_request_key: String,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DebugReplyRequest {
    pub text: String,
    pub client_request_key: String,
}

/// The same fields as the durable session, excluding internal initialization
/// claims and all Job credentials.
#[derive(Serialize, ToSchema)]
pub(crate) struct DebugSessionResponse {
    pub id: Uuid,
    pub user_id: Uuid,
    pub team_id: Uuid,
    pub project_id: Uuid,
    pub client_request_key: String,
    pub service_id: String,
    pub run_id: Option<Uuid>,
    pub model: String,
    pub checkout_revision: String,
    pub job_namespace: String,
    pub job_name: String,
    pub deadline: DateTime<Utc>,
    pub model_budget: i64,
    pub tool_budget: i64,
    pub tool_calls_used: i64,
    pub service_context: Value,
    pub failing_evidence: Value,
    pub state: String,
    pub outcome: Option<Value>,
    pub cleanup_required: bool,
    pub upstream_session_id: Option<String>,
    /// The upstream seed was durably accepted; replies are permitted only then.
    pub seed_accepted: bool,
    pub next_event_sequence: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl DebugSessionResponse {
    fn from_session(session: DebugSession, seed_accepted: bool) -> Self {
        let state = match session.state {
            SessionState::Pending => "pending",
            SessionState::Running => "running",
            SessionState::Completed => "completed",
            SessionState::Failed => "failed",
            SessionState::Cancelled => "cancelled",
            SessionState::TimedOut => "timed_out",
        };
        Self {
            id: session.id,
            user_id: session.user_id,
            team_id: session.team_id,
            project_id: session.project_id,
            client_request_key: session.client_request_key,
            service_id: session.service_id,
            run_id: session.run_id,
            model: session.model,
            checkout_revision: session.checkout_revision,
            job_namespace: session.job_namespace,
            job_name: session.job_name,
            deadline: session.deadline,
            model_budget: session.model_budget,
            tool_budget: session.tool_budget,
            tool_calls_used: session.tool_calls_used,
            service_context: session.service_context,
            failing_evidence: session.failing_evidence,
            state: state.into(),
            outcome: session.outcome,
            cleanup_required: session.cleanup_required,
            upstream_session_id: session.upstream_session_id,
            seed_accepted,
            next_event_sequence: session.next_event_sequence,
            created_at: session.created_at,
            updated_at: session.updated_at,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct DebugEventResponse {
    pub session_id: Uuid,
    pub sequence: i64,
    pub client_request_key: String,
    pub kind: String,
    pub payload: Value,
    pub created_at: DateTime<Utc>,
}

impl From<DebugEvent> for DebugEventResponse {
    fn from(event: DebugEvent) -> Self {
        Self {
            session_id: event.session_id,
            sequence: event.sequence,
            client_request_key: event.client_request_key,
            kind: event.kind,
            payload: event.payload,
            created_at: event.created_at,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelsQuery {
    project_id: Uuid,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct DebugModelsResponse {
    pub models: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionQuery {
    team_id: Uuid,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EventsQuery {
    team_id: Uuid,
    #[serde(default)]
    after: i64,
    #[serde(default = "default_limit")]
    limit: i64,
}
fn default_limit() -> i64 {
    100
}

fn store_error(error: StoreError) -> ApiError {
    match error {
        StoreError::Invalid(_) => ApiError::new(StatusCode::BAD_REQUEST, "invalid debug request"),
        StoreError::Forbidden | StoreError::NotFound | StoreError::InvalidCredential => {
            ApiError::new(StatusCode::NOT_FOUND, "debug session not found")
        }
        StoreError::IdempotencyConflict
        | StoreError::InvalidTransition
        | StoreError::Inactive
        | StoreError::BudgetExhausted
        | StoreError::InitializationBusy => {
            ApiError::new(StatusCode::CONFLICT, "debug session state conflict")
        }
        StoreError::Database(_)
        | StoreError::IdentityProjectionStale
        | StoreError::SequenceExhausted
        | StoreError::RandomUnavailable
        | StoreError::CorruptData => {
            ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "debug storage unavailable")
        }
    }
}

fn principal<'a>(principal: &'a VerifiedPrincipal<'_>) -> VerifiedPrincipal<'a> {
    match principal {
        VerifiedPrincipal::Session(token) => VerifiedPrincipal::Session(token),
        VerifiedPrincipal::Oidc { issuer, subject } => VerifiedPrincipal::Oidc { issuer, subject },
    }
}

/// Resolve the session under an identity guard, verify service policy, and then
/// reload it under a new guard. Store operations in the guarded transaction avoid
/// taking a second identity lock on another connection.
async fn authorized_session(
    state: &AppState,
    caller: &VerifiedPrincipal<'_>,
    team: Uuid,
    id: Uuid,
) -> Result<DebugSession, ApiError> {
    let store = DebugStore::new(state.db.clone());
    let mut guard = state
        .identity
        .begin_authorized(principal(caller), team, false)
        .await?;
    let identity = DebugIdentity {
        user_id: guard.actor_id(),
        team_id: team,
    };
    let session = store
        .get_in_transaction(guard.transaction(), &identity, id)
        .await
        .map_err(store_error)?;
    guard.commit().await?;

    let source = state.policy_source.as_ref().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "service access policy unavailable",
        )
    })?;
    let policy = source.load().map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "service access policy unavailable",
        )
    })?;
    let service = policy
        .service_id(&session.service_id)
        .map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "service access policy unavailable",
            )
        })?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "debug session not found"))?;
    let teams = policy
        .authorized_teams(&state.identity, principal(caller), &service)
        .await
        .map_err(|error| match error {
            thalamus_access::Error::Identity(error) => error.into(),
            thalamus_access::Error::Forbidden => {
                ApiError::new(StatusCode::NOT_FOUND, "debug session not found")
            }
            thalamus_access::Error::Source => ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "service access policy unavailable",
            ),
        })?;
    if !teams.contains(&team) {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "debug session not found",
        ));
    }
    Ok(session)
}

pub(crate) async fn session_for(
    state: &AppState,
    caller: &VerifiedPrincipal<'_>,
    team: Uuid,
    id: Uuid,
) -> Result<DebugSession, ApiError> {
    let checked = authorized_session(state, caller, team, id).await?;
    let mut guard = state
        .identity
        .begin_authorized(principal(caller), team, false)
        .await?;
    let identity = DebugIdentity {
        user_id: guard.actor_id(),
        team_id: team,
    };
    let session = DebugStore::new(state.db.clone())
        .get_in_transaction(guard.transaction(), &identity, id)
        .await
        .map_err(store_error)?;
    if session.service_id != checked.service_id {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "debug session not found",
        ));
    }
    guard.commit().await?;
    Ok(session)
}

pub(crate) async fn events_for(
    state: &AppState,
    caller: &VerifiedPrincipal<'_>,
    team: Uuid,
    id: Uuid,
    after: i64,
    limit: i64,
) -> Result<Vec<DebugEvent>, ApiError> {
    let checked = authorized_session(state, caller, team, id).await?;
    let store = DebugStore::new(state.db.clone());
    let mut guard = state
        .identity
        .begin_authorized(principal(caller), team, false)
        .await?;
    let identity = DebugIdentity {
        user_id: guard.actor_id(),
        team_id: team,
    };
    let session = store
        .get_in_transaction(guard.transaction(), &identity, id)
        .await
        .map_err(store_error)?;
    if session.service_id != checked.service_id {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "debug session not found",
        ));
    }
    let events = store
        .events_in_transaction(guard.transaction(), &identity, id, after, limit)
        .await
        .map_err(store_error)?;
    guard.commit().await?;
    Ok(events)
}

fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "debug resource not found")
}

fn policy_error(error: thalamus_access::Error) -> ApiError {
    match error {
        thalamus_access::Error::Identity(error) => error.into(),
        thalamus_access::Error::Forbidden => not_found(),
        thalamus_access::Error::Source => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "service access policy unavailable",
        ),
    }
}

async fn project_team(
    state: &AppState,
    caller: &VerifiedPrincipal<'_>,
    project_id: Uuid,
) -> Result<Uuid, ApiError> {
    if project_id.is_nil() {
        return Err(not_found());
    }
    state
        .identity
        .authenticate_principal(principal(caller))
        .await?;
    let team: Uuid = sqlx::query_scalar("SELECT team_id FROM identity_projects WHERE id=$1")
        .bind(project_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(not_found)?;
    let guard = state
        .identity
        .begin_authorized(principal(caller), team, false)
        .await
        .map_err(|error| match error {
            thalamus_identity::Error::Forbidden => not_found(),
            other => other.into(),
        })?;
    guard.commit().await?;
    Ok(team)
}

async fn service_for_team(
    state: &AppState,
    policy: &thalamus_access::Policy,
    caller: &VerifiedPrincipal<'_>,
    team: Uuid,
    name: &str,
    namespace: &str,
) -> Result<thalamus_access::ServiceId, ApiError> {
    let service = policy
        .service_id(name)
        .map_err(policy_error)?
        .ok_or_else(not_found)?;
    let teams = policy
        .authorized_teams(&state.identity, principal(caller), &service)
        .await
        .map_err(policy_error)?;
    if !teams.contains(&team) || service.namespace != namespace {
        return Err(not_found());
    }
    Ok(service)
}

#[utoipa::path(get, path = "/api/v1/debug/models",
    params(("project_id" = Uuid, Query)),
    responses((status = 200, body = DebugModelsResponse), (status = 404, description = "Project not found"), (status = 503, description = "Debug configuration unavailable")))]
pub(crate) async fn models(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<crate::RequestPrincipal>,
    Query(query): Query<ModelsQuery>,
) -> Result<Json<DebugModelsResponse>, ApiError> {
    let config =
        state.config.debug.as_ref().ok_or_else(|| {
            ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "debug is not configured")
        })?;
    let team = project_team(&state, &caller.principal(), query.project_id).await?;
    let (policy, _) = crate::catalog::committed_catalog(&state).await?;
    let mut models = Vec::new();
    for model in &config.allowed_models {
        let Some(service) = policy.service_id(model).map_err(policy_error)? else {
            continue;
        };
        if service.namespace != config.namespace {
            continue;
        }
        match policy
            .authorized_teams(&state.identity, caller.principal(), &service)
            .await
        {
            Ok(teams) if teams.contains(&team) => models.push(model.clone()),
            Ok(_) | Err(thalamus_access::Error::Forbidden) => {}
            Err(error) => return Err(policy_error(error)),
        }
    }
    Ok(Json(DebugModelsResponse { models }))
}

#[utoipa::path(post, path = "/api/v1/debug/sessions",
    request_body = DebugSessionCreateRequest,
    responses((status = 200, body = DebugSessionResponse), (status = 400, description = "Invalid debug request"),
              (status = 404, description = "Project, service, deployment, or run not found"),
              (status = 409, description = "Idempotency conflict"), (status = 503, description = "Debug unavailable")))]
pub(crate) async fn create_session(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<crate::RequestPrincipal>,
    Json(input): Json<DebugSessionCreateRequest>,
) -> Result<Json<DebugSessionResponse>, ApiError> {
    let config =
        state.config.debug.as_ref().ok_or_else(|| {
            ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "debug is not configured")
        })?;
    if !config.allowed_models.contains(&input.model) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "model is not configured for debug",
        ));
    }
    let team = project_team(&state, &caller.principal(), input.project_id).await?;
    let (policy, _) = crate::catalog::committed_catalog(&state).await?;
    let service = service_for_team(
        &state,
        &policy,
        &caller.principal(),
        team,
        &input.service_id,
        &config.namespace,
    )
    .await?;
    let model = service_for_team(
        &state,
        &policy,
        &caller.principal(),
        team,
        &input.model,
        &config.namespace,
    )
    .await?;
    if let Some(deployment) = &input.deployment {
        if !policy
            .includes_deployment(&service, deployment)
            .map_err(policy_error)?
        {
            return Err(not_found());
        }
    }

    let store = DebugStore::new(state.db.clone());
    let mut guard = state
        .identity
        .begin_authorized(caller.principal(), team, false)
        .await?;
    let identity = DebugIdentity {
        user_id: guard.actor_id(),
        team_id: team,
    };
    // Lookup serializes concurrent uses of this owner/key until commit. A retry
    // reuses its original deadline and evidence instead of deriving them again.
    if let Some(existing) = store
        .get_by_request_key_in_transaction(
            guard.transaction(),
            &identity,
            &input.client_request_key,
        )
        .await
        .map_err(store_error)?
    {
        if existing.project_id != input.project_id
            || existing.service_id != input.service_id
            || existing.run_id != input.failed_run_id
            || existing.model != input.model
            || existing
                .service_context
                .get("requested_deployment")
                .and_then(Value::as_str)
                != input.deployment.as_deref()
        {
            return Err(store_error(StoreError::IdempotencyConflict));
        }
        let seed_accepted = store
            .seed_accepted_in_transaction(guard.transaction(), &identity, existing.id)
            .await
            .map_err(store_error)?;
        guard.commit().await?;
        return Ok(Json(DebugSessionResponse::from_session(
            existing,
            seed_accepted,
        )));
    }

    let (deployment, evidence) = if let Some(run_id) = input.failed_run_id {
        let run = thalamus_runs::Store::new(state.db.clone())
            .get_in_transaction(guard.transaction(), &team.to_string(), run_id)
            .await
            .map_err(|error| match error {
                thalamus_runs::Error::NotFound => not_found(),
                _ => ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "run evidence unavailable"),
            })?;
        if run.request.project_id != input.project_id
            || run.request.service != input.service_id
            || !matches!(run.state.as_str(), "failed" | "ingestion_failed")
            || input
                .deployment
                .as_ref()
                .is_some_and(|name| name != &run.request.deployment)
            || !policy
                .includes_deployment(&service, &run.request.deployment)
                .map_err(policy_error)?
        {
            return Err(not_found());
        }
        // Runner output and stored errors may contain credentials. Include only
        // scoped identifiers and diagnostic metadata; MCP can retrieve other
        // authorized observations without exposing arbitrary raw Job objects.
        let observed = thalamus_runtime::records::Records::new(state.db.clone())
            .get_failure_in_transaction(guard.transaction(), run_id)
            .await
            .map_err(|_| {
                ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "run observation unavailable",
                )
            })?;
        let observation = if let Some(observed) = observed {
            if observed.run_id != run.id
                || observed.request_fingerprint != run.fingerprint
                || observed.prelaunch.service != input.service_id
                || observed.ingestion.service != input.service_id
            {
                return Err(ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "run observation is inconsistent",
                ));
            }
            let mut deployments = observed
                .ingestion
                .deployments
                .iter()
                .filter(|entry| entry.deployment == run.request.deployment);
            let entry = deployments.next().ok_or_else(|| {
                ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "run observation is inconsistent",
                )
            })?;
            if deployments.next().is_some() {
                return Err(ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "run observation is inconsistent",
                ));
            }
            Some(json!({
                "prelaunch_observed_at": observed.prelaunch.observed_at,
                "failure_observed_at": observed.ingestion.observed_at,
                "revision": entry.revision,
                "backend": entry.backend,
                "runner_job_uid": observed.runner.job_uid,
                "runner_pod_uid": observed.runner.pod_uid,
                "runner_image_id": observed.runner.image_id
            }))
        } else {
            None
        };
        let summary = if run.state == "failed" {
            "Kubernetes run job failed"
        } else {
            "run result ingestion failed"
        };
        let evidence = json!({
            "kind": "failed_run",
            "run_id": run.id,
            "state": run.state,
            "deployment": run.request.deployment,
            "summary": summary,
            "artifact_count": run.artifacts.as_ref().map_or(0, Vec::len),
            "failure_observation": observation,
            "created_at": run.created_at,
            "updated_at": run.updated_at
        });
        (Some(run.request.deployment), evidence)
    } else {
        (
            input.deployment.clone(),
            json!({"kind": "service_inspection", "run": "not supplied"}),
        )
    };
    let deadline = Utc::now()
        .checked_add_signed(Duration::seconds(config.duration_seconds))
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "invalid debug deadline"))?;
    let session = store
        .create_in_transaction(
            guard.transaction(),
            &identity,
            NewDebugSession {
                client_request_key: input.client_request_key,
                project_id: input.project_id,
                service_id: input.service_id,
                run_id: input.failed_run_id,
                model: input.model,
                checkout_revision: config.checkout_revision.clone(),
                job_namespace: config.namespace.clone(),
                deadline,
                model_budget: config.model_budget,
                tool_budget: config.tool_budget,
                service_context: json!({
                    "service": service.name,
                    "namespace": service.namespace,
                    "deployment": deployment,
                    "requested_deployment": input.deployment,
                    "model_service": model.name,
                    "model_namespace": model.namespace
                }),
                failing_evidence: evidence,
            },
        )
        .await
        .map_err(store_error)?;
    let seed_accepted = store
        .seed_accepted_in_transaction(guard.transaction(), &identity, session.id)
        .await
        .map_err(store_error)?;
    guard
        .audit(None, "debug.create", &session.id.to_string())
        .await?;
    guard.commit().await?;
    Ok(Json(DebugSessionResponse::from_session(
        session,
        seed_accepted,
    )))
}

fn proxy_error(error: ProxyError) -> ApiError {
    match error {
        ProxyError::InvalidMessage
        | ProxyError::InvalidRequestKey
        | ProxyError::Host(thalamus_debug::proxy::HostError::InvalidInput) => {
            ApiError::new(StatusCode::BAD_REQUEST, "invalid debug reply")
        }
        ProxyError::PromptIndeterminate => ApiError::new(
            StatusCode::CONFLICT,
            "reply outcome uncertain; do not resend automatically",
        ),
        ProxyError::BudgetExhausted
        | ProxyError::NotInitialized
        | ProxyError::AlreadyInitialized
        | ProxyError::DeadlineExceeded
        | ProxyError::InitializationIndeterminate { .. } => {
            ApiError::new(StatusCode::CONFLICT, "debug session state conflict")
        }
        ProxyError::Host(
            thalamus_debug::proxy::HostError::Unauthorized
            | thalamus_debug::proxy::HostError::NotFound,
        ) => not_found(),
        ProxyError::Host(
            thalamus_debug::proxy::HostError::Conflict
            | thalamus_debug::proxy::HostError::BudgetExhausted
            | thalamus_debug::proxy::HostError::InitializationBusy,
        ) => ApiError::new(StatusCode::CONFLICT, "debug session state conflict"),
        _ => ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "debug proxy unavailable"),
    }
}

#[utoipa::path(post, path = "/api/v1/debug/sessions/{id}/reply",
    params(("id" = Uuid, Path), ("team_id" = Uuid, Query)), request_body = DebugReplyRequest,
    responses((status = 204, description = "Reply accepted"), (status = 400, description = "Invalid reply"),
              (status = 404, description = "Session not found"), (status = 409, description = "Session or reply conflict"),
              (status = 503, description = "Debug proxy unavailable")))]
pub(crate) async fn reply(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<crate::RequestPrincipal>,
    Path(id): Path<Uuid>,
    Query(query): Query<SessionQuery>,
    Json(input): Json<DebugReplyRequest>,
) -> Result<StatusCode, ApiError> {
    let session = session_for(&state, &caller.principal(), query.team_id, id).await?;
    let proxy = state
        .debug_proxy
        .as_ref()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "debug proxy unavailable"))?;
    let actor = Actor {
        user_id: session.user_id,
        team_id: session.team_id,
    };
    proxy
        .send_reply(&actor, id, &input.client_request_key, &input.text)
        .await
        .map_err(proxy_error)?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(delete, path = "/api/v1/debug/sessions/{id}",
    params(("id" = Uuid, Path), ("team_id" = Uuid, Query)),
    responses((status = 204, description = "Cancellation requested; cleanup may remain pending"),
              (status = 404, description = "Session not found"), (status = 409, description = "Session already terminal")))]
pub(crate) async fn cancel_session(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<crate::RequestPrincipal>,
    Path(id): Path<Uuid>,
    Query(query): Query<SessionQuery>,
) -> Result<StatusCode, ApiError> {
    let checked = authorized_session(&state, &caller.principal(), query.team_id, id).await?;
    let mut guard = state
        .identity
        .begin_authorized(caller.principal(), query.team_id, false)
        .await?;
    let identity = DebugIdentity {
        user_id: guard.actor_id(),
        team_id: query.team_id,
    };
    let store = DebugStore::new(state.db.clone());
    let session = store
        .get_in_transaction(guard.transaction(), &identity, id)
        .await
        .map_err(store_error)?;
    if session.service_id != checked.service_id {
        return Err(not_found());
    }
    if session.state == SessionState::Cancelled {
        guard.commit().await?;
        return Ok(StatusCode::NO_CONTENT);
    }
    if session.state.is_terminal() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "debug session is already terminal",
        ));
    }
    store
        .transition_in_transaction(
            guard.transaction(),
            &identity,
            id,
            session.state,
            SessionState::Cancelled,
            Some(json!({"reason": "cancelled_by_owner"})),
        )
        .await
        .map_err(store_error)?;
    guard.audit(None, "debug.cancel", &id.to_string()).await?;
    guard.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(get, path = "/api/v1/debug/sessions/{id}",
    params(("id" = Uuid, Path), ("team_id" = Uuid, Query)),
    responses((status = 200, body = DebugSessionResponse), (status = 404, description = "Session not found")))]
pub(crate) async fn get_session(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<crate::RequestPrincipal>,
    Path(id): Path<Uuid>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<DebugSessionResponse>, ApiError> {
    let checked = authorized_session(&state, &caller.principal(), query.team_id, id).await?;
    let mut guard = state
        .identity
        .begin_authorized(caller.principal(), query.team_id, false)
        .await?;
    let identity = DebugIdentity {
        user_id: guard.actor_id(),
        team_id: query.team_id,
    };
    let store = DebugStore::new(state.db.clone());
    let session = store
        .get_in_transaction(guard.transaction(), &identity, id)
        .await
        .map_err(store_error)?;
    if session.service_id != checked.service_id {
        return Err(not_found());
    }
    let seed_accepted = store
        .seed_accepted_in_transaction(guard.transaction(), &identity, id)
        .await
        .map_err(store_error)?;
    guard.commit().await?;
    Ok(Json(DebugSessionResponse::from_session(
        session,
        seed_accepted,
    )))
}

#[utoipa::path(get, path = "/api/v1/debug/sessions/{id}/events",
    params(("id" = Uuid, Path), ("team_id" = Uuid, Query),
           ("after" = Option<i64>, Query), ("limit" = Option<i64>, Query)),
    responses((status = 200, body = [DebugEventResponse]), (status = 404, description = "Session not found")))]
pub(crate) async fn get_events(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<crate::RequestPrincipal>,
    Path(id): Path<Uuid>,
    Query(query): Query<EventsQuery>,
) -> Result<Json<Vec<DebugEventResponse>>, ApiError> {
    events_for(
        &state,
        &caller.principal(),
        query.team_id,
        id,
        query.after,
        query.limit,
    )
    .await
    .map(|events| Json(events.into_iter().map(Into::into).collect()))
}
