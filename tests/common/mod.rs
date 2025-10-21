//! Common test utilities and fixtures

use sqlx::PgPool;
use std::sync::Once;

static INIT: Once = Once::new();

/// Initialize test logging (call once per test process)
pub fn init_test_logging() {
    INIT.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("thalmus=debug,sqlx=warn")
            .with_test_writer()
            .try_init();
    });
}

/// Create a test database pool
///
/// Uses the DATABASE_URL from .env.test or defaults to localhost:5433
pub async fn create_test_pool() -> PgPool {
    let database_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@localhost:5433/thalmus_test".to_string());

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

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Create a test user (placeholder for when we implement user management)
    #[allow(dead_code)]
    pub async fn create_user(&self, _username: &str, _email: &str) -> uuid::Uuid {
        // Will be implemented when we add user tables
        uuid::Uuid::new_v4()
    }

    /// Create a test team (placeholder)
    #[allow(dead_code)]
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
