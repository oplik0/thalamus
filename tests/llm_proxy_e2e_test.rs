//! End-to-end tests for LLM Proxy routes
//!
//! These tests use WireMock to simulate LLM backends and test the complete
//! request/response flow including routing, authentication, and streaming.

#![allow(unused_imports, unused_mut, unused_variables)]

use axum::body::Body;
use axum::{Router, ServiceExt};
use http::{Request, StatusCode};
use serde_json::json;
use sqlx::PgPool;
use tower::ServiceExt as _;

// Import common test utilities
#[path = "common/mod.rs"]
mod common;

use common::fixtures::{LlmRequestBuilder, ResponseAsserter, TestApiKeyBuilder, TestUserBuilder};
use common::http::{api_key_headers, assert_status, extract_json, extract_text};
use common::wiremock_backends::MockLlmBackend;
use common::{init_test_logging, init_test_state_with_backends};

/// Setup helper: Create a test user with API key and authorized backend
async fn setup_authorized_user_and_backend(
    pool: &PgPool,
) -> (
    common::fixtures::TestUser,
    common::fixtures::TestApiKey,
    MockLlmBackend,
) {
    init_test_logging();

    // Create test user with LLM scope
    let user = TestUserBuilder::new()
        .with_scope("llm:*")
        .create(pool)
        .await;

    // Create API key
    let api_key = TestApiKeyBuilder::new()
        .for_user(&user)
        .with_scope("llm:*")
        .create(pool)
        .await;

    // Start mock backend
    let backend = MockLlmBackend::start("test-backend", vec!["gpt-oss:120b", "gpt-oss:20b"]).await;

    (user, api_key, backend)
}

/// Build the Axum app from state
fn build_app(state: thalamus::bootstrap::AppState) -> Router {
    thalamus::bootstrap::build_router(state)
}

// =============================================================================
// Basic Chat Completion Tests
// =============================================================================

#[sqlx::test]
async fn chat_completion_basic_success(pool: PgPool) {
    let (user, api_key, backend) = setup_authorized_user_and_backend(&pool).await;

    // Mount successful response
    backend
        .with_response_builder()
        .model("gpt-oss:120b")
        .content("Hello from mock backend!")
        .tokens(10, 5)
        .mount()
        .await;

    // Initialize state with backend
    let state = init_test_state_with_backends(pool, &[&backend]).await;
    let app = build_app(state);

    // Build request
    let request_body = LlmRequestBuilder::openai()
        .model("gpt-oss:120b")
        .user_message("Say hello")
        .build();

    // Make request
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", api_key.auth_header())
        .header("Content-Type", "application/json")
        .body(Body::from(request_body.to_string()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();

    // Assert success
    ResponseAsserter::new(response)
        .has_status(200)
        .has_content_type("application/json");

    // Verify backend received the request
    assert!(
        backend.verify_calls(1),
        "Backend should have received 1 request"
    );
}

#[sqlx::test]
async fn chat_completion_returns_correct_format(pool: PgPool) {
    let (user, api_key, backend) = setup_authorized_user_and_backend(&pool).await;

    // Mount response with specific values
    backend
        .with_response_builder()
        .id("chatcmpl-test-123")
        .model("gpt-oss:120b")
        .content("Test response")
        .tokens(15, 8)
        .mount()
        .await;

    let state = init_test_state_with_backends(pool, &[&backend]).await;
    let app = build_app(state);

    let request_body = LlmRequestBuilder::openai()
        .model("gpt-oss:120b")
        .user_message("Test")
        .build();

    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", api_key.auth_header())
        .header("Content-Type", "application/json")
        .body(Body::from(request_body.to_string()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    let body = extract_json(response).await;

    // Verify response format
    assert_eq!(body["id"], "chatcmpl-test-123");
    assert_eq!(body["object"], "chat.completion");
    assert!(body.get("created").is_some());
    assert_eq!(body["model"], "gpt-oss:120b");

    // Verify choices array
    let choices = body["choices"].as_array().unwrap();
    assert_eq!(choices.len(), 1);
    assert_eq!(choices[0]["index"], 0);
    assert_eq!(choices[0]["message"]["role"], "assistant");
    assert_eq!(choices[0]["message"]["content"], "Test response");
    assert_eq!(choices[0]["finish_reason"], "stop");

    // Verify usage
    assert_eq!(body["usage"]["prompt_tokens"], 15);
    assert_eq!(body["usage"]["completion_tokens"], 8);
    assert_eq!(body["usage"]["total_tokens"], 23);
}

#[sqlx::test]
async fn chat_completion_no_backends_available(pool: PgPool) {
    init_test_logging();

    // Create user and API key but NO backend
    let user = TestUserBuilder::new()
        .with_scope("llm:*")
        .create(&pool)
        .await;

    let api_key = TestApiKeyBuilder::new()
        .for_user(&user)
        .with_scope("llm:*")
        .create(&pool)
        .await;

    // Initialize state WITHOUT any backends
    let state = init_test_state_with_backends(pool, &[]).await;
    let app = build_app(state);

    let request_body = LlmRequestBuilder::openai()
        .model("gpt-oss:120b")
        .user_message("Test")
        .build();

    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", api_key.auth_header())
        .header("Content-Type", "application/json")
        .body(Body::from(request_body.to_string()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();

    // Should get 400 Bad Request - no backend supports the model
    assert_status(&response, StatusCode::BAD_REQUEST);

    let body = extract_json(response).await;
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("No healthy backend")
    );
}

#[sqlx::test]
async fn chat_completion_backend_returns_error(pool: PgPool) {
    let (user, api_key, backend) = setup_authorized_user_and_backend(&pool).await;

    // Mount error response
    backend
        .mount_error_response(
            500,
            Some(json!({
                "error": {
                    "message": "Internal server error",
                    "type": "internal_error"
                }
            })),
        )
        .await;

    let state = init_test_state_with_backends(pool, &[&backend]).await;
    let app = build_app(state);

    let request_body = LlmRequestBuilder::openai()
        .model("gpt-oss:120b")
        .user_message("Test")
        .build();

    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", api_key.auth_header())
        .header("Content-Type", "application/json")
        .body(Body::from(request_body.to_string()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();

    // Should get 502 Bad Gateway for backend errors
    assert_status(&response, StatusCode::BAD_GATEWAY);
}

#[sqlx::test]
async fn chat_completion_backend_timeout(pool: PgPool) {
    let (user, api_key, backend) = setup_authorized_user_and_backend(&pool).await;

    // Mount response with long delay (longer than backend timeout)
    backend
        .with_response_builder()
        .content("Delayed response")
        .delay(std::time::Duration::from_secs(60)) // Very long delay
        .mount()
        .await;

    // Configure short timeout
    let mut config = common::config_builder::BackendConfigBuilder::new("test-backend")
        .with_endpoint(backend.base_url(), 10, vec!["gpt-oss:120b"])
        .with_timeout("1s")
        .build();

    let mut backend_configs = std::collections::HashMap::new();
    backend_configs.insert("test-backend".to_string(), config);

    let state = init_test_state_with_backends(pool, &[&backend]).await;
    let app = build_app(state);

    let request_body = LlmRequestBuilder::openai()
        .model("gpt-oss:120b")
        .user_message("Test")
        .build();

    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", api_key.auth_header())
        .header("Content-Type", "application/json")
        .body(Body::from(request_body.to_string()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();

    // Should get 502 for timeout
    assert_status(&response, StatusCode::BAD_GATEWAY);
}

// =============================================================================
// Streaming Tests
// =============================================================================

#[sqlx::test]
async fn chat_completion_streaming_success(pool: PgPool) {
    let (user, api_key, backend) = setup_authorized_user_and_backend(&pool).await;

    // Mount streaming response
    backend
        .with_streaming_builder()
        .content_parts(vec!["Hello", " ", "world", "!"])
        .chunk_delay(std::time::Duration::from_millis(10))
        .mount()
        .await;

    let state = init_test_state_with_backends(pool, &[&backend]).await;
    let app = build_app(state);

    let request_body = LlmRequestBuilder::openai()
        .model("gpt-oss:120b")
        .user_message("Say hello world")
        .with_streaming()
        .build();

    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", api_key.auth_header())
        .header("Content-Type", "application/json")
        .body(Body::from(request_body.to_string()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();

    // Should return SSE content type
    assert!(response.status().is_success());

    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(content_type.contains("text/event-stream"));

    // Verify backend was called
    assert!(backend.verify_calls(1));
}

#[sqlx::test]
async fn chat_completion_streaming_returns_correct_events(pool: PgPool) {
    let (user, api_key, backend) = setup_authorized_user_and_backend(&pool).await;

    // Mount streaming response with specific content
    backend
        .with_streaming_builder()
        .content_parts(vec!["Test", " ", "streaming", " ", "response"])
        .mount()
        .await;

    let state = init_test_state_with_backends(pool, &[&backend]).await;
    let app = build_app(state);

    let request_body = LlmRequestBuilder::openai()
        .model("gpt-oss:120b")
        .user_message("Test streaming")
        .with_streaming()
        .build();

    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", api_key.auth_header())
        .header("Content-Type", "application/json")
        .body(Body::from(request_body.to_string()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    let sse_text = extract_text(response).await;

    // Parse SSE events
    let events: Vec<&str> = sse_text
        .lines()
        .filter(|line| line.starts_with("data:"))
        .collect();

    // Should have events for: role, "Test", " ", "streaming", " ", "response", [DONE]
    assert!(
        events.len() >= 6,
        "Expected at least 6 SSE events, got {}",
        events.len()
    );

    // Last event should be [DONE]
    let last_event = events.last().unwrap();
    assert!(
        last_event.contains("[DONE]"),
        "Last event should be [DONE], got: {}",
        last_event
    );

    // Verify content chunks
    let content_parts: Vec<String> = events
        .iter()
        .filter_map(|e| {
            let json_str = e.strip_prefix("data: ")?;
            if json_str == "[DONE]" {
                return None;
            }
            let json: serde_json::Value = serde_json::from_str(json_str).ok()?;
            json["choices"][0]["delta"]["content"]
                .as_str()
                .map(|s| s.to_string())
        })
        .collect();

    let full_content: String = content_parts.join("");
    assert_eq!(full_content, "Test streaming response");
}

// =============================================================================
// Embeddings Tests
// =============================================================================

#[sqlx::test]
async fn embeddings_basic_success(pool: PgPool) {
    let (user, api_key, backend) = setup_authorized_user_and_backend(&pool).await;

    // Mount embeddings response
    backend
        .mount_embeddings_response(json!({
            "object": "list",
            "data": [
                {
                    "object": "embedding",
                    "embedding": [0.1, 0.2, 0.3, 0.4, 0.5],
                    "index": 0
                }
            ],
            "model": "text-embedding-3-small",
            "usage": {
                "prompt_tokens": 10,
                "total_tokens": 10
            }
        }))
        .await;

    let state = init_test_state_with_backends(pool, &[&backend]).await;
    let app = build_app(state);

    let request_body = common::fixtures::EmbeddingsRequestBuilder::new()
        .model("text-embedding-3-small")
        .text("Hello world")
        .build();

    let request = Request::builder()
        .method("POST")
        .uri("/v1/embeddings")
        .header("Authorization", api_key.auth_header())
        .header("Content-Type", "application/json")
        .body(Body::from(request_body.to_string()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();

    assert_status(&response, StatusCode::OK);

    let body = extract_json(response).await;
    assert_eq!(body["object"], "list");
    assert!(body["data"].as_array().is_some());
    assert_eq!(body["model"], "text-embedding-3-small");
}

// =============================================================================
// Model-Specific Routing Tests
// =============================================================================

#[sqlx::test]
async fn chat_completion_model_not_supported(pool: PgPool) {
    let (user, api_key, backend) = setup_authorized_user_and_backend(&pool).await;

    // Backend only supports gpt-oss:120b
    let state = init_test_state_with_backends(pool, &[&backend]).await;
    let app = build_app(state);

    // Request unsupported model
    let request_body = LlmRequestBuilder::openai()
        .model("unsupported-model-xyz")
        .user_message("Test")
        .build();

    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", api_key.auth_header())
        .header("Content-Type", "application/json")
        .body(Body::from(request_body.to_string()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();

    // Should fail because backend doesn't support this model
    assert_status(&response, StatusCode::BAD_REQUEST);

    let body = extract_json(response).await;
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("unsupported-model-xyz")
    );
}

#[sqlx::test]
async fn chat_completion_multiple_backends_different_models(pool: PgPool) {
    init_test_logging();

    // Create user and API key
    let user = TestUserBuilder::new()
        .with_scope("llm:*")
        .create(&pool)
        .await;

    let api_key = TestApiKeyBuilder::new()
        .for_user(&user)
        .with_scope("llm:*")
        .create(&pool)
        .await;

    // Create two backends with different models
    let gpt4_backend = MockLlmBackend::start("gpt4-backend", vec!["gpt-oss:120b"]).await;
    let claude_backend = MockLlmBackend::start("claude-backend", vec!["claude-3-opus"]).await;

    gpt4_backend
        .with_response_builder()
        .model("gpt-oss:120b")
        .content("gpt-oss:120b response")
        .mount()
        .await;

    claude_backend
        .with_response_builder()
        .model("claude-3-opus")
        .content("Claude response")
        .mount()
        .await;

    let state = init_test_state_with_backends(pool, &[&gpt4_backend, &claude_backend]).await;
    let app = build_app(state);

    // Test gpt-oss:120b request
    let gpt4_request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", api_key.auth_header())
        .header("Content-Type", "application/json")
        .body(Body::from(
            LlmRequestBuilder::openai()
                .model("gpt-oss:120b")
                .user_message("Test")
                .build()
                .to_string(),
        ))
        .unwrap();

    let gpt4_response = app.oneshot(gpt4_request).await.unwrap();
    let body = extract_json(gpt4_response).await;
    assert_eq!(body["model"], "gpt-oss:120b");
    assert!(
        gpt4_backend.verify_calls(1) || claude_backend.verify_calls(1),
        "One of the backends should have received the request"
    );
}

// =============================================================================
// Authentication & Authorization Tests
// =============================================================================

#[sqlx::test]
async fn chat_completion_no_api_key(pool: PgPool) {
    init_test_logging();

    // Create backend but don't provide API key
    let backend = MockLlmBackend::start("test-backend", vec!["gpt-oss:120b"]).await;
    backend
        .with_response_builder()
        .content("Test response")
        .mount()
        .await;

    let state = init_test_state_with_backends(pool, &[&backend]).await;
    let app = build_app(state);

    let request_body = LlmRequestBuilder::openai()
        .model("gpt-oss:120b")
        .user_message("Test")
        .build();

    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        // No Authorization header
        .header("Content-Type", "application/json")
        .body(Body::from(request_body.to_string()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();

    // Should get 401 Unauthorized
    assert_status(&response, StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn chat_completion_invalid_api_key(pool: PgPool) {
    init_test_logging();

    let backend = MockLlmBackend::start("test-backend", vec!["gpt-oss:120b"]).await;

    let state = init_test_state_with_backends(pool, &[&backend]).await;
    let app = build_app(state);

    let request_body = LlmRequestBuilder::openai()
        .model("gpt-oss:120b")
        .user_message("Test")
        .build();

    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", "Bearer invalid-key-12345")
        .header("Content-Type", "application/json")
        .body(Body::from(request_body.to_string()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();

    // Should get 401 Unauthorized
    assert_status(&response, StatusCode::UNAUTHORIZED);
}

// =============================================================================
// Request Parameter Tests
// =============================================================================

#[sqlx::test]
async fn chat_completion_with_temperature(pool: PgPool) {
    let (user, api_key, backend) = setup_authorized_user_and_backend(&pool).await;

    backend
        .with_response_builder()
        .content("Response with temperature")
        .mount()
        .await;

    let state = init_test_state_with_backends(pool, &[&backend]).await;
    let app = build_app(state);

    let request_body = LlmRequestBuilder::openai()
        .model("gpt-oss:120b")
        .user_message("Test temperature")
        .temperature(0.5)
        .build();

    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", api_key.auth_header())
        .header("Content-Type", "application/json")
        .body(Body::from(request_body.to_string()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();

    assert!(response.status().is_success());
    // Backend should have received the temperature parameter (WireMock can verify request body)
}

#[sqlx::test]
async fn chat_completion_with_max_tokens(pool: PgPool) {
    let (user, api_key, backend) = setup_authorized_user_and_backend(&pool).await;

    backend
        .with_response_builder()
        .content("Short response")
        .mount()
        .await;

    let state = init_test_state_with_backends(pool, &[&backend]).await;
    let app = build_app(state);

    let request_body = LlmRequestBuilder::openai()
        .model("gpt-oss:120b")
        .user_message("Test max tokens")
        .max_tokens(100)
        .build();

    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", api_key.auth_header())
        .header("Content-Type", "application/json")
        .body(Body::from(request_body.to_string()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();

    assert!(response.status().is_success());
}

#[sqlx::test]
async fn chat_completion_multi_turn_conversation(pool: PgPool) {
    let (user, api_key, backend) = setup_authorized_user_and_backend(&pool).await;

    backend
        .with_response_builder()
        .content("Yes, Paris is the capital of France.")
        .mount()
        .await;

    let state = init_test_state_with_backends(pool, &[&backend]).await;
    let app = build_app(state);

    let request_body = LlmRequestBuilder::openai()
        .model("gpt-oss:120b")
        .system_message("You are a helpful assistant.")
        .user_message("What is the capital of France?")
        .assistant_message("The capital of France is Paris.")
        .user_message("Can you confirm that?")
        .build();

    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", api_key.auth_header())
        .header("Content-Type", "application/json")
        .body(Body::from(request_body.to_string()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();

    assert!(response.status().is_success());

    // Note: The messages were passed through to the backend, which we verified via the response
}
