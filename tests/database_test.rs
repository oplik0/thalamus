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

    // Should have at least the default team
    assert!(
        result.0 >= 1,
        "Should have at least one team from migration"
    );
}

#[tokio::test]
async fn test_default_data() {
    common::init_test_logging();

    let pool = create_test_pool().await;
    run_migrations(&pool).await;

    // Verify default team exists
    let team: (String,) = sqlx::query_as("SELECT name FROM teams WHERE name = 'default'")
        .fetch_one(&pool)
        .await
        .expect("Failed to find default team");

    assert_eq!(team.0, "default");

    // Verify default admin user exists
    let user: (String,) = sqlx::query_as("SELECT username FROM users WHERE username = 'admin'")
        .fetch_one(&pool)
        .await
        .expect("Failed to find admin user");

    assert_eq!(user.0, "admin");

    // Verify default Casbin policies exist
    let policy_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM casbin_rule")
        .fetch_one(&pool)
        .await
        .expect("Failed to count casbin rules");

    assert!(policy_count.0 > 0, "Should have default Casbin policies");
}
