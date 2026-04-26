//! WireMock-based mock LLM backends for E2E testing
//!
//! This module provides configurable mock backends using WireMock,
//! allowing tests to simulate various backend behaviors including:
//! - Successful responses (streaming and non-streaming)
//! - Error responses (HTTP errors, timeouts)
//! - Configurable response delays
//! - Health check endpoint simulation
//! - Request verification

use std::collections::HashMap;
use std::time::Duration;

use serde_json::json;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, Request as WireRequest, ResponseTemplate};

use crate::common::config_builder::BackendConfigBuilder;

/// A mock LLM backend powered by WireMock
///
/// Provides a running HTTP server that simulates an LLM backend endpoint
/// with configurable responses for chat completions, embeddings, and health checks.
pub struct MockLlmBackend {
    /// The WireMock server instance
    server: MockServer,
    /// Backend name (identifier)
    name: String,
    /// Supported models
    models: Vec<String>,
    /// Endpoint capacity
    capacity: u32,
    /// Authentication token (if using Bearer auth)
    auth_token: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// Request counter for verification
    request_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

// Note: MockLlmBackend does not implement Clone because MockServer is not Clone.
// If you need to share a backend across multiple places, wrap it in Arc<MockLlmBackend>.

impl std::fmt::Debug for MockLlmBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let token_display = match self.auth_token.lock() {
            Ok(guard) => guard.as_ref().map(|_| "***").unwrap_or("none").to_string(),
            Err(_) => "locked".to_string(),
        };
        f.debug_struct("MockLlmBackend")
            .field("name", &self.name)
            .field("base_url", &self.server.uri())
            .field("models", &self.models)
            .field("capacity", &self.capacity)
            .field("auth_token", &token_display)
            .field(
                "request_count",
                &self.request_count.load(std::sync::atomic::Ordering::SeqCst),
            )
            .finish()
    }
}

impl MockLlmBackend {
    /// Start a new mock backend server
    ///
    /// # Arguments
    /// * `name` - Backend identifier
    /// * `models` - List of supported model names
    ///
    /// # Example
    /// ```rust
    /// let backend = MockLlmBackend::start("gpt-oss-backend", vec!["gpt-oss:120b", "gpt-oss:20b"]).await;
    /// ```
    pub async fn start(name: impl Into<String>, models: Vec<impl Into<String>>) -> Self {
        let server = MockServer::start().await;
        let name = name.into();
        let models: Vec<String> = models.into_iter().map(|m| m.into()).collect();
        let request_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let mut backend = Self {
            server,
            name,
            models,
            capacity: 10,
            auth_token: std::sync::Arc::new(std::sync::Mutex::new(None)),
            request_count,
        };

        // Mount default health endpoint
        backend.mount_default_health_endpoint().await;

        backend
    }

    /// Start a backend with specific capacity
    pub async fn start_with_capacity(
        name: impl Into<String>,
        models: Vec<impl Into<String>>,
        capacity: u32,
    ) -> Self {
        let mut backend = Self::start(name, models).await;
        backend.capacity = capacity;
        backend
    }

    /// Get the base URL of the mock server
    pub fn base_url(&self) -> String {
        self.server.uri()
    }

    /// Get the backend name
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get supported models
    pub fn models(&self) -> &[String] {
        &self.models
    }

    /// Get endpoint capacity
    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    /// Mount a default health check endpoint that returns 200 OK
    async fn mount_default_health_endpoint(&self) {
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&self.server)
            .await;
    }

    /// Mount a health check endpoint with custom response
    ///
    /// # Arguments
    /// * `healthy` - If true, returns 200 OK; if false, returns 503 Service Unavailable
    pub async fn mount_health_endpoint(&self, healthy: bool) {
        let status = if healthy { 200 } else { 503 };
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&self.server)
            .await;
    }

    /// Mount a successful chat completion response
    ///
    /// # Arguments
    /// * `response_body` - The JSON response body to return
    /// * `delay` - Optional response delay
    ///
    /// # Example
    /// ```rust
    /// backend.mount_chat_completion_response(
    ///     json!({
    ///         "id": "test-123",
    ///         "choices": [{"message": {"role": "assistant", "content": "Hello!"}}]
    ///     }),
    ///     Some(Duration::from_millis(100))
    /// ).await;
    /// ```
    pub async fn mount_chat_completion_response(
        &self,
        response_body: serde_json::Value,
        delay: Option<Duration>,
    ) {
        let mut template = ResponseTemplate::new(200)
            .set_body_json(response_body)
            .append_header("content-type", "application/json");

        if let Some(d) = delay {
            template = template.set_delay(d);
        }

        let counter = self.request_count.clone();
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(move |_: &WireRequest| {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                template.clone()
            })
            .mount(&self.server)
            .await;
    }

    /// Mount a successful chat completion response using a builder pattern
    ///
    /// # Example
    /// ```rust
    /// backend
    ///     .with_response_builder()
    ///     .content("Hello from mock!")
    ///     .model("gpt-oss:120b")
    ///     .tokens(10, 5)
    ///     .mount()
    ///     .await;
    /// ```
    pub fn with_response_builder(&self) -> ChatCompletionResponseBuilder<'_> {
        ChatCompletionResponseBuilder::new(self)
    }

    /// Mount a streaming chat completion response (SSE)
    ///
    /// # Arguments
    /// * `chunks` - Vector of JSON objects representing stream chunks
    /// * `delay_between_chunks` - Optional delay between each SSE event
    ///
    /// # Example
    /// ```rust
    /// backend.mount_streaming_response(
    ///     vec![
    ///         json!({"choices": [{"delta": {"role": "assistant"}}]}),
    ///         json!({"choices": [{"delta": {"content": "Hello"}}]}),
    ///         json!({"choices": [{"delta": {"content": "!"}}]}),
    ///     ],
    ///     Some(Duration::from_millis(10))
    /// ).await;
    /// ```
    pub async fn mount_streaming_response(
        &self,
        chunks: Vec<serde_json::Value>,
        delay_between_chunks: Option<Duration>,
    ) {
        // Build SSE body
        let mut sse_body = String::new();
        for chunk in chunks {
            sse_body.push_str(&format!("data: {}\n\n", chunk.to_string()));
        }
        sse_body.push_str("data: [DONE]\n\n");

        let mut template = ResponseTemplate::new(200)
            .set_body_string(sse_body)
            .append_header("content-type", "text/event-stream")
            .append_header("cache-control", "no-cache");

        if let Some(d) = delay_between_chunks {
            template = template.set_delay(d);
        }

        let counter = self.request_count.clone();
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(move |_: &WireRequest| {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                template.clone()
            })
            .mount(&self.server)
            .await;
    }

    /// Mount a streaming response using a builder for convenience
    ///
    /// # Example
    /// ```rust
    /// backend
    ///     .with_streaming_builder()
    ///     .content_parts(vec!["Hello", " ", "world", "!"])
    ///     .chunk_delay(Duration::from_millis(10))
    ///     .mount()
    ///     .await;
    /// ```
    pub fn with_streaming_builder(&self) -> StreamingResponseBuilder<'_> {
        StreamingResponseBuilder::new(self)
    }

    /// Mount an error response
    ///
    /// # Arguments
    /// * `status_code` - HTTP status code to return
    /// * `error_body` - Optional error response body
    ///
    /// # Example
    /// ```rust
    /// backend.mount_error_response(
    ///     500,
    ///     Some(json!({"error": {"message": "Internal server error"}}))
    /// ).await;
    /// ```
    pub async fn mount_error_response(
        &self,
        status_code: u16,
        error_body: Option<serde_json::Value>,
    ) {
        let mut template = ResponseTemplate::new(status_code);

        if let Some(body) = error_body {
            template = template.set_body_json(body);
        }

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(template)
            .mount(&self.server)
            .await;
    }

    /// Mount an embeddings response
    ///
    /// # Arguments
    /// * `response_body` - The JSON response body
    ///
    /// # Example
    /// ```rust
    /// backend.mount_embeddings_response(
    ///     json!({
    ///         "object": "list",
    ///         "data": [{"object": "embedding", "embedding": [0.1, 0.2, 0.3]}]
    ///     })
    /// ).await;
    /// ```
    pub async fn mount_embeddings_response(&self, response_body: serde_json::Value) {
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response_body))
            .mount(&self.server)
            .await;
    }

    /// Mount a response with specific authorization requirement
    ///
    /// Only requests with the specified Bearer token will match
    pub async fn mount_with_auth(
        &self,
        token: impl Into<String>,
        response_body: serde_json::Value,
    ) {
        let token = token.into();
        if let Ok(mut guard) = self.auth_token.lock() {
            *guard = Some(token.clone());
        }

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header(
                "authorization",
                format!("Bearer {}", token).as_str(),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(response_body))
            .mount(&self.server)
            .await;
    }

    /// Reset all mounted mocks (use between tests)
    pub async fn reset(&self) {
        self.server.reset().await;
        // Re-mount default health endpoint
        self.mount_default_health_endpoint().await;
    }

    /// Get the number of requests received
    pub fn request_count(&self) -> usize {
        self.request_count.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Verify that exactly N requests were made
    pub fn verify_calls(&self, expected: usize) -> bool {
        self.request_count() == expected
    }

    /// Generate a BackendConfig for this mock backend
    ///
    /// This creates a properly configured BackendConfig that can be used
    /// with `InMemoryBackendRegistry::from_config()`
    pub fn to_backend_config(&self) -> thalamus::shared::config::types::BackendConfig {
        let auth_token = self
            .auth_token
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_default();

        BackendConfigBuilder::new(&self.name)
            .with_endpoint(&self.base_url(), self.capacity, self.models.clone())
            .with_bearer_auth(auth_token)
            .with_health_check(true, "1s", "3s")
            .build()
    }

    /// Generate a BackendConfig with custom configuration
    pub fn to_backend_config_builder(&self) -> BackendConfigBuilder {
        BackendConfigBuilder::new(&self.name).with_endpoint(
            &self.base_url(),
            self.capacity,
            self.models.clone(),
        )
    }
}

/// Builder for constructing chat completion responses
pub struct ChatCompletionResponseBuilder<'a> {
    backend: &'a MockLlmBackend,
    id: String,
    object: String,
    model: String,
    content: String,
    role: String,
    prompt_tokens: u32,
    completion_tokens: u32,
    finish_reason: String,
    delay: Option<Duration>,
}

impl<'a> ChatCompletionResponseBuilder<'a> {
    fn new(backend: &'a MockLlmBackend) -> Self {
        Self {
            backend,
            id: format!("chatcmpl-test-{}", uuid::Uuid::new_v4()),
            object: "chat.completion".to_string(),
            model: "gpt-oss:120b".to_string(),
            content: "Hello from mock!".to_string(),
            role: "assistant".to_string(),
            prompt_tokens: 10,
            completion_tokens: 5,
            finish_reason: "stop".to_string(),
            delay: None,
        }
    }

    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    pub fn object(mut self, object: impl Into<String>) -> Self {
        self.object = object.into();
        self
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    pub fn content(mut self, content: impl Into<String>) -> Self {
        self.content = content.into();
        self
    }

    pub fn role(mut self, role: impl Into<String>) -> Self {
        self.role = role.into();
        self
    }

    pub fn tokens(mut self, prompt: u32, completion: u32) -> Self {
        self.prompt_tokens = prompt;
        self.completion_tokens = completion;
        self
    }

    pub fn finish_reason(mut self, reason: impl Into<String>) -> Self {
        self.finish_reason = reason.into();
        self
    }

    pub fn delay(mut self, delay: Duration) -> Self {
        self.delay = Some(delay);
        self
    }

    /// Mount the configured response to the backend
    pub async fn mount(self) {
        let response = json!({
            "id": self.id,
            "object": self.object,
            "created": chrono::Utc::now().timestamp(),
            "model": self.model,
            "choices": [{
                "index": 0,
                "message": {
                    "role": self.role,
                    "content": self.content,
                },
                "finish_reason": self.finish_reason,
            }],
            "usage": {
                "prompt_tokens": self.prompt_tokens,
                "completion_tokens": self.completion_tokens,
                "total_tokens": self.prompt_tokens + self.completion_tokens,
            }
        });

        self.backend
            .mount_chat_completion_response(response, self.delay)
            .await;
    }
}

/// Builder for constructing streaming responses
pub struct StreamingResponseBuilder<'a> {
    backend: &'a MockLlmBackend,
    content_parts: Vec<String>,
    model: String,
    id: String,
    role: String,
    chunk_delay: Option<Duration>,
    include_usage: bool,
}

impl<'a> StreamingResponseBuilder<'a> {
    fn new(backend: &'a MockLlmBackend) -> Self {
        Self {
            backend,
            content_parts: vec![
                "Hello".to_string(),
                " ".to_string(),
                "world".to_string(),
                "!".to_string(),
            ],
            model: "gpt-oss:120b".to_string(),
            id: format!("chatcmpl-test-{}", uuid::Uuid::new_v4()),
            role: "assistant".to_string(),
            chunk_delay: None,
            include_usage: false,
        }
    }

    pub fn content_parts(mut self, parts: Vec<impl Into<String>>) -> Self {
        self.content_parts = parts.into_iter().map(|p| p.into()).collect();
        self
    }

    pub fn content(mut self, full_content: impl Into<String>) -> Self {
        // Split content into word-sized chunks for realistic streaming
        let content = full_content.into();
        self.content_parts = content.split_whitespace().map(|s| s.to_string()).collect();
        self
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    pub fn role(mut self, role: impl Into<String>) -> Self {
        self.role = role.into();
        self
    }

    pub fn chunk_delay(mut self, delay: Duration) -> Self {
        self.chunk_delay = Some(delay);
        self
    }

    pub fn with_usage(mut self) -> Self {
        self.include_usage = true;
        self
    }

    /// Mount the configured streaming response
    pub async fn mount(self) {
        let mut chunks: Vec<serde_json::Value> = Vec::new();

        // First chunk: role
        chunks.push(json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": chrono::Utc::now().timestamp(),
            "model": self.model,
            "choices": [{
                "index": 0,
                "delta": {"role": self.role},
                "finish_reason": null,
            }]
        }));

        // Content chunks
        for part in &self.content_parts {
            chunks.push(json!({
                "id": self.id,
                "object": "chat.completion.chunk",
                "created": chrono::Utc::now().timestamp(),
                "model": self.model,
                "choices": [{
                    "index": 0,
                    "delta": {"content": part},
                    "finish_reason": null,
                }]
            }));
        }

        // Final chunk with finish_reason
        let final_chunk = if self.include_usage {
            json!({
                "id": self.id,
                "object": "chat.completion.chunk",
                "created": chrono::Utc::now().timestamp(),
                "model": self.model,
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": "stop",
                }],
                "usage": {
                    "prompt_tokens": 10,
                    "completion_tokens": self.content_parts.len() as u32,
                    "total_tokens": 10 + self.content_parts.len() as u32,
                }
            })
        } else {
            json!({
                "id": self.id,
                "object": "chat.completion.chunk",
                "created": chrono::Utc::now().timestamp(),
                "model": self.model,
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": "stop",
                }]
            })
        };
        chunks.push(final_chunk);

        self.backend
            .mount_streaming_response(chunks, self.chunk_delay)
            .await;
    }
}

/// A collection of multiple mock backends for testing routing strategies
pub struct MockBackendCluster {
    backends: Vec<MockLlmBackend>,
}

impl MockBackendCluster {
    /// Create a new empty cluster
    pub fn new() -> Self {
        Self {
            backends: Vec::new(),
        }
    }

    /// Add a backend to the cluster
    pub fn add(&mut self, backend: MockLlmBackend) {
        self.backends.push(backend);
    }

    /// Get all backends
    pub fn backends(&self) -> &[MockLlmBackend] {
        &self.backends
    }

    /// Convert all backends to BackendConfig map
    pub fn to_backend_configs(
        &self,
    ) -> HashMap<String, thalamus::shared::config::types::BackendConfig> {
        self.backends
            .iter()
            .map(|b| (b.name.clone(), b.to_backend_config()))
            .collect()
    }

    /// Reset all backends
    pub async fn reset_all(&self) {
        for backend in &self.backends {
            backend.reset().await;
        }
    }

    /// Get total request count across all backends
    pub fn total_request_count(&self) -> usize {
        self.backends.iter().map(|b| b.request_count()).sum()
    }

    /// Verify total calls across all backends
    pub fn verify_total_calls(&self, expected: usize) -> bool {
        self.total_request_count() == expected
    }
}

impl Default for MockBackendCluster {
    fn default() -> Self {
        Self::new()
    }
}
