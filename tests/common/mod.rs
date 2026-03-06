//! Common test utilities and fixtures

use sqlx::PgPool;
use std::sync::Arc;
use std::sync::Once;

use thalamus::features::backends::infra::{AdaptingBackendClient, InMemoryBackendRegistry};
use thalamus::features::llm_proxy::domain::ProxyService;
use thalamus::features::routing::infra::RouterService;

static INIT: Once = Once::new();

/// Initialize test logging (call once per test process)
pub fn init_test_logging() {
    INIT.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("thalamus=debug,sqlx=warn")
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
/// This truncates all tables to ensure test isolation
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

/// Test fixtures builder
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
    // TODO: use the pool in tests
    #[expect(dead_code)]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Create a test user (placeholder for when we implement user management)
    #[expect(dead_code)]
    pub async fn create_user(&self, _username: &str, _email: &str) -> uuid::Uuid {
        // Will be implemented when we add user tables
        uuid::Uuid::new_v4()
    }

    /// Create a test team (placeholder)
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

/// Initialize a test AppState
#[allow(dead_code)]
pub async fn init_test_state() -> thalamus::bootstrap::AppState {
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
    };
    let tasks = axum_tasks::AppTasks::new();

    // Create OAuth service with empty providers for tests
    let oauth_service = std::sync::Arc::new(
        thalamus::features::auth::infra::OAuthService::new(&config.oauth_providers)
            .expect("Failed to create OAuth service for tests"),
    );

    // Initialize backend registry and proxy pipeline for tests
    let backend_registry = Arc::new(InMemoryBackendRegistry::from_config(&config.backends));
    let adapters = AdaptingBackendClient::adapters_from_config(&config);

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
    ));
    let proxy = Arc::new(ProxyService::new(
        router_service,
        backend_client,
        backend_registry.clone(),
    ));

    thalamus::bootstrap::AppState {
        db_pool: pool,
        config,
        tasks,
        rate_limiter: None,
        authorizer: None,
        breach_detector: None,
        oauth_service,
        backend_registry,
        proxy,
    }
}
