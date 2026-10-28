//! Browser-facing control plane. Secrets and authority remain in the identity service.
//!
//! API bearer authentication is enabled by `THALAMUS_API_INTROSPECTION_ENDPOINT`,
//! `THALAMUS_API_CLIENT_ID`, `THALAMUS_API_CLIENT_SECRET`, and
//! `THALAMUS_API_AUDIENCE`; `THALAMUS_API_REQUIRED_SCOPES` optionally specifies
//! space-separated required scopes. The audience must be issued specifically for
//! this API, not the separate MCP resource. Bearer subjects must already have been
//! provisioned through browser OIDC login and retain only their current local roles.
use axum::{
    extract::{Request, State},
    http::{header, HeaderMap, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use utoipa::{OpenApi, ToSchema};
use uuid::Uuid;

mod auth;
pub mod capability_evidence;
mod catalog;
pub mod client_config;
pub mod debug;
pub mod inspection;
mod mcp;
mod projects;
mod spec;
pub mod usage;
pub mod workflows;

#[derive(Clone)]
pub struct McpConfig {
    transport: thalamus_mcp::Config,
    verifier: Arc<thalamus_oidc_resource::ResourceVerifier>,
}

#[derive(Clone)]
pub struct Config {
    pub public_origin: String,
    pub database_url: String,
    pub catalog_dir: PathBuf,
    pub prometheus_url: String,
    pub oidc_issuer: String,
    pub oidc_client_id: String,
    pub oidc_client_secret: String,
    pub cookie_signing_key: Vec<u8>,
    pub ui_dir: PathBuf,
    pub upstream_url: Option<String>,
    pub gateway_projection_namespace: Option<String>,
    pub gateway_projection_name: Option<String>,
    pub bind: String,
    pub groups: thalamus_identity::GroupMapping,
    pub workflow: Option<workflows::WorkflowConfig>,
    pub inspection: Option<inspection::InspectionConfig>,
    pub policy_source: Option<Arc<thalamus_access::PolicySource>>,
    pub mcp: Option<McpConfig>,
    /// Optional, independently configured OIDC resource server for the HTTP API.
    pub api_verifier: Option<Arc<thalamus_oidc_resource::ResourceVerifier>>,
}

#[derive(Deserialize, Default)]
struct Groups {
    platform_admin_group: Option<String>,
    #[serde(default)]
    team_admin_groups: HashMap<String, Uuid>,
    #[serde(default)]
    member_groups: HashMap<String, Uuid>,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        fn required(name: &str) -> Result<String, String> {
            std::env::var(name)
                .ok()
                .filter(|v| !v.is_empty())
                .ok_or_else(|| format!("{name} is required"))
        }
        let public_origin = required("THALAMUS_PUBLIC_ORIGIN")?;
        let parsed = url::Url::parse(&public_origin).map_err(|_| "invalid public origin")?;
        if parsed.scheme() != "https"
            || parsed.host_str().is_none()
            || parsed.username() != ""
            || parsed.password().is_some()
            || parsed.path() != "/"
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || parsed.origin().ascii_serialization() != public_origin
        {
            return Err("THALAMUS_PUBLIC_ORIGIN must be a canonical HTTPS origin without a path or trailing slash".into());
        }
        let key = required("THALAMUS_SESSION_SIGNING_KEY")?.into_bytes();
        if key.len() < 32 {
            return Err("THALAMUS_SESSION_SIGNING_KEY must contain at least 32 bytes".into());
        }
        let groups: Groups = match std::env::var("THALAMUS_GROUP_MAPPING") {
            Ok(s) => serde_json::from_str(&s)
                .map_err(|e| format!("invalid THALAMUS_GROUP_MAPPING: {e}"))?,
            Err(_) => Groups::default(),
        };
        let issuer = required("THALAMUS_OIDC_ISSUER")?;
        let mcp = match std::env::var("THALAMUS_MCP_INTROSPECTION_ENDPOINT") {
            Ok(endpoint) => {
                let resource = format!("{public_origin}/mcp");
                let transport = thalamus_mcp::Config::new(&resource, &issuer)
                    .map_err(|e| format!("invalid MCP configuration: {e}"))?;
                let scopes = match std::env::var("THALAMUS_MCP_REQUIRED_SCOPES") {
                    Ok(value) => value,
                    Err(std::env::VarError::NotPresent) => String::new(),
                    Err(_) => return Err("invalid THALAMUS_MCP_REQUIRED_SCOPES".into()),
                };
                let verifier = thalamus_oidc_resource::ResourceVerifier::new(
                    thalamus_oidc_resource::ResourceConfig {
                        introspection_endpoint: endpoint,
                        client_id: required("THALAMUS_MCP_CLIENT_ID")?,
                        client_secret: required("THALAMUS_MCP_CLIENT_SECRET")?,
                        issuer: issuer.clone(),
                        audience: resource,
                        required_scopes: scopes.split_whitespace().map(str::to_owned).collect(),
                    },
                )
                .map_err(|_| "invalid MCP introspection configuration".to_owned())?;
                Some(McpConfig {
                    transport,
                    verifier: Arc::new(verifier),
                })
            }
            Err(std::env::VarError::NotPresent) => {
                if [
                    "THALAMUS_MCP_CLIENT_ID",
                    "THALAMUS_MCP_CLIENT_SECRET",
                    "THALAMUS_MCP_REQUIRED_SCOPES",
                ]
                .iter()
                .any(|name| std::env::var_os(name).is_some())
                {
                    return Err(
                        "THALAMUS_MCP_INTROSPECTION_ENDPOINT is required when MCP is configured"
                            .into(),
                    );
                }
                None
            }
            Err(_) => return Err("invalid THALAMUS_MCP_INTROSPECTION_ENDPOINT".into()),
        };
        // API and MCP tokens are distinct resources; never infer one audience from the other.
        let api_verifier = match std::env::var("THALAMUS_API_INTROSPECTION_ENDPOINT") {
            Ok(endpoint) => {
                let scopes = match std::env::var("THALAMUS_API_REQUIRED_SCOPES") {
                    Ok(value) => value,
                    Err(std::env::VarError::NotPresent) => String::new(),
                    Err(_) => return Err("invalid THALAMUS_API_REQUIRED_SCOPES".into()),
                };
                Some(Arc::new(
                    thalamus_oidc_resource::ResourceVerifier::new(
                        thalamus_oidc_resource::ResourceConfig {
                            introspection_endpoint: endpoint,
                            client_id: required("THALAMUS_API_CLIENT_ID")?,
                            client_secret: required("THALAMUS_API_CLIENT_SECRET")?,
                            issuer: issuer.clone(),
                            audience: required("THALAMUS_API_AUDIENCE")?,
                            required_scopes: scopes.split_whitespace().map(str::to_owned).collect(),
                        },
                    )
                    .map_err(|_| "invalid API introspection configuration".to_owned())?,
                ))
            }
            Err(std::env::VarError::NotPresent) => {
                if [
                    "THALAMUS_API_CLIENT_ID",
                    "THALAMUS_API_CLIENT_SECRET",
                    "THALAMUS_API_AUDIENCE",
                    "THALAMUS_API_REQUIRED_SCOPES",
                ]
                .iter()
                .any(|name| std::env::var_os(name).is_some())
                {
                    return Err("THALAMUS_API_INTROSPECTION_ENDPOINT is required when API bearer authentication is configured".into());
                }
                None
            }
            Err(_) => return Err("invalid THALAMUS_API_INTROSPECTION_ENDPOINT".into()),
        };
        let policy_source = if [
            "THALAMUS_ACCESS_REPOSITORY",
            "THALAMUS_ACCESS_CATALOG_PATH",
            "THALAMUS_ACCESS_COMMIT",
        ]
        .iter()
        .any(|name| std::env::var_os(name).is_some())
        {
            Some(Arc::new(thalamus_access::PolicySource::new(
                required("THALAMUS_ACCESS_REPOSITORY")?,
                required("THALAMUS_ACCESS_CATALOG_PATH")?,
                required("THALAMUS_ACCESS_COMMIT")?,
            )))
        } else {
            None
        };
        Ok(Self {
            public_origin,
            database_url: required("DATABASE_URL")?,
            catalog_dir: required("THALAMUS_CATALOG_DIR")?.into(),
            prometheus_url: required("THALAMUS_PROMETHEUS_URL")?,
            oidc_issuer: issuer,
            oidc_client_id: required("THALAMUS_OIDC_CLIENT_ID")?,
            oidc_client_secret: required("THALAMUS_OIDC_CLIENT_SECRET")?,
            cookie_signing_key: key,
            ui_dir: std::env::var("THALAMUS_UI_DIR")
                .unwrap_or_else(|_| "ui/build".into())
                .into(),
            upstream_url: std::env::var("THALAMUS_GATEWAY_URL")
                .ok()
                .filter(|s| !s.is_empty()),
            gateway_projection_namespace: std::env::var("THALAMUS_K8S_NAMESPACE")
                .ok()
                .filter(|s| !s.is_empty()),
            gateway_projection_name: std::env::var("THALAMUS_GATEWAY_CONFIGMAP")
                .ok()
                .filter(|s| !s.is_empty()),
            bind: std::env::var("THALAMUS_BIND").unwrap_or_else(|_| "127.0.0.1:8080".into()),
            groups: thalamus_identity::GroupMapping {
                platform_admin_group: groups.platform_admin_group,
                team_admin_groups: groups.team_admin_groups,
                member_groups: groups.member_groups,
            },
            workflow: workflows::WorkflowConfig::from_env()?,
            inspection: inspection::InspectionConfig::from_env()?,
            policy_source,
            mcp,
            api_verifier,
        })
    }
}

pub struct AppState {
    pub config: Config,
    pub db: PgPool,
    pub identity: thalamus_identity::IdentityService,
    pub capability_evidence: Option<Arc<dyn client_config::CapabilityEvidenceProvider>>,
    pub policy_source: Option<Arc<thalamus_access::PolicySource>>,
}

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}
impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}
impl From<thalamus_identity::Error> for ApiError {
    fn from(error: thalamus_identity::Error) -> Self {
        use thalamus_identity::Error::*;
        match error {
            Unauthorized => Self::new(StatusCode::UNAUTHORIZED, "authentication required"),
            Forbidden => Self::new(StatusCode::FORBIDDEN, "permission denied"),
            NotFound => Self::new(StatusCode::NOT_FOUND, "not found"),
            Invalid(message) => Self::new(StatusCode::BAD_REQUEST, message),
            Database(_) | Oidc(_) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "identity service unavailable",
            ),
        }
    }
}
impl From<sqlx::Error> for ApiError {
    fn from(_: sqlx::Error) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "database unavailable")
    }
}
#[derive(Serialize, ToSchema)]
pub(crate) struct ErrorBody {
    message: String,
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                message: self.message,
            }),
        )
            .into_response()
    }
}

/// Reject duplicate session cookies rather than allowing ambiguous authentication.
fn session_cookie(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    let mut found = None;
    for value in headers.get_all(header::COOKIE) {
        let value = value
            .to_str()
            .map_err(|_| ApiError::new(StatusCode::UNAUTHORIZED, "invalid cookie"))?;
        for pair in value.split(';') {
            if let Some((name, token)) = pair.trim().split_once('=') {
                if name == "__Host-thalamus-session" {
                    if found.is_some() || token.is_empty() {
                        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "invalid cookie"));
                    }
                    found = Some(token.to_owned());
                }
            }
        }
    }
    Ok(found)
}

pub fn session(headers: &HeaderMap) -> Result<String, ApiError> {
    if headers.contains_key(header::AUTHORIZATION) {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "ambiguous credentials",
        ));
    }
    session_cookie(headers)?
        .ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "authentication required"))
}

#[derive(Clone)]
pub(crate) enum RequestPrincipal {
    Session(String),
    Oidc(thalamus_oidc_resource::AuthenticatedSubject),
}

impl RequestPrincipal {
    pub(crate) fn principal(&self) -> thalamus_identity::VerifiedPrincipal<'_> {
        match self {
            Self::Session(token) => thalamus_identity::VerifiedPrincipal::Session(token),
            Self::Oidc(subject) => thalamus_identity::VerifiedPrincipal::Oidc {
                issuer: &subject.issuer,
                subject: &subject.subject,
            },
        }
    }
}

enum Credentials<'a> {
    Session(String),
    Bearer(&'a str),
}

fn credentials(headers: &HeaderMap) -> Result<Credentials<'_>, ApiError> {
    let cookie = session_cookie(headers)?;
    if headers.contains_key(header::AUTHORIZATION) {
        if cookie.is_some() {
            return Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                "ambiguous credentials",
            ));
        }
        let authorization = headers.get_all(header::AUTHORIZATION);
        let mut values = authorization.iter();
        let value = values
            .next()
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "invalid credentials"))?;
        if values.next().is_some() {
            return Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                "invalid credentials",
            ));
        }
        let token = thalamus_oidc_resource::bearer_token([value])
            .map_err(|_| ApiError::new(StatusCode::UNAUTHORIZED, "invalid credentials"))?;
        Ok(Credentials::Bearer(token))
    } else {
        Ok(Credentials::Session(cookie.ok_or_else(|| {
            ApiError::new(StatusCode::UNAUTHORIZED, "authentication required")
        })?))
    }
}

/// Verify bearer credentials at the resource boundary; local authorization is always
/// resolved from the provisioned identity and its current roles by the service methods.
pub(crate) async fn authenticate_request(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<RequestPrincipal, ApiError> {
    match credentials(headers)? {
        Credentials::Bearer(token) => {
            let verifier = state.config.api_verifier.as_ref().ok_or_else(|| {
                ApiError::new(StatusCode::UNAUTHORIZED, "authentication required")
            })?;
            let subject = verifier
                .verify(token, &verifier.metadata().resource)
                .await
                .map_err(|error| match error {
                    thalamus_oidc_resource::VerifyError::Unavailable
                    | thalamus_oidc_resource::VerifyError::Configuration => ApiError::new(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "identity service unavailable",
                    ),
                    thalamus_oidc_resource::VerifyError::Unauthorized => {
                        ApiError::new(StatusCode::UNAUTHORIZED, "invalid credentials")
                    }
                })?;
            Ok(RequestPrincipal::Oidc(subject))
        }
        Credentials::Session(cookie) => Ok(RequestPrincipal::Session(cookie)),
    }
}

async fn origin_guard(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let api = request.uri().path().starts_with("/api/v1/");
    let unsafe_method = !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    );
    let origins = request.headers().get_all(header::ORIGIN);
    let mut values = origins.iter();
    let allowed_origin = values.next().and_then(|v| v.to_str().ok())
        == Some(state.config.public_origin.as_str())
        && values.next().is_none();
    if unsafe_method
        && !allowed_origin
        && !(api && request.headers().contains_key(header::AUTHORIZATION))
    {
        return ApiError::new(StatusCode::FORBIDDEN, "origin rejected").into_response();
    }
    if api {
        let caller = match authenticate_request(&state, request.headers()).await {
            Ok(caller) => caller,
            Err(error) => return error.into_response(),
        };
        if unsafe_method && !allowed_origin {
            if !matches!(&caller, RequestPrincipal::Oidc(_)) {
                return ApiError::new(StatusCode::FORBIDDEN, "origin rejected").into_response();
            }
            if let Err(error) = state
                .identity
                .authenticate_principal(caller.principal())
                .await
            {
                return ApiError::from(error).into_response();
            }
        }
        // This request-scoped extension is populated only after credential verification;
        // application services still load and authorize current local roles.
        request.extensions_mut().insert(caller);
    }
    next.run(request).await
}

#[derive(Serialize, ToSchema)]
struct Health {
    status: &'static str,
}
#[utoipa::path(get, path = "/health", responses((status = 200, description = "Process is serving requests", body = Health)))]
async fn health() -> Json<Health> {
    Json(Health { status: "ok" })
}
/// The same document is served over HTTP and available for offline generation.
pub fn openapi_document() -> utoipa::openapi::OpenApi {
    spec::ApiDoc::openapi()
}
#[utoipa::path(get, path = "/openapi.json", responses((status = 200, description = "OpenAPI document", content_type = "application/json")))]
async fn openapi() -> Json<utoipa::openapi::OpenApi> {
    Json(openapi_document())
}
async fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "route not found")
}

pub fn router(state: Arc<AppState>) -> Router {
    let api = Router::new()
        .merge(catalog::router())
        .merge(projects::router())
        .merge(client_config::router())
        .merge(workflows::router())
        .merge(inspection::router())
        .merge(usage::router())
        .merge(debug::router())
        .fallback(not_found);
    let index = state.config.ui_dir.join("index.html");
    let static_files = tower_http::services::ServeDir::new(&state.config.ui_dir)
        .not_found_service(tower_http::services::ServeFile::new(index));
    let browser = Router::new()
        .route("/health", get(health))
        .route("/openapi.json", get(openapi))
        .merge(auth::router())
        .nest("/api/v1", api)
        .fallback_service(static_files)
        .layer(middleware::from_fn_with_state(state.clone(), origin_guard))
        .with_state(state.clone());
    match &state.config.mcp {
        Some(config) => browser.merge(thalamus_mcp::router(
            config.transport.clone(),
            Arc::new(mcp::McpControlPlane::new(
                state.clone(),
                config.verifier.clone(),
            )),
        )),
        None => browser,
    }
}

/// Transactional, serialized migrations with checksums; a changed applied migration is an error.
pub async fn migrate(db: &PgPool) -> Result<(), sqlx::Error> {
    let mut tx = db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(709304117822)")
        .execute(&mut *tx)
        .await?;
    sqlx::query("CREATE TABLE IF NOT EXISTS thalamus_schema_migrations (name TEXT PRIMARY KEY, checksum TEXT NOT NULL)").execute(&mut *tx).await?;
    for (name, script) in [
        ("identity-0001", thalamus_identity::INITIAL_MIGRATION),
        ("identity-0002", thalamus_identity::WORKER_KEYS_MIGRATION),
        ("identity-0003", thalamus_identity::BUDGET_CAPS_MIGRATION),
        (
            "runs-0001",
            include_str!("../../thalamus-runs/migrations/0001_runs.sql"),
        ),
        (
            "runs-0002",
            include_str!("../../thalamus-runs/migrations/0002_actionable.sql"),
        ),
        (
            "runs-0003",
            include_str!("../../thalamus-runs/migrations/0003_run_project.sql"),
        ),
        (
            "changes-0001",
            include_str!("../../thalamus-changes/migrations/0001_changes.sql"),
        ),
        (
            "usage-0001",
            include_str!("../../thalamus-usage/migrations/0001_usage.sql"),
        ),
        ("debug-0001", thalamus_debug::MIGRATION),
    ] {
        let checksum = format!("{:x}", Sha256::digest(script.as_bytes()));
        let previous: Option<String> =
            sqlx::query_scalar("SELECT checksum FROM thalamus_schema_migrations WHERE name=$1")
                .bind(name)
                .fetch_optional(&mut *tx)
                .await?;
        match previous {
            Some(value) if value != checksum => {
                return Err(sqlx::Error::Protocol(format!(
                    "migration {name} checksum changed"
                )))
            }
            Some(_) => {}
            None => {
                sqlx::raw_sql(script).execute(&mut *tx).await?;
                sqlx::query("INSERT INTO thalamus_schema_migrations(name,checksum) VALUES ($1,$2)")
                    .bind(name)
                    .bind(checksum)
                    .execute(&mut *tx)
                    .await?;
            }
        }
    }
    tx.commit().await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn session_rejects_ambiguous_cookies() {
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            "__Host-thalamus-session=a; __Host-thalamus-session=b"
                .parse()
                .unwrap(),
        );
        assert!(session(&h).is_err());
        h.insert(
            header::COOKIE,
            "other=x; __Host-thalamus-session=a".parse().unwrap(),
        );
        assert_eq!(session(&h).unwrap(), "a");
    }
    #[test]
    fn credential_selection_rejects_downgrade_and_ambiguous_headers() {
        let mut headers = HeaderMap::new();
        assert!(credentials(&headers).is_err());
        headers.insert(header::AUTHORIZATION, "Bearer token".parse().unwrap());
        assert!(matches!(
            credentials(&headers),
            Ok(Credentials::Bearer("token"))
        ));
        headers.insert(header::COOKIE, "other=value".parse().unwrap());
        assert!(matches!(
            credentials(&headers),
            Ok(Credentials::Bearer("token"))
        ));
        headers.insert(
            header::COOKIE,
            "__Host-thalamus-session=session".parse().unwrap(),
        );
        assert!(credentials(&headers).is_err());
        assert!(session(&headers).is_err());
        headers.remove(header::COOKIE);
        headers.append(header::AUTHORIZATION, "Bearer another".parse().unwrap());
        assert!(credentials(&headers).is_err());
        headers.remove(header::AUTHORIZATION);
        headers.insert(header::AUTHORIZATION, "Basic token".parse().unwrap());
        assert!(credentials(&headers).is_err());
        headers.remove(header::AUTHORIZATION);
        headers.insert(
            header::COOKIE,
            "__Host-thalamus-session=session".parse().unwrap(),
        );
        assert!(
            matches!(credentials(&headers), Ok(Credentials::Session(value)) if value == "session")
        );
    }
}
