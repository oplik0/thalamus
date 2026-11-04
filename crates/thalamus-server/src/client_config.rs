//! Client setup derived from current, independently validated service contracts.
//!
//! A catalog declaration or a submitted run request is not validation evidence.
//! The server starts without an evidence provider and fails closed until a trusted
//! collector is installed. The check uses only the operator-configured gateway;
//! neither a request URL nor a supplied key is ever persisted or logged here.

use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use axum::{
    extract::{rejection::JsonRejection, DefaultBodyLimit, State},
    http::{header::AUTHORIZATION, HeaderValue, StatusCode},
    routing::post,
    Extension, Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use thalamus_core::{
    published_capabilities, EligibleDeployment, EngineProtocol, PublishedCapabilities,
    PublishedService, ValidatedCapabilities,
};
use url::Url;

use crate::{ApiError, AppState, RequestPrincipal};

/// A snapshot collected by the control plane, never supplied by an API caller.
///
/// `service` must describe the *actual* current routing membership, including
/// failover and zero-weight targets; `eligible` must contain the freshly observed
/// exact revision for every target. `validations` must come from authenticated,
/// successful validation runs bound to those revisions, and a failed newer run or
/// unavailable/stale observation must make the collector return `None`. The
/// feature names `tools` and `streaming` mean that both behaviors were verified
/// for every exposed protocol, not merely declared by an artifact or recipe.
/// `tools` must include a successful tool loop accepting a 128-token output budget.
/// A collector must not infer a revision or passing result from a run request.
pub struct CapabilityEvidence {
    pub service: PublishedService,
    pub eligible: Vec<EligibleDeployment>,
    pub validations: Vec<ValidatedCapabilities>,
}

/// Trusted adapter for freshly collecting routing, revisions and run evidence.
/// Implementations must authorize the authenticated user for the service and
/// return `None` for denied access or missing, stale, ambiguous, unauthenticated
/// or otherwise incomplete data. It must not be backed by browser-provided data.
pub trait CapabilityEvidenceProvider: Send + Sync {
    fn collect<'a>(
        &'a self,
        service: &'a str,
        user: uuid::Uuid,
    ) -> Pin<Box<dyn Future<Output = Option<CapabilityEvidence>> + Send + 'a>>;
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConfigRequest {
    service: String,
    client: String,
    major_version: String,
}

// Deliberately not Debug: this object contains a pasted credential.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CheckRequest {
    service: String,
    client: String,
    major_version: String,
    key: String,
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct ConfigResponse {
    config: String,
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct CheckResponse {
    ok: bool,
    detail: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Client {
    OpenCode,
    OpenCodeV2,
    Pi,
    ClaudeCode,
    Codex,
}

impl Client {
    // These are client-specific supported major versions, not a shared version
    // range. A full version, leading zero, or an unknown future major is refused.
    fn parse(client: &str, major: &str) -> Result<Self, ApiError> {
        match (client, major) {
            ("opencode", "1") => Ok(Self::OpenCode),
            ("opencode", "2") => Ok(Self::OpenCodeV2),
            ("pi", "0") => Ok(Self::Pi),
            ("claude-code", "2") => Ok(Self::ClaudeCode),
            ("codex", "0") => Ok(Self::Codex),
            ("opencode" | "pi" | "claude-code" | "codex", _) => Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "unsupported major version for this client",
            )),
            _ => Err(ApiError::new(StatusCode::BAD_REQUEST, "unsupported client")),
        }
    }

    fn protocol(self) -> EngineProtocol {
        match self {
            Self::OpenCode | Self::OpenCodeV2 | Self::Pi => EngineProtocol::OpenAiChat,
            Self::ClaudeCode => EngineProtocol::AnthropicMessages,
            // Codex uses the Responses API, not OpenAI Chat Completions.
            Self::Codex => EngineProtocol::Other("openai_responses".into()),
        }
    }

    fn endpoint(self) -> &'static str {
        match self {
            Self::OpenCode | Self::OpenCodeV2 | Self::Pi => "chat/completions",
            Self::ClaudeCode => "messages",
            Self::Codex => "responses",
        }
    }
}

fn valid_service_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b':' | b'/'))
}

fn unavailable(message: &'static str) -> ApiError {
    ApiError::new(StatusCode::SERVICE_UNAVAILABLE, message)
}

/// Require an explicit, operator-controlled HTTPS API base ending in `/v1`.
/// It cannot carry credentials, query parameters or a fragment; rejecting
/// encoded/ambiguous path segments also makes endpoint construction unambiguous.
fn gateway_url(configured: Option<&str>) -> Result<Url, ApiError> {
    let value = configured.ok_or_else(|| unavailable("gateway URL is not configured"))?;
    let mut url = Url::parse(value).map_err(|_| unavailable("invalid gateway URL"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(unavailable(
            "gateway URL must be an HTTPS API base without credentials, query or fragment",
        ));
    }
    let path = url.path().trim_end_matches('/').to_owned();
    if !path.ends_with("/v1")
        || path.split('/').skip(1).any(|segment| {
            segment.is_empty()
                || matches!(segment, "." | "..")
                || !segment
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b'~'))
        })
    {
        return Err(unavailable(
            "gateway URL must end in /v1 and use unambiguous path segments",
        ));
    }
    url.set_path(&path);
    Ok(url)
}

fn endpoint(base: &Url, client: Client) -> Url {
    let mut url = base.clone();
    url.set_path(&format!("{}/{}", base.path(), client.endpoint()));
    url
}

async fn collect(
    state: &AppState,
    service: &str,
    user: uuid::Uuid,
) -> Result<CapabilityEvidence, ApiError> {
    let provider = state.capability_evidence.as_ref().ok_or_else(|| {
        unavailable("current deployment revisions and validation evidence are unavailable")
    })?;
    provider.collect(service, user).await.ok_or_else(|| {
        unavailable("current deployment revisions and validation evidence are unavailable")
    })
}

fn verified_contract(
    service: &str,
    client: Client,
    evidence: &CapabilityEvidence,
) -> Result<PublishedCapabilities, ApiError> {
    if evidence.service.name != service {
        return Err(unavailable(
            "validation evidence does not match the service",
        ));
    }
    let published =
        published_capabilities(&evidence.service, &evidence.eligible, &evidence.validations)
            .map_err(|_| unavailable("current published capabilities cannot be established"))?;
    if !published.contract.protocols.contains(&client.protocol())
        || !published.contract.features.contains("tools")
        || !published.contract.features.contains("streaming")
        || published.contract.context_tokens < 128
    {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "the service has not validated the protocol, streaming, tools and context required by this client",
        ));
    }
    // Claude's supported auto-compaction override cannot represent smaller windows.
    if client == Client::ClaudeCode && published.contract.context_tokens < 100_000 {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "the validated context is too small for Claude Code's context override",
        ));
    }
    Ok(published)
}

fn generate_config(
    client: Client,
    service: &str,
    base: &Url,
    published: &PublishedCapabilities,
) -> Result<String, ApiError> {
    let context = published.contract.context_tokens;
    // A 128-token output budget is supported by the validated tool-loop
    // contract; the context probe alone does not establish an output limit.
    let document = match client {
        Client::OpenCode => {
            let mut models = Map::new();
            models.insert(
                service.to_owned(),
                json!({
                    "name": service,
                    "tool_call": true,
                    "reasoning": false,
                    "limit": {"context": context, "output": 128}
                }),
            );
            json!({
                "$schema": "https://opencode.ai/config.json",
                "model": format!("thalamus/{service}"),
                "provider": {"thalamus": {
                    "npm": "@ai-sdk/openai-compatible",
                    "name": "Thalamus",
                    "options": {"baseURL": base.as_str(), "apiKey": "{env:THALAMUS_API_KEY}"},
                    "models": Value::Object(models)
                }}
            })
        }
        Client::OpenCodeV2 => {
            let mut models = Map::new();
            models.insert(
                service.to_owned(),
                json!({
                    "name": service,
                    "capabilities": {
                        "tools": true, "input": ["text"], "output": ["text"],
                        "reasoning": {"supported": false}
                    },
                    "limit": {"context": context, "output": 128}
                }),
            );
            json!({
                "$schema": "https://opencode.ai/config.json",
                "model": format!("thalamus/{service}"),
                "providers": {"thalamus": {
                    "name": "Thalamus",
                    "env": ["THALAMUS_API_KEY"],
                    "package": "@opencode/ai/providers/openai-compatible",
                    "settings": {"baseURL": base.as_str()},
                    "models": Value::Object(models)
                }}
            })
        }
        Client::Pi => json!({
            "providers": {"thalamus": {
                "baseUrl": base.as_str(),
                "api": "openai-completions",
                "apiKey": "$THALAMUS_API_KEY",
                "models": [{
                    "id": service, "name": service, "reasoning": false,
                    "input": ["text"], "contextWindow": context, "maxTokens": 128
                }]
            }}
        }),
        Client::ClaudeCode => {
            let mut anthropic_base = base.clone();
            let prefix = base.path().strip_suffix("/v1").expect("validated API base");
            anthropic_base.set_path(if prefix.is_empty() { "/" } else { prefix });
            json!({"env": {
                "ANTHROPIC_BASE_URL": anthropic_base.as_str(),
                "ANTHROPIC_AUTH_TOKEN": "REPLACE_WITH_YOUR_API_KEY",
                "ANTHROPIC_MODEL": service,
                "CLAUDE_CODE_MAX_OUTPUT_TOKENS": "128",
                "CLAUDE_CODE_AUTO_COMPACT_WINDOW": context.min(1_000_000).to_string()
            }})
        }
        Client::Codex => {
            // JSON basic-string escaping is also valid TOML for these validated
            // service and URL strings; the credential stays in an environment variable.
            let model = serde_json::to_string(service)
                .map_err(|_| unavailable("cannot encode client configuration"))?;
            let url = serde_json::to_string(base.as_str())
                .map_err(|_| unavailable("cannot encode client configuration"))?;
            return Ok(format!(
                "model = {model}\nmodel_provider = \"thalamus\"\nmodel_context_window = {context}\n\n[model_providers.thalamus]\nname = \"Thalamus\"\nbase_url = {url}\nwire_api = \"responses\"\nenv_key = \"THALAMUS_API_KEY\"\n"
            ));
        }
    };
    serde_json::to_string_pretty(&document)
        .map_err(|_| unavailable("cannot encode client configuration"))
}

async fn authenticate(state: &AppState, caller: &RequestPrincipal) -> Result<uuid::Uuid, ApiError> {
    Ok(state
        .identity
        .authenticate_principal(caller.principal())
        .await?)
}

#[utoipa::path(post, path = "/api/v1/client-config", request_body = ConfigRequest, responses((status = 200, body = ConfigResponse), (status = 400, body = crate::ErrorBody), (status = 401, body = crate::ErrorBody), (status = 503, body = crate::ErrorBody)))]
pub(crate) async fn config(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<RequestPrincipal>,
    body: Result<Json<ConfigRequest>, JsonRejection>,
) -> Result<Json<ConfigResponse>, ApiError> {
    let user = authenticate(&state, &caller).await?;
    let Json(request) =
        body.map_err(|error| ApiError::new(error.status(), "invalid request body"))?;
    let client = Client::parse(&request.client, &request.major_version)?;
    if !valid_service_name(&request.service) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid service name",
        ));
    }
    let base = gateway_url(state.config.upstream_url.as_deref())?;
    let evidence = collect(&state, &request.service, user).await?;
    let contract = verified_contract(&request.service, client, &evidence)?;
    Ok(Json(ConfigResponse {
        config: generate_config(client, &request.service, &base, &contract)?,
    }))
}

const PROMPT: &str =
    "Call the lookup tool with key alpha. After the tool returns, reply with its value only.";
const MAX_GATEWAY_BODY: usize = 256 * 1024;

fn tool_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {"key": {"type": "string"}},
        "required": ["key"],
        "additionalProperties": false
    })
}

fn first_request(client: Client, service: &str) -> Value {
    match client {
        Client::OpenCode | Client::OpenCodeV2 | Client::Pi => json!({
            "model": service, "max_tokens": 128,
            "messages": [{"role":"user", "content":PROMPT}],
            "tools": [{"type":"function", "function": {
                "name":"lookup", "description":"Look up a key", "parameters":tool_parameters()
            }}], "tool_choice":"required"
        }),
        Client::ClaudeCode => json!({
            "model": service, "max_tokens": 128,
            "messages": [{"role":"user", "content":PROMPT}],
            "tools": [{"name":"lookup", "description":"Look up a key", "input_schema":tool_parameters()}],
            "tool_choice":{"type":"tool", "name":"lookup"}
        }),
        Client::Codex => json!({
            "model": service, "max_output_tokens": 128, "store": false,
            "input": [{"role":"user", "content":PROMPT}],
            "tools": [{"type":"function", "name":"lookup", "description":"Look up a key",
                "parameters":tool_parameters(), "strict":true}], "tool_choice":"required"
        }),
    }
}

fn valid_call_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control)
}

/// Return a tool-call ID only for one well-formed lookup of the requested key.
fn tool_call(client: Client, response: &Value) -> Option<String> {
    let expected = json!({"key":"alpha"});
    let (id, valid) = match client {
        Client::OpenCode | Client::OpenCodeV2 | Client::Pi => {
            let message = response.pointer("/choices/0/message")?;
            if message.get("role")?.as_str()? != "assistant" {
                return None;
            }
            let calls = message.get("tool_calls")?.as_array()?;
            if calls.len() != 1 {
                return None;
            }
            let call = &calls[0];
            let arguments: Value =
                serde_json::from_str(call.pointer("/function/arguments")?.as_str()?).ok()?;
            (
                call.get("id")?.as_str()?,
                call.get("type")?.as_str()? == "function"
                    && call.pointer("/function/name")?.as_str()? == "lookup"
                    && arguments == expected,
            )
        }
        Client::ClaudeCode => {
            if response.get("role")?.as_str()? != "assistant" {
                return None;
            }
            let mut calls = response
                .get("content")?
                .as_array()?
                .iter()
                .filter(|item| item.get("type").and_then(Value::as_str) == Some("tool_use"));
            let call = calls.next()?;
            if calls.next().is_some() {
                return None;
            }
            (
                call.get("id")?.as_str()?,
                call.get("name")?.as_str()? == "lookup" && call.get("input")? == &expected,
            )
        }
        Client::Codex => {
            let mut calls =
                response.get("output")?.as_array()?.iter().filter(|item| {
                    item.get("type").and_then(Value::as_str) == Some("function_call")
                });
            let call = calls.next()?;
            if calls.next().is_some() {
                return None;
            }
            let arguments: Value = serde_json::from_str(call.get("arguments")?.as_str()?).ok()?;
            (
                call.get("call_id")?.as_str()?,
                call.get("name")?.as_str()? == "lookup" && arguments == expected,
            )
        }
    };
    (valid && valid_call_id(id)).then(|| id.to_owned())
}

fn continuation_request(client: Client, service: &str, id: &str, marker: &str) -> Value {
    let arguments = "{\"key\":\"alpha\"}";
    match client {
        Client::OpenCode | Client::OpenCodeV2 | Client::Pi => json!({
            "model":service, "max_tokens":128,
            "messages":[
                {"role":"user", "content":PROMPT},
                {"role":"assistant", "content":null, "tool_calls":[{
                    "id":id, "type":"function", "function":{"name":"lookup", "arguments":arguments}
                }]},
                {"role":"tool", "tool_call_id":id, "content":marker}
            ]
        }),
        Client::ClaudeCode => json!({
            "model":service, "max_tokens":128,
            "messages":[
                {"role":"user", "content":PROMPT},
                {"role":"assistant", "content":[{"type":"tool_use", "id":id, "name":"lookup", "input":{"key":"alpha"}}]},
                {"role":"user", "content":[{"type":"tool_result", "tool_use_id":id, "content":marker}]}
            ],
            "tools":[{"name":"lookup", "description":"Look up a key", "input_schema":tool_parameters()}]
        }),
        Client::Codex => json!({
            "model":service, "max_output_tokens":128, "store":false,
            "input":[
                {"role":"user", "content":PROMPT},
                {"type":"function_call", "call_id":id, "name":"lookup", "arguments":arguments},
                {"type":"function_call_output", "call_id":id, "output":marker}
            ],
            "tools":[{"type":"function", "name":"lookup", "description":"Look up a key", "parameters":tool_parameters(), "strict":true}]
        }),
    }
}

fn answer_contains_marker(client: Client, response: &Value, marker: &str) -> bool {
    match client {
        Client::OpenCode | Client::OpenCodeV2 | Client::Pi => {
            response
                .pointer("/choices/0/message/role")
                .and_then(Value::as_str)
                == Some("assistant")
                && response
                    .pointer("/choices/0/message/content")
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.contains(marker))
        }
        Client::ClaudeCode => {
            response.get("role").and_then(Value::as_str) == Some("assistant")
                && response
                    .get("content")
                    .and_then(Value::as_array)
                    .is_some_and(|items| {
                        items.iter().any(|item| {
                            item.get("type").and_then(Value::as_str) == Some("text")
                                && item
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .is_some_and(|text| text.contains(marker))
                        })
                    })
        }
        Client::Codex => response
            .get("output")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items.iter().any(|item| {
                    item.get("type").and_then(Value::as_str) == Some("message")
                        && item.get("role").and_then(Value::as_str) == Some("assistant")
                        && item
                            .get("content")
                            .and_then(Value::as_array)
                            .is_some_and(|parts| {
                                parts.iter().any(|part| {
                                    part.get("type").and_then(Value::as_str) == Some("output_text")
                                        && part
                                            .get("text")
                                            .and_then(Value::as_str)
                                            .is_some_and(|text| text.contains(marker))
                                })
                            })
                })
            }),
    }
}

async fn gateway_post(
    http: &reqwest::Client,
    url: &Url,
    client: Client,
    authorization: &HeaderValue,
    body: &Value,
) -> Result<Value, &'static str> {
    let mut request = http
        .post(url.clone())
        .header(AUTHORIZATION, authorization.clone())
        .json(body);
    if client == Client::ClaudeCode {
        request = request.header("anthropic-version", "2023-06-01");
    }
    // Never surface a transport error or upstream body: either could include a
    // credential, URL, or user-controlled content. Redirects are disabled below.
    let mut response = request
        .send()
        .await
        .map_err(|_| "Gateway request failed or timed out.")?;
    if !response.status().is_success() {
        return Err("Gateway rejected or failed the tool-call request.");
    }
    if response
        .content_length()
        .is_some_and(|len| len > MAX_GATEWAY_BODY as u64)
    {
        return Err("Gateway response exceeded the size limit.");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "Gateway response could not be read.")?
    {
        if chunk.len() > MAX_GATEWAY_BODY.saturating_sub(bytes.len()) {
            return Err("Gateway response exceeded the size limit.");
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| "Gateway returned an invalid JSON response.")
}

async fn tool_roundtrip(
    http: &reqwest::Client,
    base: &Url,
    client: Client,
    service: &str,
    key: &str,
) -> Result<(), &'static str> {
    let mut authorization =
        HeaderValue::from_str(&format!("Bearer {key}")).map_err(|_| "Invalid API key.")?;
    authorization.set_sensitive(true);
    let url = endpoint(base, client);
    let first = gateway_post(
        http,
        &url,
        client,
        &authorization,
        &first_request(client, service),
    )
    .await?;
    let id = tool_call(client, &first).ok_or("Gateway did not return the expected tool call.")?;
    // The marker is generated only after receiving the call; the model cannot
    // know it without receiving and processing the actual tool result.
    let marker = format!("thalamus-check-{}", uuid::Uuid::new_v4().simple());
    let second = gateway_post(
        http,
        &url,
        client,
        &authorization,
        &continuation_request(client, service, &id, &marker),
    )
    .await?;
    if !answer_contains_marker(client, &second, &marker) {
        return Err("Gateway did not complete the tool-result round trip.");
    }
    Ok(())
}

#[utoipa::path(post, path = "/api/v1/client-config/check", request_body = CheckRequest, responses((status = 200, body = CheckResponse), (status = 400, body = crate::ErrorBody), (status = 401, body = crate::ErrorBody), (status = 503, body = crate::ErrorBody)))]
pub(crate) async fn check(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<RequestPrincipal>,
    body: Result<Json<CheckRequest>, JsonRejection>,
) -> Result<Json<CheckResponse>, ApiError> {
    let user = authenticate(&state, &caller).await?;
    let Json(request) =
        body.map_err(|error| ApiError::new(error.status(), "invalid request body"))?;
    let client = Client::parse(&request.client, &request.major_version)?;
    if !valid_service_name(&request.service) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid service name",
        ));
    }
    if request.key.is_empty()
        || request.key.len() > 4096
        || !request.key.bytes().all(|c| (0x21..=0x7e).contains(&c))
    {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "invalid API key"));
    }
    let base = gateway_url(state.config.upstream_url.as_deref())?;
    let evidence = collect(&state, &request.service, user).await?;
    verified_contract(&request.service, client, &evidence)?;
    let http = reqwest::Client::builder()
        .https_only(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| unavailable("secure gateway transport is unavailable"))?;
    let result = tool_roundtrip(&http, &base, client, &request.service, &request.key).await;
    Ok(Json(match result {
        Ok(()) => CheckResponse {
            ok: true,
            detail: "Authenticated tool-call round trip completed.",
        },
        Err(detail) => CheckResponse { ok: false, detail },
    }))
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/client-config", post(config))
        .route("/client-config/check", post(check))
        .layer(DefaultBodyLimit::max(16 * 1024))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use thalamus_core::{CapabilityContract, ServiceDeployment};

    fn evidence(protocol: EngineProtocol) -> CapabilityEvidence {
        let protocols = BTreeSet::from([protocol]);
        let contract = |context, features: &[&str]| CapabilityContract {
            context_tokens: context,
            features: features.iter().map(|s| (*s).to_owned()).collect(),
            protocols: protocols.clone(),
        };
        CapabilityEvidence {
            service: PublishedService {
                name: "model-a".into(),
                protocols: protocols.clone(),
                deployments: vec![
                    ServiceDeployment {
                        deployment: "a".into(),
                        weight: 100,
                    },
                    ServiceDeployment {
                        deployment: "failover".into(),
                        weight: 0,
                    },
                ],
                access_policy: "policy".into(),
                owners: vec![],
            },
            eligible: vec![
                EligibleDeployment {
                    deployment: "a".into(),
                    revision: "a-current".into(),
                },
                EligibleDeployment {
                    deployment: "failover".into(),
                    revision: "b-current".into(),
                },
            ],
            validations: vec![
                ValidatedCapabilities {
                    deployment: "a".into(),
                    revision: "a-current".into(),
                    run_id: "run-a".into(),
                    contract: contract(200_000, &["tools", "streaming", "reasoning"]),
                },
                ValidatedCapabilities {
                    deployment: "failover".into(),
                    revision: "b-current".into(),
                    run_id: "run-b".into(),
                    contract: contract(120_000, &["tools", "streaming"]),
                },
            ],
        }
    }

    #[test]
    fn major_versions_are_client_specific_and_exact() {
        for (name, version, expected) in [
            ("opencode", "1", Client::OpenCode),
            ("opencode", "2", Client::OpenCodeV2),
            ("pi", "0", Client::Pi),
            ("claude-code", "2", Client::ClaudeCode),
            ("codex", "0", Client::Codex),
        ] {
            assert_eq!(Client::parse(name, version).unwrap(), expected);
        }
        for (name, version) in [
            ("pi", "1"),
            ("codex", "1"),
            ("claude-code", "1"),
            ("opencode", "3"),
            ("opencode", "01"),
            ("opencode", "1.0"),
            ("opencode", " 1"),
            ("unknown", "1"),
        ] {
            assert!(Client::parse(name, version).is_err());
        }
    }

    #[test]
    fn every_routing_target_needs_unique_current_passing_evidence() {
        let e = evidence(EngineProtocol::OpenAiChat);
        let result = verified_contract("model-a", Client::Pi, &e).unwrap();
        assert_eq!(result.contract.context_tokens, 120_000);
        assert!(!result.contract.features.contains("reasoning"));
        assert!(verified_contract("other", Client::Pi, &e).is_err());
        let mut missing = evidence(EngineProtocol::OpenAiChat);
        missing.eligible.pop();
        assert!(verified_contract("model-a", Client::Pi, &missing).is_err());
        let mut stale = evidence(EngineProtocol::OpenAiChat);
        stale.validations[1].revision = "old".into();
        assert!(verified_contract("model-a", Client::Pi, &stale).is_err());
        let mut duplicate = evidence(EngineProtocol::OpenAiChat);
        duplicate.validations.push(duplicate.validations[1].clone());
        assert!(verified_contract("model-a", Client::Pi, &duplicate).is_err());
        let mut no_run = evidence(EngineProtocol::OpenAiChat);
        no_run.validations[1].run_id.clear();
        assert!(verified_contract("model-a", Client::Pi, &no_run).is_err());
        let mut missing_tools = evidence(EngineProtocol::OpenAiChat);
        missing_tools.validations[1]
            .contract
            .features
            .remove("tools");
        assert!(verified_contract("model-a", Client::Pi, &missing_tools).is_err());
        let mut missing_stream = evidence(EngineProtocol::OpenAiChat);
        missing_stream.validations[1]
            .contract
            .features
            .remove("streaming");
        assert!(verified_contract("model-a", Client::Pi, &missing_stream).is_err());
        let mut absent = evidence(EngineProtocol::OpenAiChat);
        absent.validations.clear();
        assert!(verified_contract("model-a", Client::Pi, &absent).is_err());
        let mut too_small = evidence(EngineProtocol::OpenAiChat);
        too_small.validations[1].contract.context_tokens = 127;
        assert!(verified_contract("model-a", Client::Pi, &too_small).is_err());
    }

    #[test]
    fn protocol_exposure_and_validation_are_not_interchangeable() {
        let chat = evidence(EngineProtocol::OpenAiChat);
        assert!(verified_contract("model-a", Client::ClaudeCode, &chat).is_err());
        assert!(verified_contract("model-a", Client::Codex, &chat).is_err());
        let anthropic = evidence(EngineProtocol::AnthropicMessages);
        assert!(verified_contract("model-a", Client::ClaudeCode, &anthropic).is_ok());
        let responses = evidence(EngineProtocol::Other("openai_responses".into()));
        assert!(verified_contract("model-a", Client::Codex, &responses).is_ok());
        let mut incomplete = evidence(EngineProtocol::AnthropicMessages);
        incomplete.validations[1].contract.protocols.clear();
        assert!(verified_contract("model-a", Client::ClaudeCode, &incomplete).is_err());
        let mut small_window = evidence(EngineProtocol::AnthropicMessages);
        small_window.validations[1].contract.context_tokens = 99_999;
        assert!(verified_contract("model-a", Client::ClaudeCode, &small_window).is_err());
    }

    #[test]
    fn configuration_uses_intersection_and_has_no_stored_secret() {
        let e = evidence(EngineProtocol::OpenAiChat);
        let contract = verified_contract("model-a", Client::OpenCode, &e).unwrap();
        let base = gateway_url(Some("https://gateway.example/prefix/v1")).unwrap();
        let code: Value = serde_json::from_str(
            &generate_config(Client::OpenCode, "model-a", &base, &contract).unwrap(),
        )
        .unwrap();
        assert_eq!(
            code.pointer("/provider/thalamus/models/model-a/limit/context"),
            Some(&json!(120_000))
        );
        assert_eq!(
            code.pointer("/provider/thalamus/models/model-a/reasoning"),
            Some(&json!(false))
        );
        assert_eq!(
            code.pointer("/provider/thalamus/options/apiKey"),
            Some(&json!("{env:THALAMUS_API_KEY}"))
        );
        let code_v2: Value = serde_json::from_str(
            &generate_config(Client::OpenCodeV2, "model-a", &base, &contract).unwrap(),
        )
        .unwrap();
        assert_eq!(
            code_v2.pointer("/providers/thalamus/models/model-a/limit/context"),
            Some(&json!(120_000))
        );
        assert_eq!(
            code_v2.pointer("/providers/thalamus/models/model-a/capabilities/tools"),
            Some(&json!(true))
        );
        assert_eq!(
            code_v2.pointer("/providers/thalamus/env/0"),
            Some(&json!("THALAMUS_API_KEY"))
        );
        let pi: Value = serde_json::from_str(
            &generate_config(Client::Pi, "model-a", &base, &contract).unwrap(),
        )
        .unwrap();
        assert_eq!(
            pi.pointer("/providers/thalamus/models/0/contextWindow"),
            Some(&json!(120_000))
        );
        assert_eq!(
            pi.pointer("/providers/thalamus/baseUrl"),
            Some(&json!("https://gateway.example/prefix/v1"))
        );
    }

    #[test]
    fn non_chat_configs_use_their_own_validated_protocol() {
        let base = gateway_url(Some("https://gateway.example/prefix/v1")).unwrap();
        let anthropic = evidence(EngineProtocol::AnthropicMessages);
        let contract = verified_contract("model-a", Client::ClaudeCode, &anthropic).unwrap();
        let claude: Value = serde_json::from_str(
            &generate_config(Client::ClaudeCode, "model-a", &base, &contract).unwrap(),
        )
        .unwrap();
        assert_eq!(
            claude.pointer("/env/ANTHROPIC_BASE_URL"),
            Some(&json!("https://gateway.example/prefix"))
        );
        assert_eq!(
            claude.pointer("/env/ANTHROPIC_AUTH_TOKEN"),
            Some(&json!("REPLACE_WITH_YOUR_API_KEY"))
        );
        let responses = evidence(EngineProtocol::Other("openai_responses".into()));
        let contract = verified_contract("model-a", Client::Codex, &responses).unwrap();
        let codex = generate_config(Client::Codex, "model-a", &base, &contract).unwrap();
        assert!(codex.contains("wire_api = \"responses\""));
        assert!(codex.contains("model_context_window = 120000"));
        assert!(codex.contains("env_key = \"THALAMUS_API_KEY\""));
    }

    #[test]
    fn unsafe_or_missing_gateway_is_unavailable() {
        for value in [
            None,
            Some("http://gateway.example/v1"),
            Some("https://name:secret@gateway.example/v1"),
            Some("https://gateway.example/v1?key=x"),
            Some("https://gateway.example/v1#x"),
            Some("https://gateway.example/v2"),
            Some("https://gateway.example/%2f/v1"),
        ] {
            assert!(gateway_url(value).is_err());
        }
        let base = gateway_url(Some("https://gateway.example/api/v1/")).unwrap();
        assert_eq!(
            endpoint(&base, Client::Pi).as_str(),
            "https://gateway.example/api/v1/chat/completions"
        );
        assert_eq!(
            endpoint(&base, Client::ClaudeCode).as_str(),
            "https://gateway.example/api/v1/messages"
        );
        assert_eq!(
            endpoint(&base, Client::Codex).as_str(),
            "https://gateway.example/api/v1/responses"
        );
    }

    #[test]
    fn roundtrip_requires_actual_valid_call_and_returned_marker() {
        let chat = json!({"choices":[{"message":{"role":"assistant", "tool_calls":[{"id":"c1", "type":"function", "function":{"name":"lookup", "arguments":"{\"key\":\"alpha\"}"}}]}}]});
        assert_eq!(tool_call(Client::Pi, &chat).as_deref(), Some("c1"));
        assert!(tool_call(
            Client::Pi,
            &json!({"choices":[{"message":{"role":"assistant", "content":"done"}}]})
        )
        .is_none());
        let continuation =
            continuation_request(Client::Pi, "model-a", "c1", "unpredictable-marker");
        assert_eq!(
            continuation.pointer("/messages/2/tool_call_id"),
            Some(&json!("c1"))
        );
        assert!(!answer_contains_marker(
            Client::Pi,
            &json!({"choices":[{"message":{"role":"assistant", "content":"ok"}}]}),
            "unpredictable-marker"
        ));
        assert!(answer_contains_marker(
            Client::Pi,
            &json!({"choices":[{"message":{"role":"assistant", "content":"unpredictable-marker"}}]}),
            "unpredictable-marker"
        ));
        let anthropic = json!({"role":"assistant", "content":[{"type":"tool_use", "id":"c2", "name":"lookup", "input":{"key":"alpha"}}]});
        assert_eq!(
            tool_call(Client::ClaudeCode, &anthropic).as_deref(),
            Some("c2")
        );
        let responses = json!({"output":[{"type":"function_call", "call_id":"c3", "name":"lookup", "arguments":"{\"key\":\"alpha\"}"}]});
        assert_eq!(tool_call(Client::Codex, &responses).as_deref(), Some("c3"));
        assert!(!answer_contains_marker(
            Client::Codex,
            &json!({"output":[]}),
            "unpredictable-marker"
        ));
    }
}
