//! Transactional test setup with SQLx
//!
//! Provides test isolation using SQLx's `#[sqlx::test]` macro pattern,
//! which automatically wraps each test in a database transaction that
/// rolls back after the test completes.
use std::collections::HashMap;
use std::sync::Arc;

use sqlx::PgPool;

use thalamus::features::backends::infra::{AdaptingBackendClient, InMemoryBackendRegistry};
use thalamus::features::llm_proxy::domain::ProxyService;
use thalamus::features::plugin::guardrail_bridge::GuardrailService;
use thalamus::features::routing::infra::RouterService;
use thalamus::shared::config::types::Config;

use super::wiremock_backends::MockLlmBackend;

/// Initialize an AppState for transactional tests
///
/// This creates an AppState using the provided pool (which may be a transaction
/// managed by `#[sqlx::test]`). All database operations will be within the
/// transaction and automatically rolled back after the test.
///
/// # Arguments
/// * `pool` - Database pool (typically from `#[sqlx::test]`)
///
/// # Example
/// ```rust
/// #[sqlx::test]
/// async fn my_test(pool: PgPool) {
///     let state = init_test_state(pool).await;
///     // Test code here - all DB changes roll back automatically
/// }
/// ```
pub async fn init_test_state(pool: PgPool) -> thalamus::bootstrap::AppState {
    // Initialize global task pool for tests
    thalamus::features::auth::infra::init_task_db_pool(pool.clone());

    let config = create_test_config();
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

/// Initialize an AppState with mock backends
///
/// Creates an AppState pre-configured with WireMock backends for E2E testing.
/// The mock backends are automatically registered in the configuration.
///
/// # Arguments
/// * `pool` - Database pool
/// * `backends` - Mock backends to register (slice of references)
///
/// # Example
/// ```rust
/// #[sqlx::test]
/// async fn test_with_backends(pool: PgPool) {
///     let backend = MockLlmBackend::start("gpt4", vec!["gpt-oss:120b"]).await;
///     let state = init_test_state_with_backends(pool, &[&backend]).await;
///     // Test code
/// }
/// ```
pub async fn init_test_state_with_backends(
    pool: PgPool,
    backends: &[&MockLlmBackend],
) -> thalamus::bootstrap::AppState {
    // Initialize global task pool for tests
    thalamus::features::auth::infra::init_task_db_pool(pool.clone());

    // Build backend configs from mock backends
    let backend_configs: HashMap<String, thalamus::shared::config::types::BackendConfig> = backends
        .iter()
        .map(|b| (b.name().to_string(), b.to_backend_config()))
        .collect();

    let mut config = create_test_config();
    config.backends = backend_configs;

    let tasks = axum_tasks::AppTasks::new();

    // Create OAuth service
    let oauth_service = std::sync::Arc::new(
        thalamus::features::auth::infra::OAuthService::new(&config.oauth_providers)
            .expect("Failed to create OAuth service for tests"),
    );

    // Initialize backend registry with mock backends
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

/// Initialize an AppState with custom configuration
///
/// Allows full customization of the test configuration.
pub async fn init_test_state_with_config(
    pool: PgPool,
    config: Config,
) -> thalamus::bootstrap::AppState {
    // Initialize global task pool for tests
    thalamus::features::auth::infra::init_task_db_pool(pool.clone());

    let tasks = axum_tasks::AppTasks::new();

    // Create OAuth service
    let oauth_service = std::sync::Arc::new(
        thalamus::features::auth::infra::OAuthService::new(&config.oauth_providers)
            .expect("Failed to create OAuth service for tests"),
    );

    // Initialize backend registry with custom backends
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

/// Create default test configuration
fn create_test_config() -> Config {
    use thalamus::shared::config::types::*;

    Config {
        server: ServerConfig {
            host: "127.0.0.1".to_string(),
            port: 0,
            workers: None,
            base_url: None,
        },
        database: DatabaseConfig {
            url: std::env::var("DATABASE_URL")
                .unwrap_or_else(|_| "postgres://postgres@localhost:5432/thalamus_test".to_string()),
            max_connections: 5,
            min_connections: 1,
            pool_timeout: "30s".to_string(),
            idle_timeout: "10m".to_string(),
            max_lifetime: "30m".to_string(),
        },
        backends: HashMap::new(),
        routing: RoutingConfig {
            strategy: StrategyConfig {
                name: "round_robin".to_string(),
                prefer_loaded_models: true,
                consider_queue_depth: true,
                fallback_strategy: "round_robin".to_string(),
                hysteresis_threshold: 0.10,
                health_weighted: false,
                admission_control: true,
            },
            priority_queues: {
                let mut map = HashMap::new();
                map.insert(
                    "realtime".to_string(),
                    QueueConfig {
                        priority: 1,
                        max_queue_size: 100,
                        timeout: "30s".to_string(),
                    },
                );
                map
            },
            default_queue: "realtime".to_string(),
        },
        observability: ObservabilityConfig {
            tracing: TracingConfig {
                enabled: false,
                level: "info".to_string(),
                format: "json".to_string(),
                otlp_endpoint: None,
                sample_rate: 1.0,
            },
            metrics: MetricsConfig {
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
        security: SecurityConfig {
            api_key_secret: "test_secret_key_must_be_at_least_32_bytes_long".to_string(),
            paseto_secret_key: "exactly_32_bytes_for_paseto_key!".to_string(),
            opaque_server_setup: "test_opaque_setup".to_string(),
        },
        plugins: None,
    }
}

/// Test context that automatically cleans up resources
///
/// Wraps an AppState and provides automatic cleanup when dropped.
/// Note: Database cleanup is handled by SQLx transaction rollback.
pub struct TestContext {
    pub state: thalamus::bootstrap::AppState,
    pub mock_backends: Vec<MockLlmBackend>,
}

impl TestContext {
    /// Create a new test context with the given state
    pub fn new(state: thalamus::bootstrap::AppState) -> Self {
        Self {
            state,
            mock_backends: Vec::new(),
        }
    }

    /// Create a new test context with mock backends
    pub fn with_backends(
        state: thalamus::bootstrap::AppState,
        backends: Vec<MockLlmBackend>,
    ) -> Self {
        Self {
            state,
            mock_backends: backends,
        }
    }

    /// Get a reference to the AppState
    pub fn state(&self) -> &thalamus::bootstrap::AppState {
        &self.state
    }

    /// Get a mutable reference to the AppState (rarely needed)
    pub fn state_mut(&mut self) -> &mut thalamus::bootstrap::AppState {
        &mut self.state
    }

    /// Get the database pool
    pub fn pool(&self) -> &PgPool {
        &self.state.db_pool
    }

    /// Reset all mock backends (clears mounted mocks)
    pub async fn reset_backends(&self) {
        for backend in &self.mock_backends {
            backend.reset().await;
        }
    }

    /// Verify total requests across all backends
    pub fn verify_total_backend_calls(&self, expected: usize) -> bool {
        let total: usize = self.mock_backends.iter().map(|b| b.request_count()).sum();
        total == expected
    }
}

impl std::ops::Deref for TestContext {
    type Target = thalamus::bootstrap::AppState;

    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

impl std::ops::DerefMut for TestContext {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.state
    }
}
