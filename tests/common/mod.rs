//! Common test utilities and fixtures for E2E testing
//!
//! This module provides comprehensive infrastructure for end-to-end testing:
//! - WireMock-based mock LLM backends
//! - Builder-pattern fixtures for test data
//! - Transaction-based test isolation
//! - Configuration builders
//! - HTTP testing helpers

// Re-export submodules
pub mod config_builder;
pub mod fixtures;
pub mod transactional;
pub mod wiremock_backends;

// Re-export commonly used types
pub use config_builder::{BackendConfigBuilder, RoutingConfigBuilder};
pub use fixtures::{
    EmbeddingsRequestBuilder, LlmRequestBuilder, RequestFormat, ResponseAsserter, TestApiKey,
    TestApiKeyBuilder, TestUser, TestUserBuilder, response_parsers,
};
pub use transactional::{
    TestContext, init_test_state, init_test_state_with_backends, init_test_state_with_config,
};
pub use wiremock_backends::{
    ChatCompletionResponseBuilder, MockBackendCluster, MockLlmBackend, StreamingResponseBuilder,
};

use sqlx::PgPool;
use std::sync::Arc;
use std::sync::Once;

use thalamus::features::backends::infra::{AdaptingBackendClient, InMemoryBackendRegistry};
use thalamus::features::llm_proxy::domain::ProxyService;
use thalamus::features::plugin::guardrail_bridge::GuardrailService;
use thalamus::features::routing::infra::RouterService;

static INIT: Once = Once::new();

/// Initialize test logging (call once per test process)
pub fn init_test_logging() {
    INIT.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("thalamus=debug,sqlx=warn,wiremock=info")
            .with_test_writer()
            .try_init();
    });
}

/// Create a test database pool
///
/// Uses the DATABASE_URL from .env.test or defaults to system user on localhost:5432
pub async fn create_test_pool() -> PgPool {
    let database_url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        let user = std::env::var("USER").unwrap_or_else(|_| "postgres".to_string());
        format!("postgres://{user}@localhost:5432/thalamus_test")
    });

    PgPool::connect(&database_url)
        .await
        .expect("Failed to connect to test database")
}

/// Run migrations on the test database
pub async fn run_migrations(pool: &PgPool) {
    sqlx::migrate!("./migrations")
        .run(pool)
        .await
        .expect("Failed to run migrations");
}

/// Clean up test data from tables
///
/// This truncates all tables to ensure test isolation.
/// **Note:** When using `#[sqlx::test]`, this is not needed as transactions auto-rollback.
#[allow(dead_code)]
pub async fn cleanup_database(pool: &PgPool) {
    // Clean up test data (keep default data from migration)
    let _ = sqlx::query("DELETE FROM request_logs WHERE created_at > NOW() - INTERVAL '1 hour'")
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM usage_logs WHERE created_at > NOW() - INTERVAL '1 hour'")
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM api_keys WHERE created_at > NOW() - INTERVAL '1 hour'")
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM team_memberships WHERE user_id NOT IN (SELECT id FROM users WHERE username = 'admin')")
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM users WHERE username != 'admin'")
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM teams WHERE name != 'default'")
        .execute(pool)
        .await;
}

/// Legacy Test fixtures builder (deprecated, use builder patterns instead)
///
/// For new tests, prefer using the individual builders from `fixtures` module.
pub struct TestFixtures {
    pool: PgPool,
}

impl TestFixtures {
    #[allow(dead_code)]
    pub async fn new() -> Self {
        let pool = create_test_pool().await;
        run_migrations(&pool).await;
        cleanup_database(&pool).await;
        Self { pool }
    }

    #[expect(dead_code)]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Create a test user (deprecated, use `TestUserBuilder`)
    #[expect(dead_code)]
    pub async fn create_user(&self, _username: &str, _email: &str) -> uuid::Uuid {
        // Will be implemented when we add user tables
        uuid::Uuid::new_v4()
    }

    /// Create a test team (deprecated, use fixtures module)
    #[expect(dead_code)]
    pub async fn create_team(&self, _name: &str) -> uuid::Uuid {
        // Will be implemented when we add team tables
        uuid::Uuid::new_v4()
    }
}

impl Drop for TestFixtures {
    fn drop(&mut self) {
        // Cleanup happens automatically via PgPool drop
    }
}

/// Initialize a test AppState (legacy, non-transactional)
///
/// **Deprecated:** Use `#[sqlx::test]` with `init_test_state(pool)` instead
/// for proper transaction-based test isolation.
#[allow(dead_code)]
pub async fn init_test_state_legacy() -> thalamus::bootstrap::AppState {
    let pool = create_test_pool().await;
    run_migrations(&pool).await;

    // Initialize global task pool for tests
    thalamus::features::auth::infra::init_task_db_pool(pool.clone());

    let config = thalamus::shared::config::types::Config {
        server: thalamus::shared::config::types::ServerConfig {
            host: "127.0.0.1".to_string(),
            port: 0, // Ephemeral port
            workers: None,
            base_url: None,
        },
        database: thalamus::shared::config::types::DatabaseConfig {
            url: std::env::var("DATABASE_URL")
                .unwrap_or_else(|_| "postgres://postgres@localhost:5432/thalamus_test".to_string()),
            max_connections: 5,
            min_connections: 1,
            pool_timeout: "30s".to_string(),
            idle_timeout: "10m".to_string(),
            max_lifetime: "30m".to_string(),
        },
        backends: std::collections::HashMap::new(),
        routing: thalamus::shared::config::types::RoutingConfig {
            strategy: thalamus::shared::config::types::StrategyConfig {
                name: "round_robin".to_string(),
                prefer_loaded_models: true,
                consider_queue_depth: true,
                fallback_strategy: "round_robin".to_string(),
                hysteresis_threshold: 0.10,
                health_weighted: false,
                admission_control: true,
            },
            priority_queues: {
                let mut map = std::collections::HashMap::new();
                map.insert(
                    "realtime".to_string(),
                    thalamus::shared::config::types::QueueConfig {
                        priority: 1,
                        max_queue_size: 100,
                        timeout: "30s".to_string(),
                    },
                );
                map
            },
            default_queue: "realtime".to_string(),
        },
        observability: thalamus::shared::config::types::ObservabilityConfig {
            tracing: thalamus::shared::config::types::TracingConfig {
                enabled: false,
                level: "info".to_string(),
                format: "json".to_string(),
                otlp_endpoint: None,
                sample_rate: 1.0,
            },
            metrics: thalamus::shared::config::types::MetricsConfig {
                enabled: false,
                prometheus_endpoint: "/metrics".to_string(),
                collection_interval: "10s".to_string(),
                include_per_backend: true,
                include_per_model: true,
            },
            logging_per_team: None,
        },
        cache: None,
        rate_limiting: None,
        oauth_providers: Vec::new(),
        security: thalamus::shared::config::types::SecurityConfig {
            api_key_secret: "test_secret_key_must_be_at_least_32_bytes_long".to_string(),
            paseto_secret_key: "exactly_32_bytes_for_paseto_key!".to_string(),
            opaque_server_setup: "test_opaque_setup".to_string(),
        },
        plugins: None,
    };
    let tasks = axum_tasks::AppTasks::new();

    // Create OAuth service with empty providers for tests
    let oauth_service = std::sync::Arc::new(
        thalamus::features::auth::infra::OAuthService::new(&config.oauth_providers)
            .expect("Failed to create OAuth service for tests"),
    );

    // Initialize backend registry and proxy pipeline for tests
    let backend_registry = Arc::new(InMemoryBackendRegistry::from_config(&config.backends));
    let adapters = AdaptingBackendClient::adapters_from_config(&config, None);

    let http_client = reqwest::Client::builder()
        .pool_idle_timeout(Some(std::time::Duration::from_secs(90)))
        .pool_max_idle_per_host(32)
        .build()
        .expect("Failed to create HTTP client for tests");

    let backend_client = Arc::new(AdaptingBackendClient::new(
        http_client,
        backend_registry.clone(),
        adapters,
    ));
    let router_service = Arc::new(RouterService::from_config(
        backend_registry.clone(),
        &config.routing,
        None,
    ));
    let proxy = Arc::new(ProxyService::new(
        router_service,
        backend_client,
        backend_registry.clone(),
        GuardrailService::empty(),
    ));

    // Initialize team repositories (needed for compilation)
    let team_repository = Arc::new(thalamus::features::teams::infra::SqlxTeamRepository::new(
        pool.clone(),
    ));
    let membership_repository =
        Arc::new(thalamus::features::teams::infra::SqlxMembershipRepository::new(pool.clone()));
    let project_repository =
        Arc::new(thalamus::features::teams::infra::SqlxProjectRepository::new(pool.clone()));
    let team_hierarchy_resolver =
        Arc::new(thalamus::features::teams::infra::SqlxTeamHierarchyResolver::new(pool.clone()));
    // Use a simple stub permission service for tests
    let team_permission_service: Arc<dyn thalamus::features::teams::domain::TeamPermissionService> =
        Arc::new(
            thalamus::features::teams::infra::CasbinTeamPermissionService::new(Arc::new(
                thalamus::features::authorization::CasbinAuthorizer::new(pool.clone())
                    .await
                    .expect("Failed to create Casbin authorizer"),
            )),
        );

    thalamus::bootstrap::AppState {
        db_pool: pool,
        config: Arc::new(config),
        tasks,
        rate_limiter: None,
        authorizer: None,
        oauth_service,
        backend_registry,
        proxy,
        plugin_manager: None,
        team_repository,
        membership_repository,
        project_repository,
        team_hierarchy_resolver,
        team_permission_service,
    }
}

/// HTTP testing helpers
pub mod http {
    use axum::body::{Body, to_bytes};
    use axum::http::StatusCode;
    use axum::response::Response;
    use serde_json::Value;
    use std::collections::HashMap;

    /// Extract JSON body from response
    pub async fn extract_json(response: Response) -> Value {
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("Failed to read body");
        serde_json::from_slice(&bytes).expect("Failed to parse JSON")
    }

    /// Extract text body from response
    pub async fn extract_text(response: Response) -> String {
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("Failed to read body");
        String::from_utf8(bytes.to_vec()).expect("Invalid UTF-8")
    }

    /// Extract SSE stream as vector of events
    pub async fn extract_sse(response: Response) -> Vec<String> {
        let text = extract_text(response).await;
        text.lines()
            .filter(|line| !line.is_empty())
            .map(|line| line.to_string())
            .collect()
    }

    /// Build headers for API key authentication
    pub fn api_key_headers(api_key: &str) -> HashMap<String, String> {
        let mut headers = HashMap::new();
        headers.insert("Authorization".to_string(), format!("Bearer {}", api_key));
        headers.insert("Content-Type".to_string(), "application/json".to_string());
        headers
    }

    /// Build headers for bearer token authentication
    pub fn bearer_headers(token: &str) -> HashMap<String, String> {
        let mut headers = HashMap::new();
        headers.insert("Authorization".to_string(), format!("Bearer {}", token));
        headers.insert("Content-Type".to_string(), "application/json".to_string());
        headers
    }

    /// Assert response has expected status
    pub fn assert_status(response: &Response, expected: StatusCode) {
        assert_eq!(
            response.status(),
            expected,
            "Expected status {}, got {}. Body: {:?}",
            expected,
            response.status(),
            response.body()
        );
    }

    /// Assert response is successful (2xx)
    pub fn assert_success(response: &Response) {
        assert!(
            response.status().is_success(),
            "Expected success status, got {}. Body: {:?}",
            response.status(),
            response.body()
        );
    }
}
