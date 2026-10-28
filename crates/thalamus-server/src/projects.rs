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
        .route("/projects", get(list_projects).post(create_project))
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
pub(crate) struct ProjectView {
    id: Uuid,
    name: String,
    team: String,
    budget: Option<f64>,
}

impl ProjectView {
    fn new(project: Project, team: &Team) -> Self {
        Self {
            id: project.id,
            name: project.name,
            team: team.name.clone(),
            budget: project.budget,
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
            None,
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
        }),
    ))
}

#[derive(Serialize, ToSchema)]
pub(crate) struct KeyView {
    id: Uuid,
    name: String,
    project_id: Uuid,
    allowed_services: Vec<String>,
    expires_at: Option<DateTime<Utc>>,
    revocation_state: &'static str,
}

impl KeyView {
    // Personal keys do not have a project and cannot be represented by the
    // project-key UI. The identity service still enforces their ownership.
    fn from_metadata(key: KeyMetadata, now: DateTime<Utc>) -> Option<Self> {
        let project_id = key.project_id?;
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
        Some(Self {
            id: key.id,
            name: key.name,
            project_id,
            allowed_services: key.allowed_services,
            expires_at: key.expires_at,
            revocation_state,
        })
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct KeysResponse {
    keys: Vec<KeyView>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct CreateKey {
    name: String,
    project_id: Uuid,
    allowed_services: Vec<String>,
    expires_at: Option<DateTime<Utc>>,
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
                .filter_map(|key| KeyView::from_metadata(key, now)),
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

#[utoipa::path(post, path = "/api/v1/keys", request_body = CreateKey, responses((status = 201, body = IssuedKeyView), (status = 400, description = "Invalid request"), (status = 401, description = "Authentication required"), (status = 403, description = "Permission denied"), (status = 404, description = "Project not found"), (status = 503, description = "Identity service unavailable")))]
pub(crate) async fn create_key(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<RequestPrincipal>,
    body: Result<Json<CreateKey>, JsonRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let Json(input) = body.map_err(json_error)?;
    create_key_for(&state.identity, &caller.principal(), input, None).await
}

pub(crate) async fn create_key_for(
    identity: &IdentityService,
    caller: &VerifiedPrincipal<'_>,
    input: CreateKey,
    agent: Option<&str>,
) -> Result<impl IntoResponse, ApiError> {
    let team_id = project_team(identity, caller, input.project_id).await?;
    if input.allowed_services.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "at least one allowed service is required",
        ));
    }
    let issued = identity
        .issue_key_principal(
            principal(caller),
            NewKey {
                name: input.name,
                team_id,
                project_id: Some(input.project_id),
                kind: KeyKind::Project,
                allowed_services: input.allowed_services,
                budget: None,
                expires_at: input.expires_at,
            },
            agent,
        )
        .await?;
    let key = KeyView::from_metadata(issued.key, Utc::now()).ok_or_else(|| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "issued key has no project",
        )
    })?;
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
            kind: "project".into(),
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
            cost_centre: None,
            budget: Some(12.5),
        };
        assert_eq!(
            serde_json::to_value(ProjectView::new(project, &team)).unwrap(),
            json!({
                "id": id, "name": "benchmarks", "team": "research", "budget": 12.5
            })
        );
        let input: CreateProject = serde_json::from_value(json!({
            "name": "benchmarks", "team": "research", "budget": null
        }))
        .unwrap();
        assert_eq!(input.team, "research");
        assert_eq!(input.budget, None);
    }

    #[test]
    fn project_key_metadata_has_no_secret_or_hash() {
        let project_id = Uuid::new_v4();
        let metadata = key_metadata(Some(project_id));
        let id = metadata.id;
        let key = KeyView::from_metadata(metadata, Utc::now()).unwrap();
        let value = serde_json::to_value(&key).unwrap();
        assert_eq!(
            value,
            json!({
                "id": id, "name": "CI key", "project_id": project_id,
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
            "name": "CI key", "project_id": project_id,
            "allowed_services": ["chat"], "expires_at": null
        }))
        .unwrap();
        assert_eq!(body.name, "CI key");
        assert_eq!(body.project_id, project_id);
        assert_eq!(body.allowed_services, vec!["chat"]);
        assert!(body.expires_at.is_none());
        let dated: CreateKey = serde_json::from_value(json!({
            "name": "dated", "project_id": project_id,
            "allowed_services": ["chat"], "expires_at": "2030-01-01T00:00:00Z"
        }))
        .unwrap();
        assert!(dated.expires_at.is_some());
    }

    #[test]
    fn revocation_is_pending_until_gateway_confirmation() {
        let now = Utc::now();
        let mut key = key_metadata(Some(Uuid::new_v4()));
        key.revoked_at = Some(now);
        key.expires_at = Some(now - chrono::Duration::seconds(1));
        assert_eq!(
            KeyView::from_metadata(key, now).unwrap().revocation_state,
            "revocation pending"
        );
        let mut key = key_metadata(Some(Uuid::new_v4()));
        key.expires_at = Some(now - chrono::Duration::seconds(1));
        assert_eq!(
            KeyView::from_metadata(key, now).unwrap().revocation_state,
            "expired"
        );
        assert!(KeyView::from_metadata(key_metadata(None), now).is_none());
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
                "name": "key", "project_id": project_id,
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
