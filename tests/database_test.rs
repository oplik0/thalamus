//! Database infrastructure tests

#[path = "common/mod.rs"]
mod common;

use common::{create_test_pool, run_migrations};

#[tokio::test]
async fn test_database_connection() {
    common::init_test_logging();

    let pool = create_test_pool().await;

    // Verify we can connect
    let result = sqlx::query("SELECT 1 as value").fetch_one(&pool).await;

    assert!(result.is_ok());
}

#[tokio::test]
async fn test_run_migrations() {
    common::init_test_logging();

    let pool = create_test_pool().await;
    run_migrations(&pool).await;

    // Verify tables exist
    let result: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM teams")
        .fetch_one(&pool)
        .await
        .expect("Failed to query teams table");

    // Teams table should be queryable (no default team seeded anymore)
    assert!(result.0 >= 0, "Should be able to query teams table");
}

#[tokio::test]
async fn test_default_data() {
    common::init_test_logging();

    let pool = create_test_pool().await;
    run_migrations(&pool).await;

    // Verify default Casbin policies exist
    let policy_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM casbin_rule")
        .fetch_one(&pool)
        .await
        .expect("Failed to count casbin rules");

    assert!(policy_count.0 > 0, "Should have default Casbin policies");
}
