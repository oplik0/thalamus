//! MCP resource-server transport. The host must implement `ControlPlane` using the
//! same authorization and audited application operations as its UI, and must verify
//! OAuth access tokens (including issuer, signature, expiry and exact audience).
//! No Kubernetes API or caller-supplied actor is exposed here.
use async_trait::async_trait;
use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post_service},
    Json as AxumJson, Router,
};
use rmcp::{
    handler::server::{router::tool::ToolRouter, tool::Extension, wrapper::Parameters},
    model::{CallToolResult, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router, ServerHandler,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use url::Url;

#[cfg(test)]
mod tests;

/// An identity returned only by the host's token verifier, never from MCP arguments.
/// The host must resolve this issuer/subject to an active Thalamus user on each call.
/// A debug session binding is resolved by that verifier from the bearer credential,
/// not from a caller-supplied session identifier.
#[derive(Clone, Debug)]
pub struct Identity {
    pub issuer: String,
    pub subject: String,
    pub debug_session: Option<String>,
}

/// The host implementation is the security boundary: `authenticate` must validate an
/// OAuth access token for exactly `audience`, or securely resolve a debug credential
/// to its owner and session. `execute` must enforce current UI RBAC and session limits,
/// derive evidence server-side, and audit mutations as this user acting through MCP.
#[async_trait]
pub trait ControlPlane: Send + Sync + 'static {
    async fn authenticate(&self, bearer: &str, audience: &str) -> Result<Identity, ()>;
    async fn execute(&self, identity: &Identity, action: Action) -> Result<Value, String>;
}

#[derive(Clone, Debug)]
pub enum Action {
    ServiceContext(Service),
    CompareRevisions(Compare),
    QueryMetrics(Service),
    GetLogs(Logs),
    CheckCompatibility(Service),
    RunValidation(Run),
    RunBenchmark(Run),
    GetRun(RunId),
    ProposeChange(Change),
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Service {
    pub service: String,
}
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Compare {
    pub service: String,
    pub from: String,
    pub to: String,
}
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Logs {
    pub service: String,
    pub deployment: String,
    pub since_seconds: u32,
    pub limit: u32,
}
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Run {
    pub project_id: String,
    pub service: String,
    pub deployment: String,
    pub runner: String,
    pub workload: Workload,
    pub idempotency_key: String,
}
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
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
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunId {
    pub id: String,
}
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Change {
    pub service: String,
    pub base_commit: String,
    pub patch: Vec<Patch>,
    pub idempotency_key: String,
}
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Patch {
    pub path: Vec<String>,
    pub operation: PatchOperation,
}
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(tag = "op", content = "value", rename_all = "snake_case")]
pub enum PatchOperation {
    Set(Value),
    Remove,
}

fn valid_text(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control)
}
fn valid_project_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                *b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
        && bytes.iter().any(|b| b.is_ascii_hexdigit() && *b != b'0')
}
impl Action {
    fn validate(&self) -> bool {
        let text = |s: &str| valid_text(s, 253);
        match self {
            Self::ServiceContext(s) | Self::QueryMetrics(s) | Self::CheckCompatibility(s) => {
                text(&s.service)
            }
            Self::CompareRevisions(c) => text(&c.service) && text(&c.from) && text(&c.to),
            Self::GetLogs(l) => {
                text(&l.service)
                    && text(&l.deployment)
                    && (1..=86400).contains(&l.since_seconds)
                    && (1..=1000).contains(&l.limit)
            }
            Self::GetRun(r) => text(&r.id),
            Self::RunValidation(r) | Self::RunBenchmark(r) => {
                let w = &r.workload;
                valid_project_id(&r.project_id)
                    && text(&r.service)
                    && text(&r.deployment)
                    && text(&r.runner)
                    && text(&r.idempotency_key)
                    && text(&w.name)
                    && (1..=256).contains(&w.concurrency)
                    && (1..=3600).contains(&w.duration_seconds)
                    && w.warmup_seconds <= w.duration_seconds
                    && (1..=1_000_000).contains(&w.max_requests)
                    && w.model_budget_tokens <= 1_000_000
                    && w.tool_budget_calls <= 1000
            }
            Self::ProposeChange(c) => {
                text(&c.service)
                    && text(&c.base_commit)
                    && text(&c.idempotency_key)
                    && !c.patch.is_empty()
                    && c.patch.len() <= 128
                    && c.patch.iter().all(|p| {
                        !p.path.is_empty() && p.path.len() <= 32 && p.path.iter().all(|s| text(s))
                    })
            }
        }
    }
}

#[derive(Clone)]
struct Server {
    backend: Arc<dyn ControlPlane>,
    tool_router: ToolRouter<Self>,
}
#[tool_router]
impl Server {
    fn new(backend: Arc<dyn ControlPlane>) -> Self {
        Self {
            backend,
            tool_router: Self::tool_router(),
        }
    }
    async fn invoke(
        &self,
        parts: axum::http::request::Parts,
        action: Action,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        if !action.validate() {
            return Err(rmcp::ErrorData::invalid_params(
                "invalid or out-of-bounds arguments",
                None,
            ));
        }
        let identity = parts.extensions.get::<Identity>().ok_or_else(|| {
            rmcp::ErrorData::internal_error("missing authenticated request identity", None)
        })?;
        self.backend
            .execute(identity, action)
            .await
            .map(CallToolResult::structured)
            .map_err(|_| rmcp::ErrorData::internal_error("control-plane operation failed", None))
    }
    #[tool(description = "Get a published service and its operational context")]
    async fn get_service_context(
        &self,
        Extension(p): Extension<axum::http::request::Parts>,
        Parameters(a): Parameters<Service>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        self.invoke(p, Action::ServiceContext(a)).await
    }
    #[tool(description = "Compare two revisions of a published service")]
    async fn compare_revisions(
        &self,
        Extension(p): Extension<axum::http::request::Parts>,
        Parameters(a): Parameters<Compare>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        self.invoke(p, Action::CompareRevisions(a)).await
    }
    #[tool(description = "Query the preset service metrics")]
    async fn query_metrics(
        &self,
        Extension(p): Extension<axum::http::request::Parts>,
        Parameters(a): Parameters<Service>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        self.invoke(p, Action::QueryMetrics(a)).await
    }
    #[tool(description = "Get bounded logs for a service deployment")]
    async fn get_logs(
        &self,
        Extension(p): Extension<axum::http::request::Parts>,
        Parameters(a): Parameters<Logs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        self.invoke(p, Action::GetLogs(a)).await
    }
    #[tool(description = "Check service compatibility")]
    async fn check_compatibility(
        &self,
        Extension(p): Extension<axum::http::request::Parts>,
        Parameters(a): Parameters<Service>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        self.invoke(p, Action::CheckCompatibility(a)).await
    }
    #[tool(description = "Submit a bounded validation run")]
    async fn run_validation(
        &self,
        Extension(p): Extension<axum::http::request::Parts>,
        Parameters(a): Parameters<Run>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        self.invoke(p, Action::RunValidation(a)).await
    }
    #[tool(description = "Submit a bounded benchmark run")]
    async fn run_benchmark(
        &self,
        Extension(p): Extension<axum::http::request::Parts>,
        Parameters(a): Parameters<Run>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        self.invoke(p, Action::RunBenchmark(a)).await
    }
    #[tool(description = "Get an authorized run")]
    async fn get_run(
        &self,
        Extension(p): Extension<axum::http::request::Parts>,
        Parameters(a): Parameters<RunId>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        self.invoke(p, Action::GetRun(a)).await
    }
    #[tool(description = "Propose a structured catalog change for review")]
    async fn propose_change(
        &self,
        Extension(p): Extension<axum::http::request::Parts>,
        Parameters(a): Parameters<Change>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        self.invoke(p, Action::ProposeChange(a)).await
    }
}
#[tool_handler(router = self.tool_router)]
impl ServerHandler for Server {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

/// External URLs must be HTTPS, without credentials, query or fragment. The
/// resource path is fixed to `/mcp`; mount the returned router at the origin root.
#[derive(Clone)]
pub struct Config {
    resource: Url,
    authorization_server: Url,
}
impl Config {
    pub fn new(resource: &str, authorization_server: &str) -> Result<Self, &'static str> {
        let resource = Url::parse(resource).map_err(|_| "invalid resource URL")?;
        let authorization_server =
            Url::parse(authorization_server).map_err(|_| "invalid authorization server URL")?;
        let safe = |u: &Url| {
            u.scheme() == "https"
                && u.host_str().is_some()
                && u.username().is_empty()
                && u.password().is_none()
                && u.query().is_none()
                && u.fragment().is_none()
        };
        if !safe(&resource) || !safe(&authorization_server) || resource.path() != "/mcp" {
            return Err("URLs must be HTTPS and resource path must be /mcp");
        }
        Ok(Self {
            resource,
            authorization_server,
        })
    }
    fn metadata_url(&self) -> String {
        let mut u = self.resource.clone();
        u.set_path("/.well-known/oauth-protected-resource/mcp");
        u.to_string()
    }
}
#[derive(Clone)]
struct AuthState {
    backend: Arc<dyn ControlPlane>,
    config: Config,
}
fn bearer_token(headers: &axum::http::HeaderMap) -> Option<&str> {
    let all = headers.get_all(header::AUTHORIZATION);
    let mut values = all.iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    let (scheme, credentials) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return None;
    }
    let token = credentials.trim_start_matches(' ');
    let (body, padding) = token.split_once('=').unwrap_or((token, ""));
    if body.is_empty()
        || !body.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'+' | b'/')
        })
        || !padding.bytes().all(|b| b == b'=')
    {
        return None;
    }
    Some(token)
}
async fn authenticate(State(state): State<AuthState>, mut req: Request, next: Next) -> Response {
    let token = bearer_token(req.headers());
    if let Some(token) = token {
        if let Ok(identity) = state
            .backend
            .authenticate(token, state.config.resource.as_str())
            .await
        {
            if valid_text(&identity.issuer, 2048)
                && valid_text(&identity.subject, 1024)
                && identity
                    .debug_session
                    .as_ref()
                    .is_none_or(|id| valid_text(id, 1024))
            {
                req.extensions_mut().insert(identity);
                return next.run(req).await;
            }
        }
    }
    let challenge = format!(
        "Bearer resource_metadata=\"{}\"",
        state.config.metadata_url()
    );
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, challenge)],
        "Unauthorized",
    )
        .into_response()
}

/// Root-mounted router exposing `/mcp` and RFC 9728 protected-resource metadata.
/// Stateless mode prevents cross-user session reuse; every transport request is authenticated.
pub fn router(config: Config, backend: Arc<dyn ControlPlane>) -> Router {
    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager,
        tower::{StreamableHttpServerConfig, StreamableHttpService},
    };
    let factory_backend = backend.clone();
    let service = StreamableHttpService::new(
        move || Ok(Server::new(factory_backend.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig {
            stateful_mode: false,
            ..Default::default()
        },
    );
    let state = AuthState {
        backend,
        config: config.clone(),
    };
    let mcp = Router::new()
        .route_service("/mcp", post_service(service))
        .layer(middleware::from_fn_with_state(state, authenticate));
    let metadata = json!({"resource": config.resource.as_str(), "authorization_servers": [config.authorization_server.as_str()], "bearer_methods_supported": ["header"]});
    mcp.route(
        "/.well-known/oauth-protected-resource/mcp",
        get(move || {
            let metadata = metadata.clone();
            async move { AxumJson(metadata) }
        }),
    )
}

#[cfg(test)]
mod run_project_tests {
    use super::*;

    fn run(project_id: &str) -> Run {
        Run {
            project_id: project_id.into(),
            service: "svc".into(),
            deployment: "prod".into(),
            runner: "runner".into(),
            idempotency_key: "key".into(),
            workload: Workload {
                name: "work".into(),
                seed: 1,
                concurrency: 1,
                duration_seconds: 10,
                warmup_seconds: 1,
                max_requests: 10,
                model_budget_tokens: 100,
                tool_budget_calls: 1,
            },
        }
    }

    #[test]
    fn run_actions_require_non_nil_canonical_project_uuid() {
        let valid = "9d798769-5cba-41c1-b902-53b91d41dd83";
        for action in [
            Action::RunValidation(run(valid)),
            Action::RunBenchmark(run(valid)),
        ] {
            assert!(action.validate());
        }
        for id in [
            "",
            "00000000-0000-0000-0000-000000000000",
            "9d7987695cba41c1b90253b91d41dd83",
            "9d798769-5cba-41c1-b902-53b91d41dd8z",
            "9d798769-5cba-41c1-b902-53b91d41dd83 ",
            "9d798769_5cba-41c1-b902-53b91d41dd83",
        ] {
            assert!(!Action::RunValidation(run(id)).validate(), "{id}");
            assert!(!Action::RunBenchmark(run(id)).validate(), "{id}");
        }
    }

    #[test]
    fn run_schema_and_deserialization_require_project_id() {
        let schema = serde_json::to_value(schemars::schema_for!(Run)).unwrap();
        assert!(schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "project_id"));
        let value = json!({"service":"svc", "deployment":"prod", "runner":"runner", "idempotency_key":"key",
            "workload":{"name":"work", "seed":1, "concurrency":1, "duration_seconds":10,
                "warmup_seconds":1, "max_requests":10, "model_budget_tokens":100, "tool_budget_calls":1}});
        assert!(serde_json::from_value::<Run>(value.clone()).is_err());
        let mut value = value;
        value["project_id"] = json!("9d798769-5cba-41c1-b902-53b91d41dd83");
        let parsed: Run = serde_json::from_value(value).unwrap();
        assert!(Action::RunValidation(parsed).validate());
    }
}
#[cfg(test)]
mod authorization_header_tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    struct Backend(AtomicUsize);
    #[async_trait]
    impl ControlPlane for Backend {
        async fn authenticate(&self, _: &str, _: &str) -> Result<Identity, ()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Identity {
                issuer: "https://id.example".into(),
                subject: "alice".into(),
                debug_session: None,
            })
        }
        async fn execute(&self, _: &Identity, _: Action) -> Result<Value, String> {
            panic!("ambiguous credentials must not reach the control plane")
        }
    }

    #[tokio::test]
    async fn duplicate_authorization_headers_are_rejected_before_authentication() {
        let backend = Arc::new(Backend(AtomicUsize::new(0)));
        let config = Config::new("https://control.example/mcp", "https://id.example").unwrap();
        let request = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::AUTHORIZATION, "Bearer alice")
            .header(header::AUTHORIZATION, "Bearer bob")
            .body(Body::empty())
            .unwrap();
        let response = router(config, backend.clone())
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers()[header::WWW_AUTHENTICATE],
            "Bearer resource_metadata=\"https://control.example/.well-known/oauth-protected-resource/mcp\"");
        assert_eq!(backend.0.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn bearer_token_rejects_malformed_values() {
        let mut headers = axum::http::HeaderMap::new();
        assert!(bearer_token(&headers).is_none());
        for value in [
            "Bearer ",
            "Bearer alice bob",
            "Bearer alice,bob",
            "Bearer a=b",
            "Basic alice",
        ] {
            headers.insert(header::AUTHORIZATION, value.parse().unwrap());
            assert!(bearer_token(&headers).is_none(), "{value}");
        }
        headers.insert(header::AUTHORIZATION, "bearer  abc+/==".parse().unwrap());
        assert_eq!(bearer_token(&headers), Some("abc+/=="));
    }
}
