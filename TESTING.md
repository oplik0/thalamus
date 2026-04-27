# Testing Guide

This guide covers everything you need to know about testing Thalamus, from running your first test to writing comprehensive integration tests with mocked LLM backends.

## Quick Start Guide

One-command setup for new developers:

```bash
# Enter the development shell and start services
devenv shell
services-up && db-migrate && test
```

This will:
1. Start PostgreSQL and Redis
2. Run database migrations
3. Execute all tests with cargo-nextest

## Prerequisites

### Nix and devenv

Thalamus uses [Nix](https://nixos.org/) and [devenv](https://devenv.sh/) to provide a fully reproducible development environment. All testing tools are automatically installed when you enter `devenv shell`.

**Required:**
- Nix with flakes enabled
- devenv (`nix profile install nixpkgs#devenv`)

**What you get automatically:**
- Rust toolchain (1.89+)
- cargo-nextest (advanced test runner)
- sqlx-cli (database migrations)
- PostgreSQL and Redis clients
- All environment variables pre-configured

### Environment Variables

When you enter `devenv shell`, these are automatically set:

```bash
DATABASE_URL=postgresql://$USER@localhost:5432/thalamus
TEST_DATABASE_URL=postgresql://$USER@localhost:5432/thalamus_test
REDIS_URL=redis://localhost:6379
```

## Test Structure

Thalamus uses a layered testing approach:

```
tests/
├── common/
│   ├── mod.rs              # Common imports and utilities
│   ├── fixtures.rs         # Builder-pattern test fixtures
│   ├── transactional.rs    # Test state initialization
│   ├── wiremock_backends.rs # Mock LLM backend support
│   └── config_builder.rs   # Backend configuration builders
├── *_test.rs               # Integration test files
└── **/                     # Unit tests in src/ (inline)
```

### Unit Tests vs Integration Tests

**Unit tests** live alongside source code in `src/`:

```rust
// In src/features/health/domain.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_status_default_is_healthy() {
        let status = HealthStatus::default();
        assert!(status.is_healthy());
    }
}
```

**Integration tests** live in `tests/` and test full HTTP request/response cycles with the database.

### The `#[sqlx::test]` Pattern

Integration tests use SQLx's transactional testing pattern for automatic isolation:

```rust
use sqlx::PgPool;

#[sqlx::test]
async fn test_something(pool: PgPool) {
    // This test runs in a database transaction
    // All changes are rolled back automatically after the test
}
```

**Key benefits:**
- Each test gets its own database transaction
- Changes are automatically rolled back (no cleanup needed)
- Tests run in parallel without interference
- No test data pollution between runs

### Test Fixtures Pattern

We use the builder pattern for creating test data. See `tests/common/fixtures.rs`:

```rust
// Creating a test user
let user = TestUserBuilder::new()
    .username("alice")
    .email("alice@example.com")
    .with_scope("llm:*")
    .create(&pool)
    .await;

// Creating an API key
let api_key = TestApiKeyBuilder::new()
    .for_user(&user)
    .with_scope("llm:*")
    .name("Test Key")
    .create(&pool)
    .await;
```

### WireMock Backend Mocking

For E2E tests, we mock LLM backends using WireMock:

```rust
use common::wiremock_backends::MockLlmBackend;

// Start a mock backend
let backend = MockLlmBackend::start("gpt4-backend", vec!["gpt-oss:120b"]).await;

// Configure response
backend
    .with_response_builder()
    .content("Hello from mock!")
    .model("gpt-oss:120b")
    .mount()
    .await;
```

## Running Tests

### Test Commands

| Command | Description |
|---------|-------------|
| `test` | Run all tests with nextest (default profile) |
| `test-verbose` | Run tests with immediate output on success |
| `test-ci` | CI mode: no retries, fail-fast, 30s timeout |
| `test-cargo` | Run tests with standard `cargo test` |
| `ci` | Full CI suite: format check → lint → test-ci |

### Running Specific Tests

```bash
# Run a specific test file
cargo nextest run --test health_test

# Run a specific test by name
cargo nextest run test_health_check_returns_ok

# Run tests matching a pattern
cargo nextest run health

# Run with output visible
cargo nextest run --success-output immediate

# Run a single test with cargo test (for debugging)
cargo test test_health_check_returns_ok -- --nocapture
```

### Test Profiles

**Default profile** (`.config/nextest.toml`):
- 2 retries on failure
- Parallel threads (num-cpus)
- 60s slow timeout, terminate after 2 periods
- No fail-fast (run all tests)

**CI profile**:
- No retries
- Fail-fast on first failure
- 30s slow timeout

## Understanding Test Fixtures

### TestUserBuilder

Creates test users with configurable properties:

```rust
let user = TestUserBuilder::new()
    .username("testuser")
    .email("test@example.com")
    .with_scope("llm:chat")
    .with_scope("llm:embeddings")
    .as_admin()  // Adds admin:* scope
    .create(&pool)
    .await;

// Check user properties
assert!(user.has_scope("llm:chat"));
```

### TestApiKeyBuilder

Creates API keys for authentication:

```rust
let api_key = TestApiKeyBuilder::new()
    .for_user(&user)
    .name("My Test Key")
    .with_scope("llm:*")
    .with_scope("admin:read")
    .expires_in_days(30)
    .create(&pool)
    .await;

// Use in requests
let auth_header = api_key.auth_header();  // "Bearer thalamus_test_..."
```

### LlmRequestBuilder

Builds OpenAI and Anthropic format requests:

```rust
// OpenAI format
let request = LlmRequestBuilder::openai()
    .model("gpt-oss:120b")
    .system_message("You are a helpful assistant")
    .user_message("Hello!")
    .temperature(0.7)
    .max_tokens(100)
    .with_streaming()
    .build();

// Anthropic format
let request = LlmRequestBuilder::anthropic()
    .model("claude-3-opus")
    .user_message("Hello!")
    .build();
```

### MockLlmBackend

Simulates LLM backends for E2E testing:

```rust
// Start mock backend
let backend = MockLlmBackend::start("test-backend", vec!["gpt-oss:120b"]).await;

// Mount a successful response
backend
    .with_response_builder()
    .content("Mocked response")
    .model("gpt-oss:120b")
    .tokens(10, 5)
    .mount()
    .await;

// Mount streaming response
backend
    .with_streaming_builder()
    .content_parts(vec!["Hello", " ", "world", "!"])
    .chunk_delay(Duration::from_millis(10))
    .mount()
    .await;

// Mount error response
backend
    .mount_error_response(500, Some(json!({"error": "Internal error"})))
    .await;

// Verify requests
assert!(backend.verify_calls(1));
```

### ResponseAsserter Helpers

Chainable assertions for HTTP responses:

```rust
use common::fixtures::ResponseAsserter;

ResponseAsserter::new(response)
    .has_status(200)
    .is_success()
    .has_content_type("application/json")
    .into_response();
```

### Response Parsers

Helpers for extracting data from LLM responses:

```rust
use common::fixtures::response_parsers;

// Parse chat completion
let body: Value = extract_json(response).await;
let content = response_parsers::parse_chat_completion(&body);

// Parse streaming chunks
let chunks: Vec<String> = extract_sse(response).await;
for chunk in chunks {
    if let Some(json) = response_parsers::parse_stream_chunk(&chunk) {
        if let Some(content) = response_parsers::extract_chunk_content(&json) {
            print!("{}", content);
        }
    }
}

// Parse embeddings
let embeddings = response_parsers::parse_embeddings(&body);
```

## Writing New Tests

### Template for Transactional Tests

```rust
//! Description of what this test file covers

#[path = "common/mod.rs"]
mod common;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use sqlx::PgPool;
use tower::ServiceExt;

use common::fixtures::{TestUserBuilder, TestApiKeyBuilder, LlmRequestBuilder};
use common::http::{extract_json, api_key_headers};
use common::{init_test_logging, init_test_state_with_backends};
use common::wiremock_backends::MockLlmBackend;

#[sqlx::test]
async fn my_new_test(pool: PgPool) {
    // Initialize logging (optional but recommended)
    init_test_logging();

    // Create test data
    let user = TestUserBuilder::new()
        .with_scope("llm:*")
        .create(&pool)
        .await;

    let api_key = TestApiKeyBuilder::new()
        .for_user(&user)
        .create(&pool)
        .await;

    // Set up mock backend
    let backend = MockLlmBackend::start("test-backend", vec!["gpt-oss:120b"]).await;
    backend
        .with_response_builder()
        .content("Test response")
        .mount()
        .await;

    // Initialize app state with backend
    let state = init_test_state_with_backends(pool, &[&backend]).await;
    let app = thalamus::bootstrap::build_router(state);

    // Build request
    let request_body = LlmRequestBuilder::openai()
        .model("gpt-oss:120b")
        .user_message("Hello")
        .build();

    // Send request
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("Authorization", api_key.auth_header())
                .header("Content-Type", "application/json")
                .body(Body::from(request_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    // Assert
    assert_eq!(response.status(), StatusCode::OK);

    // Verify backend was called
    assert!(backend.verify_calls(1));
}
```

### Key Patterns

1. **Always use `#[sqlx::test]`** for database tests - it provides automatic transaction isolation
2. **Call `init_test_logging()`** at the start of tests to see debug output
3. **Use builders** for test data - they're concise and readable
4. **Reset mock backends** between tests if reusing them: `backend.reset().await`
5. **Verify backend calls** to ensure routing worked correctly

### Testing Without Backends

For tests that don't need LLM backends:

```rust
#[sqlx::test]
async fn test_without_backend(pool: PgPool) {
    init_test_logging();

    let state = init_test_state(pool).await;
    let app = thalamus::bootstrap::build_router(state);

    // Test health, auth, etc.
}
```

### Testing with Custom Configuration

```rust
use common::config_builder::{BackendConfigBuilder, RoutingConfigBuilder};

#[sqlx::test]
async fn test_custom_routing(pool: PgPool) {
    let mut config = create_test_config();

    // Customize routing strategy
    config.routing = RoutingConfigBuilder::new("weighted")
        .with_admission_control(false)
        .build();

    let state = init_test_state_with_config(pool, config).await;
    // ... test code
}
```

## Debugging Failed Tests

### Database Connection Issues

**Symptom:** `Failed to connect to test database`

**Solutions:**
```bash
# Check if PostgreSQL is running
services-up

# Verify database exists
psql $TEST_DATABASE_URL -c "SELECT 1"

# Run migrations
db-migrate

# Reset test database if corrupted
db-reset
```

### Migration Failures

**Symptom:** SQLx compile-time errors or migration panics

**Solutions:**
```bash
# Run migrations with database running
db-migrate

# Check migration status
sqlx migrate info

# Reset and re-run
db-reset

# For offline mode issues (CI/builds)
cargo check  # Regenerates .sqlx/ cache
```

### Test Isolation Problems

**Symptom:** Tests pass individually but fail together

**Check:**
1. Are you using `#[sqlx::test]`? (not raw `#[tokio::test]`)
2. Are mock backends properly reset between uses?
3. Are you cleaning up any shared resources?

**Fix:**
```rust
// Don't reuse backends without resetting
let backend = MockLlmBackend::start("test", vec!["gpt-oss:120b"]).await;

// In each test or after test:
backend.reset().await;
```

### Viewing Test Logs

```bash
# Verbose output (shows all test output)
test-verbose

# Or with cargo:
cargo nextest run --success-output immediate

# For a specific test with full debug output:
RUST_LOG=debug cargo test test_name -- --nocapture

# Thalamus-specific debug:
RUST_LOG=thalamus=debug,sqlx=warn test
```

### Common Error Patterns

**"sqlx_data not found" or offline mode errors:**
```bash
# Regenerate .sqlx/ cache
cargo check

# Or with database available:
SQLX_OFFLINE=false cargo check
```

**Timeout errors in tests:**
```bash
# Run with longer timeout
cargo nextest run --timeout 120

# Or use the CI profile for stricter timeouts:
test-ci
```

## Advanced Topics

### SQLx Offline Mode

The `.sqlx/` directory contains pre-checked query metadata that enables compilation without a running database:

```bash
# Regenerate after modifying SQL queries
SQLX_OFFLINE=false cargo check

# CI builds use offline mode automatically:
SQLX_OFFLINE=true cargo build
```

**Never manually edit `.sqlx/` files** — always regenerate via `cargo check` with database available.

### Nextest Configuration

Configuration in `.config/nextest.toml`:

```toml
[profile.default]
retries = 2                    # Retry flaky tests
test-threads = "num-cpus"      # Parallel execution
slow-timeout = { period = "60s", terminate-after = 2 }
fail-fast = false

[profile.ci]
retries = 0
fail-fast = true
slow-timeout = { period = "30s" }
```

Override in command line:
```bash
cargo nextest run --profile ci
cargo nextest run --retries 0 --test-threads 4
```

### Test Coverage

Test coverage reporting is planned but not yet implemented. Coming soon with `cargo-tarpaulin` or `llvm-cov`.

### Mock Backend Clusters

For testing routing strategies with multiple backends:

```rust
use common::wiremock_backends::MockBackendCluster;

let mut cluster = MockBackendCluster::new();

let backend1 = MockLlmBackend::start_with_capacity("backend-1", vec!["gpt-oss:120b"], 5).await;
let backend2 = MockLlmBackend::start_with_capacity("backend-2", vec!["gpt-oss:120b"], 5).await;

cluster.add(backend1);
cluster.add(backend2);

// Use in test
let state = init_test_state_with_backends(pool, &[
    &cluster.backends()[0],
    &cluster.backends()[1],
]).await;

// Verify routing distributed requests
assert!(cluster.verify_total_calls(2));
```

### HTTP Testing Helpers

Additional helpers in `common::http`:

```rust
use common::http;

// Extract response data
let json = http::extract_json(response).await;
let text = http::extract_text(response).await;
let sse_events = http::extract_sse(response).await;

// Build headers
let headers = http::api_key_headers(&api_key.key);
let headers = http::bearer_headers(&token);

// Assert status
http::assert_status(&response, StatusCode::OK);
http::assert_success(&response);
```

---

For more information, see the [README.md](README.md) and source files in `tests/common/`.
