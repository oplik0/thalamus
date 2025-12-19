//! Application bootstrap and dependency injection
//!
//! This module wires together all the application components,
//! creates the `AppState`, and builds the Axum router.

use crate::features::auth::infra::OAuthService;
use crate::features::authorization::CasbinAuthorizer;
use crate::middleware::breach_detection::BreachDetector;
use crate::middleware::rate_limit::RateLimiter;
use crate::shared::config::types::Config;
use axum::Router;
use axum_tasks::{AppTasks, HasTasks};
use sqlx::PgPool;
use std::sync::Arc;

/// Application state shared across all handlers
#[derive(Clone, HasTasks)]
pub struct AppState {
    /// Database connection pool
    pub db_pool: PgPool,
    /// Application configuration
    pub config: Config,
    /// Background task queue
    pub tasks: AppTasks,
    /// Rate limiter for API requests
    pub rate_limiter: Option<Arc<RateLimiter>>,
    /// Casbin authorizer for RBAC
    pub authorizer: Option<Arc<CasbinAuthorizer>>,
    /// Breach detector for security monitoring
    pub breach_detector: Option<Arc<BreachDetector>>,
    /// OAuth service for authentication
    pub oauth_service: Arc<OAuthService>,
}

// Manual Debug implementation since AppTasks doesn't implement Debug
impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("db_pool", &"<PgPool>")
            .field("config", &self.config)
            .field("tasks", &"<AppTasks>")
            .field("rate_limiter", &"<RateLimiter>")
            .field("authorizer", &"<CasbinAuthorizer>")
            .field("breach_detector", &"<BreachDetector>")
            .finish()
    }
}

/// Build the application router with all routes and middleware
pub fn build_router(state: AppState) -> Router {
    use axum_tasks::admin_routes;

    Router::new()
        // Health check (no state needed)
        .merge(crate::features::health::router())
        // API key and auth routes
        .merge(crate::features::auth::router())
        // Authorization management routes (admin only)
        .nest("/admin/authz", crate::features::authorization::router())
        // Admin routes for task monitoring (protected by auth middleware)
        // Note: admin_routes returns Router<()> so we need to use a different approach
        // We'll protect individual routes within admin_routes using middleware
        .nest("/admin/tasks", admin_routes::<AppState>())
        // Future stateful routes will go here
        .with_state(state)
}

/// Initialize the application state
///
/// This function:
/// - Loads configuration from the specified KCL file
/// - Connects to the database
/// - Runs database migrations
/// - Initializes shared services
///
/// # Arguments
/// * `config_path` - Path to the KCL configuration file
///
/// # Errors
/// Returns an error if:
/// - Configuration file cannot be loaded or is invalid
/// - Database connection cannot be established
/// - Database migrations fail
pub async fn init_app_state(config_path: &str) -> crate::Result<AppState> {
    tracing::info!("Initializing application state");

    // Load configuration
    let config = crate::shared::config::load_config(config_path)?;
    tracing::info!(
        database_url = %config.database.url,
        max_connections = config.database.max_connections,
        "Configuration loaded"
    );

    // Parse duration strings from config
    let pool_config = crate::shared::database::PoolConfig {
        database_url: config.database.url.clone(),
        max_connections: config.database.max_connections,
        min_connections: config.database.min_connections,
        acquire_timeout: parse_duration(&config.database.pool_timeout)?,
        idle_timeout: parse_duration(&config.database.idle_timeout)?,
        max_lifetime: parse_duration(&config.database.max_lifetime)?,
    };

    // Create database pool
    let db_pool = crate::shared::database::create_pool(&pool_config).await?;

    // Run migrations
    crate::shared::database::run_migrations(&db_pool).await?;

    // Initialize the global database pool for background tasks
    crate::features::auth::infra::init_task_db_pool(db_pool.clone());

    // Initialize background task queue with auto-persist
    let tasks = AppTasks::new().with_auto_persist(|states| {
        let json = serde_json::to_string_pretty(states).unwrap_or_default();
        tokio::spawn(async move {
            if let Err(e) = tokio::fs::write("tasks.json", json).await {
                tracing::warn!(error = %e, "Failed to persist task states");
            }
        });
    });

    // Load persisted task states if available (for crash recovery)
    if let Ok(json) = tokio::fs::read_to_string("tasks.json").await {
        if let Ok(task_states) = serde_json::from_str(&json) {
            tasks.load_state(task_states).await;
            tracing::info!("Loaded persisted task states from tasks.json");
        }
    }

    // Initialize rate limiter if enabled
    let rate_limiter = config.rate_limiting.as_ref().and_then(|rl| {
        if rl.enabled {
            let rate_limit_config = crate::middleware::rate_limit::RateLimitConfig {
                key_rpm: rl.default_requests_per_minute,
                user_rpm: rl.default_requests_per_minute * 2,
                team_rpm: rl.default_requests_per_minute * 10,
                global_rpm: rl.default_requests_per_minute / 2,
                burst_multiplier: rl.burst_size,
            };
            tracing::info!(
                key_rpm = rate_limit_config.key_rpm,
                user_rpm = rate_limit_config.user_rpm,
                team_rpm = rate_limit_config.team_rpm,
                global_rpm = rate_limit_config.global_rpm,
                "Rate limiting enabled"
            );
            Some(Arc::new(RateLimiter::new(rate_limit_config)))
        } else {
            tracing::info!("Rate limiting disabled");
            None
        }
    });

    // Initialize Casbin authorizer
    let authorizer = match CasbinAuthorizer::new(db_pool.clone()).await {
        Ok(authz) => {
            tracing::info!("Casbin authorizer initialized successfully");
            Some(Arc::new(authz))
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to initialize Casbin authorizer");
            None
        }
    };

    // Initialize breach detector
    let breach_detector = {
        let config = crate::middleware::breach_detection::BreachDetectionConfig::default();
        tracing::info!("Breach detector initialized");
        Some(Arc::new(BreachDetector::new(config)))
    };

    // Initialize OAuth service
    let oauth_service = Arc::new(OAuthService::new(&config.oauth_providers)?);
    tracing::info!("OAuth service initialized");

    tracing::info!("Application state initialized successfully");

    Ok(AppState {
        db_pool,
        config,
        tasks,
        rate_limiter,
        authorizer,
        breach_detector,
        oauth_service,
    })
}

/// Parse a duration string (e.g., "30s", "5m", "1h")
///
/// # Errors
/// Returns an error if the duration string is invalid
fn parse_duration(s: &str) -> crate::Result<std::time::Duration> {
    let s = s.trim();
    if s.is_empty() {
        return Err(crate::Error::Config("Empty duration string".to_string()));
    }

    let (num_str, unit) = s.split_at(s.len() - 1);
    let num: u64 = num_str
        .parse()
        .map_err(|e| crate::Error::Config(format!("Invalid duration number '{num_str}': {e}")))?;

    let multiplier = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => {
            return Err(crate::Error::Config(format!(
                "Invalid duration unit '{unit}'. Expected 's', 'm', or 'h'"
            )));
        }
    };

    Ok(std::time::Duration::from_secs(num * multiplier))
}
