//! A narrow, credential-bound view of the installed Flux Operator MCP server.
//!
//! Debug clients never receive the upstream server's generic Kubernetes tools.
//! Both operations below start at the session's published HTTPRoute, whose name
//! and namespace come from the committed service policy, not tool arguments.
//! Only same-namespace Flux owners and sources are returned. If an upstream
//! trace reaches another namespace, that part is reported as unavailable.

use std::{sync::Arc, time::Duration};

use axum::{
    body::{to_bytes, Body},
    extract::{Request, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Router,
};
use rmcp::{
    model::{
        CallToolRequestParam, CallToolResult, ListToolsResult, PaginatedRequestParam,
        ServerCapabilities, ServerInfo, Tool, ToolAnnotations,
    },
    service::{RequestContext, ServiceExt as _},
    transport::{
        streamable_http_client::StreamableHttpClientTransportConfig,
        streamable_http_server::{
            session::local::LocalSessionManager,
            tower::{StreamableHttpServerConfig, StreamableHttpService},
        },
        StreamableHttpClientTransport,
    },
    RoleServer, ServerHandler,
};
use serde_json::{json, Value};
use thalamus_debug::store::{DebugStore, ToolCredentialBinding};
use thalamus_identity::VerifiedPrincipal;
use uuid::Uuid;

use crate::AppState;

const MAX_REQUEST_BYTES: usize = 64 * 1024;
const MAX_UPSTREAM_TEXT_BYTES: usize = 128 * 1024;
const MAX_CHAIN: usize = 32;
const MAX_CONDITIONS: usize = 128;

/// Validate the operator-owned endpoint once at configuration time. Plain HTTP
/// is restricted to an in-cluster Service name; no bearer from the debug Job is
/// ever forwarded to this endpoint. The cluster and its networking are trusted
/// installation dependencies, not claimed cryptographic isolation.
pub(crate) fn validate_upstream_url(value: &str) -> Result<(), &'static str> {
    let url = url::Url::parse(value).map_err(|_| "invalid Flux MCP URL")?;
    let host = url.host_str().ok_or("invalid Flux MCP URL")?;
    let service_host = host.ends_with(".svc") || host.ends_with(".svc.cluster.local");
    if url.as_str() != value
        || !(url.scheme() == "https" || (url.scheme() == "http" && service_host))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/mcp"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(
            "Flux MCP URL must be a canonical HTTPS URL or in-cluster Service HTTP URL at /mcp",
        );
    }
    Ok(())
}

/// Mount at the origin root, outside the browser session/origin middleware.
/// Every transport request has its own checked debug credential.
pub(crate) fn router() -> Router<Arc<AppState>> {
    Router::new().route("/flux-mcp", post(transport))
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") || !token.starts_with("thld_") {
        return None;
    }
    Some(token)
}

async fn transport(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let Some(config) = state.config.mcp.as_ref() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    if state.config.debug.is_none() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let metadata = config.verifier.metadata();
    let Some(secret) = bearer(request.headers()) else {
        return unauthorized();
    };
    let binding = match DebugStore::new(state.db.clone())
        .lookup_tool_credential(secret, &metadata.resource)
        .await
    {
        Ok(binding) if metadata.authorization_servers.first() == Some(&binding.issuer) => binding,
        _ => return unauthorized(),
    };

    let (parts, body) = request.into_parts();
    let bytes = match to_bytes(body, MAX_REQUEST_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
    };
    let service = FluxMcp {
        state,
        binding,
        audience: metadata.resource,
    };
    let stream = StreamableHttpService::new(
        move || Ok(service.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig {
            stateful_mode: false,
            ..Default::default()
        },
    );
    stream
        .handle(Request::from_parts(parts, Body::from(bytes)))
        .await
        .map(Body::new)
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        "Unauthorized",
    )
        .into_response()
}

#[derive(Clone)]
struct FluxMcp {
    state: Arc<AppState>,
    binding: ToolCredentialBinding,
    audience: String,
}

struct Scope {
    project_id: Uuid,
    service: String,
    namespace: String,
    checkout_revision: String,
    upstream: String,
}

impl FluxMcp {
    /// Consume before dispatch, including unknown tool names and bad arguments.
    /// This is the same atomic counter used by the primary Thalamus MCP server.
    async fn consume_and_scope(&self) -> Result<Scope, ()> {
        let session = DebugStore::new(self.state.db.clone())
            .consume_tool_call_by_credential(self.binding.credential_id, &self.audience)
            .await
            .map_err(|_| ())?;
        if session.id != self.binding.session_id
            || session.user_id != self.binding.identity.user_id
            || session.team_id != self.binding.identity.team_id
        {
            return Err(());
        }
        let config = self.state.config.debug.as_ref().ok_or(())?;
        if session.job_namespace != config.namespace
            || session.checkout_revision != config.checkout_revision
            || validate_upstream_url(&config.flux_mcp_url).is_err()
        {
            return Err(());
        }
        let project_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM identity_projects WHERE id=$1 AND team_id=$2)",
        )
        .bind(session.project_id)
        .bind(session.team_id)
        .fetch_one(&self.state.db)
        .await
        .map_err(|_| ())?;
        if !project_exists {
            return Err(());
        }

        let source = self.state.policy_source.clone().ok_or(())?;
        let policy = tokio::task::spawn_blocking(move || source.load())
            .await
            .map_err(|_| ())?
            .map_err(|_| ())?;
        let service = policy
            .service_id(&session.service_id)
            .map_err(|_| ())?
            .ok_or(())?;
        if service.name != session.service_id
            || service.namespace != config.namespace
            || !dns_label(&service.name)
            || !dns_label(&service.namespace)
        {
            return Err(());
        }
        let principal = VerifiedPrincipal::Oidc {
            issuer: &self.binding.issuer,
            subject: &self.binding.subject,
        };
        let teams = policy
            .authorized_teams(&self.state.identity, principal, &service)
            .await
            .map_err(|_| ())?;
        if !teams.contains(&session.team_id) {
            return Err(());
        }
        Ok(Scope {
            project_id: session.project_id,
            service: service.name,
            namespace: service.namespace,
            checkout_revision: session.checkout_revision,
            upstream: config.flux_mcp_url.clone(),
        })
    }
}

impl ServerHandler for FluxMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParam>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        if request.and_then(|p| p.cursor).is_some() {
            return Err(rmcp::ErrorData::invalid_params("invalid cursor", None));
        }
        let schema = Arc::new(
            json!({"type":"object", "properties":{}, "additionalProperties":false})
                .as_object()
                .cloned()
                .expect("static schema is an object"),
        );
        let annotations = ToolAnnotations::new()
            .read_only(true)
            .destructive(false)
            .idempotent(true)
            .open_world(false);
        Ok(ListToolsResult::with_all_items(vec![
            Tool::new(
                "get_service_gitops_trace",
                "Trace this service through same-namespace Flux reconcilers and report available revisions; cross-namespace observations are unavailable.",
                schema.clone(),
            )
            .annotate(annotations.clone()),
            Tool::new(
                "get_service_route_conditions",
                "Read same-namespace HTTPRoute parent conditions for this debug session's service.",
                schema,
            )
            .annotate(annotations),
        ]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParam,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let scope = self.consume_and_scope().await.map_err(|_| {
            rmcp::ErrorData::internal_error("debug tool authorization failed", None)
        })?;
        if request
            .arguments
            .as_ref()
            .is_some_and(|args| !args.is_empty())
        {
            return Err(rmcp::ErrorData::invalid_params(
                "this tool does not accept arguments",
                None,
            ));
        }
        let result = match request.name.as_ref() {
            "get_service_gitops_trace" => trace(&scope).await,
            "get_service_route_conditions" => route_conditions(&scope).await,
            _ => return Err(rmcp::ErrorData::invalid_params("tool is not allowed", None)),
        };
        result
            .map(CallToolResult::structured)
            .map_err(|_| rmcp::ErrorData::internal_error("Flux MCP observation unavailable", None))
    }
}

fn dns_label(value: &str) -> bool {
    let bytes = value.as_bytes();
    let valid = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    !bytes.is_empty()
        && bytes.len() <= 63
        && valid(bytes[0])
        && valid(bytes[bytes.len() - 1])
        && bytes.iter().all(|b| valid(*b) || *b == b'-')
}

fn dns_subdomain(value: &str) -> bool {
    value.len() <= 253 && value.split('.').all(dns_label)
}

/// Only the two static upstream tool names and their server-built arguments are
/// passed here. The operator must install Flux Operator MCP v0.62.1 (or a
/// compatible version) in read-only, stateless streamable-HTTP mode.
async fn invoke_upstream(scope: &Scope, name: &'static str, arguments: Value) -> Result<Value, ()> {
    if !matches!(
        name,
        "trace_kubernetes_resource" | "get_kubernetes_resources"
    ) {
        return Err(());
    }
    let arguments = match arguments {
        Value::Object(arguments) => arguments,
        _ => return Err(()),
    };
    let http = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|_| ())?;
    let transport = StreamableHttpClientTransport::with_client(
        http,
        StreamableHttpClientTransportConfig::with_uri(scope.upstream.clone()),
    );
    let client = ().serve(transport).await.map_err(|_| ())?;
    let result = client
        .call_tool(CallToolRequestParam {
            name: name.into(),
            arguments: Some(arguments),
        })
        .await;
    let _ = client.cancel().await;
    let result = result.map_err(|_| ())?;
    if result.is_error == Some(true) || result.content.len() != 1 {
        return Err(());
    }
    let text = &result.content[0].as_text().ok_or(())?.text;
    if text.len() > MAX_UPSTREAM_TEXT_BYTES {
        return Err(());
    }
    serde_yaml::from_str(text).map_err(|_| ())
}

fn safe_token_str(text: &str) -> Option<&str> {
    if text.is_empty()
        || text.len() > 128
        || text.contains("thl_")
        || text.contains("thld_")
        || !text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        return None;
    }
    Some(text)
}

fn safe_token(value: Option<&Value>) -> Option<&str> {
    safe_token_str(value?.as_str()?)
}

/// Flux Git revisions are of the form `branch@sha1:<hex>` or
/// `branch@sha256:<hex>`. Return only a digest, never branch names or URLs.
fn revision_digest(value: Option<&Value>) -> Option<&str> {
    let text = value?.as_str()?;
    let (prefix, digest) = text.rsplit_once(':')?;
    if !((prefix.ends_with("sha1") && digest.len() == 40)
        || (prefix.ends_with("sha256") && digest.len() == 64))
        || !digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    Some(digest)
}

fn namespaced_identity(value: &str) -> Option<(&str, &str)> {
    let mut parts = value.split('/');
    let kind = parts.next()?;
    let namespace = parts.next()?;
    let name = parts.next()?;
    if parts.next().is_some()
        || safe_token_str(kind).is_none()
        || !dns_label(namespace)
        || !dns_subdomain(name)
    {
        return None;
    }
    Some((kind, namespace))
}

async fn trace(scope: &Scope) -> Result<Value, ()> {
    let input = json!({
        "apiVersion": "gateway.networking.k8s.io/v1",
        "kind": "HTTPRoute",
        "name": scope.service,
        "namespace": scope.namespace,
    });
    let observed = invoke_upstream(scope, "trace_kubernetes_resource", input).await?;
    let expected = format!("HTTPRoute/{}/{}", scope.namespace, scope.service);
    if observed.get("object").and_then(Value::as_str) != Some(expected.as_str()) {
        return Err(());
    }
    let unmanaged = observed
        .get("unmanaged")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let source = observed.get("source");
    let source_identity = source
        .and_then(|v| v.get("object"))
        .and_then(Value::as_str)
        .and_then(namespaced_identity);
    let source_is_local =
        source_identity.is_some_and(|(_, namespace)| namespace == scope.namespace);
    let git_source = source_identity
        .is_some_and(|(kind, namespace)| kind == "GitRepository" && namespace == scope.namespace);
    let source_owner = source
        .and_then(|v| v.get("resolvedFor"))
        .and_then(Value::as_str);
    let mut source_owner_in_scope = source_owner == Some(expected.as_str());
    let mut outside_namespace = false;
    let mut chain = Vec::new();
    let mut next = observed.get("managedBy");
    while let Some(entry) = next {
        if chain.len() >= MAX_CHAIN {
            return Err(());
        }
        let object = entry.get("object").and_then(Value::as_str).ok_or(())?;
        let (kind, namespace) = namespaced_identity(object).ok_or(())?;
        if namespace != scope.namespace {
            outside_namespace = true;
            break;
        }
        source_owner_in_scope |= source_owner == Some(object);
        // Owner names, messages, source URLs and arbitrary resource fields can
        // describe other services; none is returned to the debug session.
        // A local reconciler can reference an external source. Do not relay
        // that source's revision indirectly through its reconciler status.
        let revision = if source_is_local && source_owner == Some(object) {
            revision_digest(entry.get("lastAppliedRevision"))
        } else {
            None
        };
        chain.push(json!({
            "kind": safe_token_str(kind),
            "same_service_namespace": true,
            "status": safe_token(entry.get("status")),
            "suspended": entry.get("suspended").and_then(Value::as_bool).unwrap_or(false),
            "applied_revision": revision,
            "matches_checkout_revision": revision
                .filter(|r| git_source && source_owner == Some(object) && r.len() == scope.checkout_revision.len())
                .map(|r| r == scope.checkout_revision),
        }));
        next = entry.get("managedBy");
    }
    let source_revision = if source_is_local && source_owner_in_scope {
        revision_digest(source.and_then(|v| v.get("revision")))
    } else {
        None
    };
    Ok(json!({
        "project_id": scope.project_id,
        "service": scope.service,
        "namespace": scope.namespace,
        "checkout_revision": scope.checkout_revision,
        "unmanaged": unmanaged,
        "reconcilers": chain,
        "outside_namespace_unavailable": outside_namespace,
        "source_availability": if source_is_local && source_owner_in_scope { "available" } else { "unavailable" },
        "source_revision": source_revision,
        "source_matches_checkout_revision": source_revision
            .filter(|r| git_source && r.len() == scope.checkout_revision.len())
            .map(|r| r == scope.checkout_revision),
    }))
}

async fn route_conditions(scope: &Scope) -> Result<Value, ()> {
    let input = json!({
        "apiVersion": "gateway.networking.k8s.io/v1",
        "kind": "HTTPRoute",
        "name": scope.service,
        "namespace": scope.namespace,
        "limit": 1,
        "fields": ["metadata.generation", "status.parents"],
    });
    let observed = invoke_upstream(scope, "get_kubernetes_resources", input).await?;
    if observed.get("apiVersion").and_then(Value::as_str) != Some("gateway.networking.k8s.io/v1")
        || observed.get("kind").and_then(Value::as_str) != Some("HTTPRoute")
        || observed.pointer("/metadata/name").and_then(Value::as_str)
            != Some(scope.service.as_str())
        || observed
            .pointer("/metadata/namespace")
            .and_then(Value::as_str)
            != Some(scope.namespace.as_str())
    {
        return Err(());
    }
    let mut conditions = Vec::new();
    let mut outside_namespace = false;
    let mut local_parent_index = 0;
    if let Some(parents) = observed.get("status.parents") {
        for (seen, parent) in parents.as_array().ok_or(())?.iter().enumerate() {
            if seen >= MAX_CHAIN {
                return Err(());
            }
            let parent_ref = parent
                .get("parentRef")
                .and_then(Value::as_object)
                .ok_or(())?;
            let namespace = match parent_ref.get("namespace") {
                Some(value) => value.as_str().ok_or(())?,
                None => scope.namespace.as_str(),
            };
            if namespace != scope.namespace {
                outside_namespace = true;
                continue;
            }
            let parent_index = local_parent_index;
            local_parent_index += 1;
            let Some(items) = parent.get("conditions") else {
                continue;
            };
            for item in items.as_array().ok_or(())? {
                if conditions.len() >= MAX_CONDITIONS {
                    return Err(());
                }
                let status = item.get("status").and_then(Value::as_str);
                if !matches!(status, Some("True" | "False" | "Unknown")) {
                    return Err(());
                }
                conditions.push(json!({
                    "parent_index": parent_index,
                    "type": safe_token(item.get("type")),
                    "status": status,
                    "reason": safe_token(item.get("reason")),
                    "observed_generation": item.get("observedGeneration").and_then(Value::as_i64),
                }));
            }
        }
    }

    Ok(json!({
        "project_id": scope.project_id,
        "service": scope.service,
        "namespace": scope.namespace,
        "outside_namespace_unavailable": outside_namespace,
        "generation": observed.get("metadata.generation").and_then(Value::as_i64),
        "conditions": conditions,
    }))
}
