//! Application bootstrap and dependency injection
//!
//! This module wires together all the application components,
//! creates the `AppState`, and builds the Axum router.

use crate::features::auth::infra::OAuthService;
use crate::features::authorization::CasbinAuthorizer;
use crate::features::backends::infra::{AdaptingBackendClient, InMemoryBackendRegistry};
use crate::features::llm_proxy::ProxyService;
use crate::features::plugin::PluginManager;
use crate::features::routing::infra::RouterService;
use crate::features::teams::domain::{
    MembershipRepository, ProjectRepository, TeamHierarchyResolver, TeamPermissionService,
    TeamRepository,
};
use crate::features::teams::infra::{
    CasbinTeamPermissionService, SqlxMembershipRepository, SqlxProjectRepository,
    SqlxTeamHierarchyResolver, SqlxTeamRepository,
};
use crate::middleware::rate_limit::RateLimiter;
use crate::shared::config::types::Config;
use axum::Router;
use axum_tasks::{AppTasks, HasTasks};
use sqlx::PgPool;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Application state shared across all handlers
#[derive(Clone, HasTasks)]
pub struct AppState {
    /// Database connection pool
    pub db_pool: PgPool,
    /// Application configuration (Arc for efficient cloning, can be swapped on hot-reload)
    pub config: Arc<Config>,
    /// Background task queue
    pub tasks: AppTasks,
    /// Rate limiter for API requests
    pub rate_limiter: Option<Arc<RateLimiter>>,
    /// Casbin authorizer for RBAC
    pub authorizer: Option<Arc<CasbinAuthorizer>>,
    /// OAuth service for authentication
    pub oauth_service: Arc<OAuthService>,
    /// In-memory backend endpoint registry
    pub backend_registry: Arc<InMemoryBackendRegistry>,
    /// Unified LLM proxy orchestrator
    pub proxy: Arc<ProxyService>,
    /// Plugin manager for WASM plugins
    pub plugin_manager: Option<Arc<PluginManager>>,
    /// Team repository
    pub team_repository: Arc<dyn TeamRepository>,
    /// Membership repository
    pub membership_repository: Arc<dyn MembershipRepository>,
    /// Project repository
    pub project_repository: Arc<dyn ProjectRepository>,
    /// Team hierarchy resolver
    pub team_hierarchy_resolver: Arc<dyn TeamHierarchyResolver>,
    /// Team permission service
    pub team_permission_service: Arc<dyn TeamPermissionService>,
}

// Manual Debug implementation since AppTasks doesn't implement Debug
impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("db_pool", &"<PgPool>")
            .field("config", &"<Config>")
            .field("tasks", &"<AppTasks>")
            .field("rate_limiter", &"<RateLimiter>")
            .field("authorizer", &"<CasbinAuthorizer>")
            .field("backend_registry", &"<InMemoryBackendRegistry>")
            .field("proxy", &"<ProxyService>")
            .field("plugin_manager", &"<PluginManager>")
            .field("team_repository", &"<TeamRepository>")
            .field("membership_repository", &"<MembershipRepository>")
            .field("project_repository", &"<ProjectRepository>")
            .field("team_hierarchy_resolver", &"<TeamHierarchyResolver>")
            .field("team_permission_service", &"<TeamPermissionService>")
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
        // Plugin management routes (admin only)
        .nest("/admin/plugins", crate::features::plugin::api::router())
        // Unified LLM proxy routes
        .merge(crate::features::llm_proxy::router())
        // Teams and projects routes
        .merge(crate::features::teams::router())
        .with_state(state)
}

/// Initialize the application state
///
/// This function:
/// - Uses the provided configuration (already loaded from KCL)
/// - Connects to the database
/// - Runs database migrations
/// - Initializes shared services
///
/// # Arguments
/// * `config` - Arc<Config> for hot-reload support
///
/// # Errors
/// Returns an error if:
/// - Configuration is invalid
/// - Database connection cannot be established
/// - Database migrations fail
pub async fn init_app_state(
    config: Arc<Config>,
    shutdown: CancellationToken,
) -> crate::Result<AppState> {
    tracing::info!("Initializing application state");

    // Get config for initialization
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

    // Initialize OAuth service
    let oauth_providers = config.oauth_providers.clone();
    let oauth_service = Arc::new(OAuthService::new(&oauth_providers)?);
    tracing::info!("OAuth service initialized");

    // Initialize rate limiter if enabled
    let rate_limiter = config.rate_limiting.as_ref().and_then(|rl| {
        if rl.enabled {
            let rate_limit_config = crate::middleware::rate_limit::RateLimitConfig {
                key_rpm: rl.default_requests_per_minute,
                user_rpm: rl.default_requests_per_minute * 2,
                team_rpm: rl.default_requests_per_minute * 10,
                global_rpm: rl.default_requests_per_minute / 2,
                burst_multiplier: rl.burst_size,
                ..crate::middleware::rate_limit::RateLimitConfig::default()
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

    // Initialize backend registry and proxy pipeline
    let backend_registry = Arc::new(InMemoryBackendRegistry::from_config(&config.backends));

    // Initialize plugin manager if plugins are configured
    let plugin_manager = config.plugins.as_ref().and_then(|pc| {
        if pc.enabled {
            match PluginManager::load_from_config(pc) {
                Ok(pm) => {
                    tracing::info!("Plugin manager initialized with {} plugins", pm.list_plugins().len());
                    Some(Arc::new(pm))
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to initialize plugin manager");
                    None
                }
            }
        } else {
            tracing::info!("Plugin system disabled");
            None
        }
    });

    let adapters = AdaptingBackendClient::adapters_from_config(&config);

    let http_client = reqwest::Client::builder()
        .pool_idle_timeout(Some(std::time::Duration::from_secs(90)))
        .pool_max_idle_per_host(32)
        .build()
        .map_err(crate::Error::from)?;

    let backend_client = Arc::new(AdaptingBackendClient::new(
        http_client.clone(),
        backend_registry.clone(),
        adapters,
    ));
    let router_service = Arc::new(RouterService::from_config(
        backend_registry.clone(),
        &config.routing,
        plugin_manager.clone(),
    ));
    let proxy = Arc::new(ProxyService::new(
        router_service,
        backend_client,
        backend_registry.clone(),
    ));

    let health_tasks = crate::features::backends::health::spawn_health_checks(
        http_client,
        backend_registry.clone(),
        &config.backends,
        shutdown.clone(),
    );
    tracing::info!(
        count = health_tasks.len(),
        "Spawned backend health check tasks"
    );

    tracing::info!("Application state initialized successfully");

    // Initialize Casbin authorizer (needs db_pool)
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

    // Initialize team repositories
    let team_repository = Arc::new(SqlxTeamRepository::new(db_pool.clone()));
    let membership_repository = Arc::new(SqlxMembershipRepository::new(db_pool.clone()));
    let project_repository = Arc::new(SqlxProjectRepository::new(db_pool.clone()));
    let team_hierarchy_resolver = Arc::new(SqlxTeamHierarchyResolver::new(db_pool.clone()));

    // Initialize team permission service (needs Casbin authorizer)
    let team_permission_service: Arc<dyn TeamPermissionService> = if let Some(authz) = &authorizer
    {
        Arc::new(CasbinTeamPermissionService::new(authz.clone()))
    } else {
        // Fallback: create a new authorizer just for team permissions (shouldn't happen in normal operation)
        tracing::warn!("No authorizer available for team permission service, creating standalone");
        match CasbinAuthorizer::new(db_pool.clone()).await {
            Ok(authz) => Arc::new(CasbinTeamPermissionService::new(Arc::new(authz))),
            Err(e) => {
                tracing::error!(error = %e, "Failed to create standalone Casbin authorizer for team permissions");
                // This will cause runtime errors if team permissions are used, but allows the app to start
                return Err(crate::Error::Internal(format!(
                    "Failed to initialize team permission service: {}",
                    e
                )));
            }
        }
    };

    tracing::info!("Team repositories initialized successfully");

    Ok(AppState {
        db_pool,
        config,
        tasks,
        rate_limiter,
        authorizer,
        oauth_service,
        backend_registry,
        proxy,
        plugin_manager,
        team_repository,
        membership_repository,
        project_repository,
        team_hierarchy_resolver,
        team_permission_service,
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
