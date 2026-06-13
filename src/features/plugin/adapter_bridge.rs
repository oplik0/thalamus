use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use extism::Pool;
use futures::Stream;
use reqwest::StatusCode;

use crate::Error;
use crate::Result;
use crate::features::backends::adapter::openai::apply_backend_auth;
use crate::features::backends::domain::BackendAdapter;
use crate::shared::config::types::AuthConfig;
use crate::shared::models::{ChatResponse, EmbeddingRequest, LlmRequest, StreamEvent};

/// Default timeout for adapter plugin calls in milliseconds.
pub const DEFAULT_ADAPTER_TIMEOUT_MS: u64 = 5000;

/// A `BackendAdapter` implementation that delegates to an Extism plugin.
pub struct ExtismBackendAdapter {
    pool: Arc<Pool>,
    name: String,
    timeout: Duration,
}

impl ExtismBackendAdapter {
    #[must_use]
    pub fn new(pool: Arc<Pool>, name: String, timeout_ms: u64) -> Self {
        Self {
            pool,
            name,
            timeout: Duration::from_millis(timeout_ms),
        }
    }
}

impl std::fmt::Debug for ExtismBackendAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtismBackendAdapter")
            .field("name", &self.name)
            .finish()
    }
}

fn to_plugin_request(request: &LlmRequest) -> thalamus_plugin::types::LlmRequest {
    thalamus_plugin::types::LlmRequest {
        model: request.model().to_string(),
    }
}

fn from_plugin_response(response: thalamus_plugin::types::ChatResponse) -> ChatResponse {
    ChatResponse {
        id: None,
        model: response.model,
        output: Vec::new(),
        status: response.status.and_then(|s| match s.as_str() {
            "completed" => Some(crate::shared::models::ResponseStatus::Completed),
            "in_progress" => Some(crate::shared::models::ResponseStatus::InProgress),
            "incomplete" => Some(crate::shared::models::ResponseStatus::Incomplete),
            _ => None,
        }),
        usage: None,
        service_tier: None,
        extensions: None,
    }
}

impl BackendAdapter for ExtismBackendAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    fn build_request(
        &self,
        client: &reqwest::Client,
        endpoint_url: &str,
        request: &LlmRequest,
        auth: &AuthConfig,
        timeout: Duration,
    ) -> Result<reqwest::Request> {
        let plugin_request = to_plugin_request(request);
        let input = (endpoint_url.to_string(), plugin_request);
        let input_json = serde_json::to_string(&input).map_err(|e| {
            Error::Backend(format!(
                "Adapter plugin '{}' failed to serialize request: {}",
                self.name, e
            ))
        })?;

        let mut plugin = self
            .pool
            .get(self.timeout)
            .map_err(|e| {
                Error::Backend(format!("Adapter plugin '{}' unavailable: {}", self.name, e))
            })?
            .ok_or_else(|| Error::Backend(format!("Adapter plugin '{}' timed out", self.name)))?;

        let output: String = plugin
            .call::<&str, String>("build_request", &input_json)
            .map_err(|e| {
                Error::Backend(format!(
                    "Adapter plugin '{}' build_request failed: {}",
                    self.name, e
                ))
            })?;
        let maybe_request: Option<thalamus_plugin::types::HttpRequest> =
            serde_json::from_str(&output).map_err(|e| {
                Error::Backend(format!(
                    "Adapter plugin '{}' returned invalid request: {}",
                    self.name, e
                ))
            })?;
        let http_request = maybe_request.ok_or_else(|| {
            Error::Backend(format!(
                "Adapter plugin '{}' refused to build request",
                self.name
            ))
        })?;

        let mut builder = client.request(
            reqwest::Method::from_bytes(http_request.method.as_bytes())
                .map_err(|e| Error::Backend(format!("Invalid HTTP method: {e}")))?,
            &http_request.url,
        );

        for (key, value) in &http_request.headers {
            builder = builder.header(key, value);
        }

        builder = builder.timeout(timeout).json(&http_request.body);
        builder = apply_backend_auth(builder, auth);

        builder.build().map_err(Error::from)
    }

    fn parse_response(&self, status: StatusCode, body: &[u8]) -> Result<ChatResponse> {
        let body_json: serde_json::Value = serde_json::from_slice(body)
            .map_err(|e| Error::Backend(format!("Invalid JSON body: {e}")))?;

        let http_response = thalamus_plugin::types::HttpResponse {
            status: status.as_u16(),
            headers: Vec::new(),
            body: body_json,
        };

        let mut plugin = self
            .pool
            .get(self.timeout)
            .map_err(|e| {
                Error::Backend(format!("Adapter plugin '{}' unavailable: {}", self.name, e))
            })?
            .ok_or_else(|| Error::Backend(format!("Adapter plugin '{}' timed out", self.name)))?;

        let http_response_json = serde_json::to_string(&http_response).map_err(|e| {
            Error::Backend(format!(
                "Adapter plugin '{}' failed to serialize response: {}",
                self.name, e
            ))
        })?;

        let output: String = plugin
            .call::<&str, String>("parse_response", &http_response_json)
            .map_err(|e| {
                Error::Backend(format!(
                    "Adapter plugin '{}' parse_response failed: {}",
                    self.name, e
                ))
            })?;
        let maybe_response: Option<thalamus_plugin::types::ChatResponse> =
            serde_json::from_str(&output).map_err(|e| {
                Error::Backend(format!(
                    "Adapter plugin '{}' returned invalid response: {}",
                    self.name, e
                ))
            })?;
        let response = maybe_response.ok_or_else(|| {
            Error::Backend(format!(
                "Adapter plugin '{}' refused to parse response",
                self.name
            ))
        })?;

        Ok(from_plugin_response(response))
    }

    fn parse_stream(
        &self,
        _body_stream: Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>> {
        Box::pin(futures::stream::iter(vec![Err(Error::Backend(
            "Streaming not supported for adapter plugins in v1".to_string(),
        ))]))
    }

    fn build_embedding_request(
        &self,
        _client: &reqwest::Client,
        _endpoint_url: &str,
        _request: &EmbeddingRequest,
        _auth: &AuthConfig,
        _timeout: Duration,
    ) -> Result<reqwest::Request> {
        Err(Error::Backend(
            "Embeddings not supported for adapter plugins in v1".to_string(),
        ))
    }

    fn parse_embedding_response(
        &self,
        _status: StatusCode,
        _body: &[u8],
    ) -> Result<serde_json::Value> {
        Err(Error::Backend(
            "Embeddings not supported for adapter plugins in v1".to_string(),
        ))
    }
}
