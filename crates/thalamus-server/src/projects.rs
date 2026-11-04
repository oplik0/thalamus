//! Team-scoped project and API key operations for the browser.
use std::sync::Arc;

use axum::{
    extract::{rejection::JsonRejection, rejection::PathRejection, Path, State},
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
    Extension, Json, Router,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;
use thalamus_identity::{
    IdentityService, KeyKind, KeyMetadata, NewKey, Project, Team, VerifiedPrincipal,
};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{ApiError, AppState, RequestPrincipal};

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/teams", get(list_teams))
        .route("/projects", get(list_projects).post(create_project))
        .route(
            "/projects/{id}/traffic-classes",
            get(project_traffic_classes),
        )
        .route(
            "/projects/{id}/traffic-class",
            axum::routing::put(set_project_traffic_class),
        )
        .route("/keys", get(list_keys).post(create_key))
        .route("/keys/{id}", axum::routing::delete(revoke_key))
        .route(
            "/keys/{id}/verify-revocation",
            axum::routing::post(verify_revocation),
        )
        .method_not_allowed_fallback(method_not_allowed)
}

async fn method_not_allowed() -> ApiError {
    ApiError::new(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
}
#[derive(Serialize, ToSchema)]
pub(crate) struct TeamView {
    id: Uuid,
    name: String,
    role: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct TeamsResponse {
    teams: Vec<TeamView>,
}

#[utoipa::path(get, path = "/api/v1/teams", responses((status = 200, body = TeamsResponse), (status = 401, description = "Authentication required"), (status = 503, description = "Identity service unavailable")))]
pub(crate) async fn list_teams(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<RequestPrincipal>,
) -> Result<Json<TeamsResponse>, ApiError> {
    let teams = state
        .identity
        .list_teams_principal(caller.principal())
        .await?
        .into_iter()
        .map(|team| TeamView {
            id: team.id,
            name: team.name,
            role: team.role,
        })
        .collect();
    Ok(Json(TeamsResponse { teams }))
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ProjectView {
    id: Uuid,
    name: String,
    team: String,
    budget: Option<f64>,
    cost_centre: Option<String>,
    traffic_class: String,
}

impl ProjectView {
    fn new(project: Project, team: &Team) -> Self {
        Self {
            id: project.id,
            name: project.name,
            team: team.name.clone(),
            budget: project.budget,
            cost_centre: project.cost_centre,
            traffic_class: project.traffic_class,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ProjectsResponse {
    projects: Vec<ProjectView>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct CreateProject {
    name: String,
    team: String,
    cost_centre: Option<String>,
    budget: Option<f64>,
}

#[utoipa::path(get, path = "/api/v1/projects", responses((status = 200, body = ProjectsResponse), (status = 401, description = "Authentication required"), (status = 503, description = "Identity service unavailable")))]
pub(crate) async fn list_projects(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<RequestPrincipal>,
) -> Result<Json<ProjectsResponse>, ApiError> {
    list_projects_for(&state.identity, &caller.principal())
        .await
        .map(Json)
}

fn principal<'a>(value: &'a VerifiedPrincipal<'_>) -> VerifiedPrincipal<'a> {
    match value {
        VerifiedPrincipal::Session(session) => VerifiedPrincipal::Session(session),
        VerifiedPrincipal::Oidc { issuer, subject } => VerifiedPrincipal::Oidc { issuer, subject },
    }
}

pub(crate) async fn list_projects_for(
    identity: &IdentityService,
    caller: &VerifiedPrincipal<'_>,
) -> Result<ProjectsResponse, ApiError> {
    let teams = identity.list_teams_principal(principal(caller)).await?;
    let mut projects = Vec::new();
    for team in teams {
        let team_projects = identity
            .list_projects_principal(principal(caller), team.id)
            .await?;
        projects.extend(
            team_projects
                .into_iter()
                .map(|project| ProjectView::new(project, &team)),
        );
    }
    Ok(ProjectsResponse { projects })
}

#[utoipa::path(post, path = "/api/v1/projects", request_body = CreateProject, responses((status = 201, body = ProjectView), (status = 400, description = "Invalid request"), (status = 401, description = "Authentication required"), (status = 403, description = "Permission denied"), (status = 404, description = "Team not found"), (status = 503, description = "Identity service unavailable")))]
pub(crate) async fn create_project(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<RequestPrincipal>,
    body: Result<Json<CreateProject>, JsonRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let Json(input) = body.map_err(json_error)?;
    create_project_for(&state.identity, &caller.principal(), input, None).await
}

pub(crate) async fn create_project_for(
    identity: &IdentityService,
    caller: &VerifiedPrincipal<'_>,
    input: CreateProject,
    agent: Option<&str>,
) -> Result<impl IntoResponse, ApiError> {
    let team = identity
        .list_teams_principal(principal(caller))
        .await?
        .into_iter()
        .find(|team| team.name == input.team)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "team not found"))?;
    let id = identity
        .create_project_principal(
            principal(caller),
            team.id,
            &input.name,
            input.cost_centre.as_deref(),
            input.budget,
            agent,
        )
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(ProjectView {
            id,
            name: input.name,
            team: team.name,
            budget: input.budget,
            cost_centre: input.cost_centre,
            traffic_class: "standard".into(),
        }),
    ))
}

#[derive(Serialize, ToSchema)]
pub(crate) struct KeyView {
    id: Uuid,
    name: String,
    team_id: Uuid,
    kind: String,
    project_id: Option<Uuid>,
    allowed_services: Vec<String>,
    expires_at: Option<DateTime<Utc>>,
    revocation_state: &'static str,
}

impl KeyView {
    fn from_metadata(key: KeyMetadata, now: DateTime<Utc>) -> Self {
        let revocation_state = if key.revoked_at.is_some() {
            // Verification is added only after a successful gateway probe.
            "revocation pending"
        } else if key
            .expires_at
            .as_ref()
            .is_some_and(|expires| expires <= &now)
        {
            "expired"
        } else {
            "not revoked"
        };
        Self {
            id: key.id,
            name: key.name,
            team_id: key.team_id,
            kind: key.kind,
            project_id: key.project_id,
            allowed_services: key.allowed_services,
            expires_at: key.expires_at,
            revocation_state,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct KeysResponse {
    keys: Vec<KeyView>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SelfServiceKeyKind {
    Personal,
    Project,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateKey {
    name: String,
    kind: SelfServiceKeyKind,
    team_id: Option<Uuid>,
    project_id: Option<Uuid>,
    allowed_services: Vec<String>,
    expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, PartialEq, Eq)]
enum KeyScope {
    Personal(Uuid),
    Project(Uuid),
}

fn key_scope(input: &CreateKey) -> Result<KeyScope, ApiError> {
    match (input.kind, input.team_id, input.project_id) {
        (SelfServiceKeyKind::Personal, Some(team), None) if !team.is_nil() => {
            Ok(KeyScope::Personal(team))
        }
        (SelfServiceKeyKind::Project, None, Some(project)) if !project.is_nil() => {
            Ok(KeyScope::Project(project))
        }
        _ => Err(ApiError::new(StatusCode::BAD_REQUEST, "invalid key scope")),
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct IssuedKeyView {
    key: KeyView,
    secret: String,
}

#[utoipa::path(get, path = "/api/v1/keys", responses((status = 200, body = KeysResponse), (status = 401, description = "Authentication required"), (status = 503, description = "Identity service unavailable")))]
pub(crate) async fn list_keys(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<RequestPrincipal>,
) -> Result<Json<KeysResponse>, ApiError> {
    list_keys_for(&state, &caller.principal()).await.map(Json)
}

pub(crate) async fn list_keys_for(
    state: &AppState,
    caller: &VerifiedPrincipal<'_>,
) -> Result<KeysResponse, ApiError> {
    let teams = state
        .identity
        .list_teams_principal(principal(caller))
        .await?;
    let mut keys = Vec::new();
    let now = Utc::now();
    for team in teams {
        let team_keys = state
            .identity
            .list_keys_principal(principal(caller), team.id)
            .await?;
        keys.extend(
            team_keys
                .into_iter()
                .map(|key| KeyView::from_metadata(key, now)),
        );
    }
    let worker_table: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('thalamus_worker_revocations')::text")
            .fetch_one(&state.db)
            .await?;
    if let (Some(_), Some(namespace), Some(name)) = (
        worker_table,
        state.config.gateway_projection_namespace.as_deref(),
        state.config.gateway_projection_name.as_deref(),
    ) {
        let ids: Vec<Uuid> = keys
            .iter()
            .filter(|key| key.revocation_state == "revocation pending")
            .map(|key| key.id)
            .collect();
        let verified: Vec<Uuid> = sqlx::query_scalar("SELECT key_id FROM thalamus_worker_revocations WHERE namespace=$1 AND name=$2 AND key_id = ANY($3) AND verified_at IS NOT NULL")
            .bind(namespace).bind(name).bind(&ids).fetch_all(&state.db).await?;
        for key in &mut keys {
            if verified.contains(&key.id) {
                key.revocation_state = "revocation verified";
            }
        }
    }
    Ok(KeysResponse { keys })
}

// Project IDs from the browser are not authority: resolve them through the
// identity service's authorized team and project listings before issuing.
async fn project_team(
    identity: &IdentityService,
    caller: &VerifiedPrincipal<'_>,
    project_id: Uuid,
) -> Result<Uuid, ApiError> {
    for team in identity.list_teams_principal(principal(caller)).await? {
        if identity
            .list_projects_principal(principal(caller), team.id)
            .await?
            .iter()
            .any(|project| project.id == project_id)
        {
            return Ok(team.id);
        }
    }
    Err(ApiError::new(StatusCode::NOT_FOUND, "project not found"))
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ProjectTrafficClasses {
    classes: Vec<String>,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetProjectTrafficClass {
    traffic_class: String,
}

fn class_policy_error(error: thalamus_identity::Error) -> ApiError {
    match error {
        thalamus_identity::Error::Invalid("budget cap unavailable or stale")
        | thalamus_identity::Error::Invalid("budget exceeds or lacks team cap") => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "reviewed class policy unavailable",
        ),
        other => other.into(),
    }
}

#[utoipa::path(get, path = "/api/v1/projects/{id}/traffic-classes", params(("id" = Uuid, Path)), responses((status = 200, body = ProjectTrafficClasses), (status = 400, description = "Invalid project id"), (status = 401, description = "Authentication required"), (status = 403, description = "Platform administrator required"), (status = 404, description = "Project not found"), (status = 503, description = "Reviewed policy unavailable")))]
pub(crate) async fn project_traffic_classes(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<RequestPrincipal>,
    path: Result<Path<Uuid>, PathRejection>,
) -> Result<Json<ProjectTrafficClasses>, ApiError> {
    let Path(id) =
        path.map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "invalid project id"))?;
    let caller = caller.principal();
    let team = project_team(&state.identity, &caller, id).await?;
    let classes = state
        .identity
        .allowed_project_traffic_classes_principal(principal(&caller), team, id)
        .await
        .map_err(class_policy_error)?;
    Ok(Json(ProjectTrafficClasses { classes }))
}

#[utoipa::path(put, path = "/api/v1/projects/{id}/traffic-class", params(("id" = Uuid, Path)), request_body = SetProjectTrafficClass, responses((status = 204, description = "Traffic class updated"), (status = 400, description = "Invalid class"), (status = 401, description = "Authentication required"), (status = 403, description = "Platform administrator and reviewed class required"), (status = 404, description = "Project not found"), (status = 503, description = "Reviewed policy unavailable")))]
pub(crate) async fn set_project_traffic_class(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<RequestPrincipal>,
    path: Result<Path<Uuid>, PathRejection>,
    body: Result<Json<SetProjectTrafficClass>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    let Path(id) =
        path.map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "invalid project id"))?;
    let Json(input) = body.map_err(json_error)?;
    let caller = caller.principal();
    let team = project_team(&state.identity, &caller, id).await?;
    state
        .identity
        .set_project_traffic_class_principal(
            principal(&caller),
            team,
            id,
            &input.traffic_class,
            None,
        )
        .await
        .map_err(class_policy_error)?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(post, path = "/api/v1/keys", request_body = CreateKey, responses((status = 201, body = IssuedKeyView), (status = 400, description = "Invalid key scope or request"), (status = 401, description = "Authentication required"), (status = 403, description = "Permission denied"), (status = 404, description = "Team, project or service not found"), (status = 503, description = "Identity service or committed access policy unavailable")))]
pub(crate) async fn create_key(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<RequestPrincipal>,
    body: Result<Json<CreateKey>, JsonRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let Json(input) = body.map_err(json_error)?;
    create_key_for(&state, &caller.principal(), input, None).await
}

pub(crate) async fn create_key_for(
    state: &AppState,
    caller: &VerifiedPrincipal<'_>,
    input: CreateKey,
    agent: Option<&str>,
) -> Result<impl IntoResponse, ApiError> {
    let identity = &state.identity;
    let (team_id, kind) = match key_scope(&input)? {
        KeyScope::Personal(team) => {
            let authorized = identity
                .list_teams_principal(principal(caller))
                .await?
                .into_iter()
                .any(|candidate| candidate.id == team);
            if !authorized {
                return Err(ApiError::new(StatusCode::NOT_FOUND, "team not found"));
            }
            (team, KeyKind::Personal)
        }
        KeyScope::Project(project) => (
            project_team(identity, caller, project).await?,
            KeyKind::Project,
        ),
    };
    if input.allowed_services.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "at least one allowed service is required",
        ));
    }
    let source = state.policy_source.clone().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "service access policy unavailable",
        )
    })?;
    let policy = tokio::task::spawn_blocking(move || source.load())
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "service access policy unavailable",
            )
        })?
        .map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "service access policy unavailable",
            )
        })?;
    authorize_key_services(&policy, team_id, &input.allowed_services)?;
    let mut guard = identity
        .begin_authorized(principal(caller), team_id, false)
        .await?;
    let issued = guard
        .issue_key(
            NewKey {
                name: input.name,
                team_id,
                project_id: input.project_id,
                kind,
                allowed_services: input.allowed_services,
                budget: None,
                expires_at: input.expires_at,
            },
            agent,
        )
        .await?;
    // Do not commit the key or its audit if the published policy moved while
    // the identity lock was acquired or issuance was in progress.
    policy.committed_revision().map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "service access policy unavailable",
        )
    })?;
    guard.commit().await?;
    let key = KeyView::from_metadata(issued.key, Utc::now());
    // A secret is returned only by this response and must not be cached.
    Ok((
        StatusCode::CREATED,
        [(header::CACHE_CONTROL, "no-store")],
        Json(IssuedKeyView {
            key,
            secret: issued.secret,
        }),
    ))
}

fn authorize_key_services(
    policy: &thalamus_access::Policy,
    team_id: Uuid,
    services: &[String],
) -> Result<(), ApiError> {
    for service in services {
        let id = policy
            .service_id(service)
            .map_err(|_| {
                ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "service access policy unavailable",
                )
            })?
            .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "service not found"))?;
        let allowed = policy.team_allowed(&id, team_id).map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "service access policy unavailable",
            )
        })?;
        if !allowed {
            return Err(ApiError::new(StatusCode::NOT_FOUND, "service not found"));
        }
    }
    Ok(())
}

#[utoipa::path(delete, path = "/api/v1/keys/{id}", params(("id" = Uuid, Path)), responses((status = 204, description = "Key revoked"), (status = 400, description = "Invalid key id"), (status = 401, description = "Authentication required"), (status = 403, description = "Permission denied"), (status = 404, description = "Key not found"), (status = 503, description = "Identity service unavailable")))]
pub(crate) async fn revoke_key(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<RequestPrincipal>,
    path: Result<Path<Uuid>, PathRejection>,
) -> Result<StatusCode, ApiError> {
    let Path(id) = path.map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "invalid key id"))?;
    revoke_key_for(&state.identity, &caller.principal(), id, None).await
}

pub(crate) async fn revoke_key_for(
    identity: &IdentityService,
    caller: &VerifiedPrincipal<'_>,
    id: Uuid,
    agent: Option<&str>,
) -> Result<StatusCode, ApiError> {
    for team in identity.list_teams_principal(principal(caller)).await? {
        if identity
            .list_keys_principal(principal(caller), team.id)
            .await?
            .iter()
            .any(|key| key.id == id)
        {
            identity
                .revoke_key_principal(principal(caller), team.id, id, agent)
                .await?;
            return Ok(StatusCode::NO_CONTENT);
        }
    }
    Err(ApiError::new(StatusCode::NOT_FOUND, "key not found"))
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct VerifyRevocation {
    key: String,
}

fn valid_key(value: &str) -> bool {
    value.len() == 47
        && value.starts_with("thl_")
        && value[4..]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn digest_matches(value: &str, stored: &[u8]) -> bool {
    let digest = Sha256::digest(value.as_bytes());
    stored.len() == digest.len()
        && digest
            .iter()
            .zip(stored)
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            == 0
}

fn gateway_probe_url(base: Option<&str>) -> Result<url::Url, ApiError> {
    let unavailable = || {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "trusted gateway unavailable",
        )
    };
    let mut url = url::Url::parse(base.ok_or_else(unavailable)?).map_err(|_| unavailable())?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path().trim_end_matches('/') != "/v1"
    {
        return Err(unavailable());
    }
    url.set_path("/v1/models");
    Ok(url)
}

// A bare 401 is ambiguous: it may come from a proxy rather than gateway key
// authentication. Accept only a specific invalid-token Bearer challenge.
fn rejected_key(status: StatusCode, headers: &HeaderMap) -> bool {
    status == StatusCode::UNAUTHORIZED
        && headers
            .get_all(header::WWW_AUTHENTICATE)
            .iter()
            .any(|value| value.to_str().ok() == Some("Bearer error=\"invalid_token\""))
}

#[utoipa::path(post, path = "/api/v1/keys/{id}/verify-revocation", params(("id" = Uuid, Path)), request_body = VerifyRevocation, responses((status = 204, description = "Revocation verified"), (status = 400, description = "Invalid request"), (status = 401, description = "Authentication required"), (status = 403, description = "Permission denied"), (status = 404, description = "Key not found"), (status = 409, description = "Revocation not confirmed"), (status = 503, description = "Verification unavailable")))]
pub(crate) async fn verify_revocation(
    State(state): State<Arc<AppState>>,
    Extension(request_principal): Extension<RequestPrincipal>,
    path: Result<Path<Uuid>, PathRejection>,
    body: Result<Json<VerifyRevocation>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    let Path(id) = path.map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "invalid key id"))?;
    let caller = request_principal.principal();
    let mut team_id = None;
    for team in state
        .identity
        .list_teams_principal(principal(&caller))
        .await?
    {
        if state
            .identity
            .list_keys_principal(principal(&caller), team.id)
            .await?
            .iter()
            .any(|key| key.id == id)
        {
            team_id = Some(team.id);
            break;
        }
    }
    let team = team_id.ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "key not found"))?;
    let mut guard = state
        .identity
        .begin_authorized(principal(&caller), team, false)
        .await?;
    let actor = guard.actor_id();
    let row = sqlx::query("SELECT digest,revoked_at FROM identity_keys WHERE id=$1 AND team_id=$2 AND (user_id=$3 OR EXISTS (SELECT 1 FROM identity_users u LEFT JOIN identity_memberships m ON m.user_id=u.id AND m.team_id=$2 WHERE u.id=$3 AND (u.platform_admin OR m.role='team_admin'))) FOR UPDATE")
        .bind(id).bind(team).bind(actor).fetch_optional(&mut **guard.transaction()).await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "key not found"))?;
    let revoked: Option<DateTime<Utc>> = row.try_get("revoked_at")?;
    if revoked.is_none() {
        return Err(ApiError::new(StatusCode::CONFLICT, "key is not revoked"));
    }
    let Json(input) = body.map_err(json_error)?;
    let digest: Vec<u8> = row.try_get("digest")?;
    if !valid_key(&input.key) || !digest_matches(&input.key, &digest) {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "invalid key"));
    }
    let namespace = state
        .config
        .gateway_projection_namespace
        .as_deref()
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "gateway projection unavailable",
            )
        })?;
    let name = state
        .config
        .gateway_projection_name
        .as_deref()
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "gateway projection unavailable",
            )
        })?;
    let projection = sqlx::query("SELECT 1 FROM thalamus_worker_revocations WHERE namespace=$1 AND name=$2 AND key_id=$3 AND removed_at >= $4 FOR UPDATE")
        .bind(namespace).bind(name).bind(id).bind(revoked).fetch_optional(&mut **guard.transaction()).await?;
    if projection.is_none() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "revocation projection not established",
        ));
    }
    let url = gateway_probe_url(state.config.upstream_url.as_deref())?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "trusted gateway unavailable",
            )
        })?;
    let response = client
        .get(url)
        .bearer_auth(&input.key)
        .send()
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "gateway verification failed",
            )
        })?;
    if !rejected_key(response.status(), response.headers()) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "gateway did not confirm key rejection",
        ));
    }
    sqlx::query("UPDATE thalamus_worker_revocations SET verified_at=now() WHERE namespace=$1 AND name=$2 AND key_id=$3")
        .bind(namespace).bind(name).bind(id).execute(&mut **guard.transaction()).await?;
    guard
        .audit(None, "key.revocation.verify", &id.to_string())
        .await?;
    guard.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

fn json_error(error: JsonRejection) -> ApiError {
    ApiError::new(error.status(), "invalid JSON request")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn key_metadata(project_id: Option<Uuid>) -> KeyMetadata {
        KeyMetadata {
            id: Uuid::new_v4(),
            name: "CI key".into(),
            user_id: Uuid::new_v4(),
            creator_id: Uuid::new_v4(),
            team_id: Uuid::new_v4(),
            project_id,
            kind: if project_id.is_some() {
                "project"
            } else {
                "personal"
            }
            .into(),
            allowed_services: vec!["chat".into(), "code".into()],
            budget: None,
            expires_at: None,
            revoked_at: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn project_response_matches_ui_contract() {
        let team = Team {
            id: Uuid::new_v4(),
            name: "research".into(),
            role: "member".into(),
        };
        let id = Uuid::new_v4();
        let project = Project {
            id,
            team_id: team.id,
            name: "benchmarks".into(),
            cost_centre: Some("research-42".into()),
            budget: Some(12.5),
            traffic_class: "standard".into(),
        };
        assert_eq!(
            serde_json::to_value(ProjectView::new(project, &team)).unwrap(),
            json!({
                "id": id, "name": "benchmarks", "team": "research", "budget": 12.5, "cost_centre": "research-42", "traffic_class": "standard"
            })
        );
        let input: CreateProject = serde_json::from_value(json!({
            "name": "benchmarks", "team": "research", "budget": null
        }))
        .unwrap();
        assert_eq!(input.team, "research");
        assert_eq!(input.budget, None);
        assert_eq!(input.cost_centre, None);
        let supplied: CreateProject = serde_json::from_value(json!({
            "name": "benchmarks", "team": "research", "budget": null, "cost_centre": "research-42"
        }))
        .unwrap();
        assert_eq!(supplied.cost_centre.as_deref(), Some("research-42"));
    }

    #[test]
    fn project_key_metadata_has_no_secret_or_hash() {
        let project_id = Uuid::new_v4();
        let metadata = key_metadata(Some(project_id));
        let id = metadata.id;
        let team_id = metadata.team_id;
        let key = KeyView::from_metadata(metadata, Utc::now());
        let value = serde_json::to_value(&key).unwrap();
        assert_eq!(
            value,
            json!({
                "id": id, "name": "CI key", "team_id": team_id, "kind": "project", "project_id": project_id,
                "allowed_services": ["chat", "code"], "expires_at": null,
                "revocation_state": "not revoked"
            })
        );
        assert!(value.get("secret").is_none());
        assert!(value.get("digest").is_none());
        assert!(value.get("hash").is_none());
        let issued = serde_json::to_value(IssuedKeyView {
            key,
            secret: "thl_once".into(),
        })
        .unwrap();
        assert_eq!(issued["secret"], "thl_once");
        assert!(issued["key"].get("secret").is_none());
        assert!(issued["key"].get("digest").is_none());
    }

    #[test]
    fn key_request_accepts_optional_expiry_and_preserves_names() {
        let project_id = Uuid::new_v4();
        let body: CreateKey = serde_json::from_value(json!({
            "name": "CI key", "kind": "project", "project_id": project_id,
            "allowed_services": ["chat"], "expires_at": null
        }))
        .unwrap();
        assert_eq!(body.name, "CI key");
        assert_eq!(body.project_id, Some(project_id));
        assert_eq!(key_scope(&body).unwrap(), KeyScope::Project(project_id));
        assert_eq!(body.allowed_services, vec!["chat"]);
        assert!(body.expires_at.is_none());
        let dated: CreateKey = serde_json::from_value(json!({
            "name": "dated", "kind": "project", "project_id": project_id,
            "allowed_services": ["chat"], "expires_at": "2030-01-01T00:00:00Z"
        }))
        .unwrap();
        assert!(dated.expires_at.is_some());
        let team_id = Uuid::new_v4();
        let personal: CreateKey = serde_json::from_value(json!({
            "name": "personal", "kind": "personal", "team_id": team_id,
            "project_id": null, "allowed_services": ["chat"], "expires_at": null
        }))
        .unwrap();
        assert_eq!(key_scope(&personal).unwrap(), KeyScope::Personal(team_id));
        for kind in ["job", "debug"] {
            assert!(serde_json::from_value::<CreateKey>(json!({
                "name": "forged", "kind": kind, "team_id": team_id,
                "allowed_services": ["chat"]
            }))
            .is_err());
        }
        let confused: CreateKey = serde_json::from_value(json!({
            "name": "confused", "kind": "personal", "team_id": team_id,
            "project_id": project_id, "allowed_services": ["chat"]
        }))
        .unwrap();
        assert!(key_scope(&confused).is_err());
        let unscoped: CreateKey = serde_json::from_value(json!({
            "name": "unscoped", "kind": "personal", "allowed_services": ["chat"]
        }))
        .unwrap();
        assert!(key_scope(&unscoped).is_err());
    }

    #[test]
    fn key_services_are_limited_to_the_selected_teams_committed_policy() {
        use std::{fs, path::PathBuf, process::Command};
        struct TempRepo(PathBuf);
        impl Drop for TempRepo {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let root =
            TempRepo(std::env::temp_dir().join(format!("thalamus-key-policy-{}", Uuid::new_v4())));
        fs::create_dir_all(root.0.join("catalog/services")).unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(&root.0)
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success(), "git fixture failed");
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "test@example.invalid"]);
        git(&["config", "user.name", "Test"]);
        let team = Uuid::new_v4();
        let other = Uuid::new_v4();
        let first = root.0.join("catalog/services/one.yaml");
        fs::write(
            &first,
            format!("name: one\nnamespace: models\naccess: {{teams: [{team}]}}\ndeployments: []\n"),
        )
        .unwrap();
        fs::write(
            root.0.join("catalog/services/two.yaml"),
            format!(
                "name: two\nnamespace: models\naccess: {{teams: [{other}]}}\ndeployments: []\n"
            ),
        )
        .unwrap();
        git(&["add", "catalog"]);
        git(&["commit", "-qm", "publish"]);
        let commit = git(&["rev-parse", "HEAD"]);
        let policy = thalamus_access::PolicySource::new(&root.0, "catalog", commit)
            .load()
            .unwrap();
        assert!(authorize_key_services(&policy, team, &["one".into()]).is_ok());
        assert!(authorize_key_services(&policy, team, &["two".into()]).is_err());
        assert!(authorize_key_services(&policy, team, &["one".into(), "two".into()]).is_err());
        assert!(authorize_key_services(&policy, team, &["missing".into()]).is_err());
        fs::write(
            first,
            format!(
                "name: one\nnamespace: models\naccess: {{teams: [{other}]}}\ndeployments: []\n"
            ),
        )
        .unwrap();
        git(&["add", "catalog"]);
        git(&["commit", "-qm", "advance"]);
        assert!(authorize_key_services(&policy, team, &["one".into()]).is_err());
    }

    #[test]
    fn revocation_is_pending_until_gateway_confirmation() {
        let now = Utc::now();
        let mut key = key_metadata(Some(Uuid::new_v4()));
        key.revoked_at = Some(now);
        key.expires_at = Some(now - chrono::Duration::seconds(1));
        assert_eq!(
            KeyView::from_metadata(key, now).revocation_state,
            "revocation pending"
        );
        let mut key = key_metadata(Some(Uuid::new_v4()));
        key.expires_at = Some(now - chrono::Duration::seconds(1));
        assert_eq!(KeyView::from_metadata(key, now).revocation_state, "expired");
        let personal = KeyView::from_metadata(key_metadata(None), now);
        assert!(personal.project_id.is_none());
        assert_eq!(personal.kind, "personal");
    }

    #[test]
    fn verification_requires_the_exact_well_formed_secret() {
        let key = format!("thl_{}", "A".repeat(43));
        let hash = Sha256::digest(key.as_bytes());
        assert!(valid_key(&key));
        assert!(digest_matches(&key, &hash));
        assert!(!digest_matches(&format!("thl_{}", "B".repeat(43)), &hash));
        assert!(!digest_matches(&key, &hash[..31]));
        assert!(!valid_key("thl_short"));
        assert!(!valid_key(&format!("thl_{}!", "A".repeat(42))));
        assert!(
            serde_json::from_value::<VerifyRevocation>(json!({"key": key, "other": "x"})).is_err()
        );
    }

    #[test]
    fn verification_fails_closed_for_ambiguous_gateway_responses() {
        let mut headers = HeaderMap::new();
        assert!(!rejected_key(StatusCode::UNAUTHORIZED, &headers));
        headers.insert(
            header::WWW_AUTHENTICATE,
            "Bearer error=\"invalid_token\"".parse().unwrap(),
        );
        assert!(rejected_key(StatusCode::UNAUTHORIZED, &headers));
        assert!(!rejected_key(StatusCode::FORBIDDEN, &headers));
        assert!(!rejected_key(StatusCode::OK, &headers));
        headers.insert(
            header::WWW_AUTHENTICATE,
            "Basic realm=\"proxy\"".parse().unwrap(),
        );
        assert!(!rejected_key(StatusCode::UNAUTHORIZED, &headers));
    }

    #[test]
    fn probe_target_is_only_the_configured_https_gateway() {
        assert_eq!(
            gateway_probe_url(Some("https://gateway.example/v1"))
                .unwrap()
                .as_str(),
            "https://gateway.example/v1/models"
        );
        for url in [
            "http://gateway.example/v1",
            "https://user@gateway.example/v1",
            "https://gateway.example/v1?x=1",
            "https://gateway.example/other",
            "https://gateway.example/v1#fragment",
        ] {
            assert!(gateway_probe_url(Some(url)).is_err());
        }
        assert!(gateway_probe_url(None).is_err());
    }

    #[test]
    fn malformed_json_fields_are_rejected() {
        let project_id = Uuid::new_v4();
        for expires_at in [json!("not a date"), json!(42)] {
            let result = serde_json::from_value::<CreateKey>(json!({
                "name": "key", "kind": "project", "project_id": project_id,
                "allowed_services": ["chat"], "expires_at": expires_at
            }));
            assert!(result.is_err());
        }
        assert!(serde_json::from_value::<CreateProject>(json!({
            "name": "project", "team": "team", "budget": Value::String("twelve".into())
        }))
        .is_err());
    }
}
