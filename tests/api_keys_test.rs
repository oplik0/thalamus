#[path = "common/mod.rs"]
mod common;

use chrono::{Duration, Utc};
use thalamus::features::auth::domain::api_key::CreateApiKeyRequest;
use thalamus::features::auth::domain::keys::{Prefix, generate_key};
use thalamus::features::auth::infra::key_storage::{list_user_keys, revoke_key, validate_key};
use uuid::Uuid;

#[tokio::test]
async fn test_api_key_lifecycle() {
    // Initialize app state (requires a test database)
    let state = common::init_test_state().await;

    // Create a test user and team
    let user_id = Uuid::new_v4();
    let team_id = Uuid::new_v4();
    let username = format!("user_{}", user_id);
    let email = format!("user_{}@example.com", user_id);
    let team_name = format!("team_{}", team_id);

    sqlx::query!(
        "INSERT INTO users (id, username, email) VALUES ($1, $2, $3)",
        user_id,
        username,
        email
    )
    .execute(&state.db_pool)
    .await
    .expect("Failed to create test user");

    sqlx::query!(
        "INSERT INTO teams (id, name) VALUES ($1, $2)",
        team_id,
        team_name
    )
    .execute(&state.db_pool)
    .await
    .expect("Failed to create test team");

    sqlx::query!(
        "INSERT INTO team_memberships (user_id, team_id, role) VALUES ($1, $2, $3)",
        user_id,
        team_id,
        "admin"
    )
    .execute(&state.db_pool)
    .await
    .expect("Failed to create team membership");

    // Create a new API key
    let request = CreateApiKeyRequest {
        user_id,
        team_id,
        name: "Test Key".to_string(),
        description: Some("A test API key".to_string()),
        scopes: Some(vec!["chat:read".to_string(), "chat:write".to_string()]),
        expires_at: Some(Utc::now() + Duration::days(30)),
    };

    let response = generate_key(Prefix::Standard, request, &state)
        .await
        .expect("Failed to generate key");

    println!("Generated API key: {}", response.key);
    println!("Key prefix: {}", response.key_prefix);

    // Validate the key
    let validated = validate_key(&response.key, &state)
        .await
        .expect("Failed to validate key");

    assert_eq!(validated.user_id, user_id);
    assert_eq!(validated.team_id, team_id);
    assert_eq!(
        validated.scopes,
        Some(vec!["chat:read".to_string(), "chat:write".to_string()])
    );

    // List user keys
    let keys = list_user_keys(user_id, &state)
        .await
        .expect("Failed to list keys");

    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].name, "Test Key");

    // Revoke the key
    revoke_key(&keys[0].key_id, &state)
        .await
        .expect("Failed to revoke key");

    // Try to validate the revoked key (should fail)
    let result = validate_key(&response.key, &state).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_expired_key() {
    let state = common::init_test_state().await;

    let user_id = Uuid::new_v4();
    let team_id = Uuid::new_v4();
    let username = format!("expired_{}", user_id);
    let email = format!("expired_{}@example.com", user_id);
    let team_name = format!("expired_team_{}", team_id);

    sqlx::query!(
        "INSERT INTO users (id, username, email) VALUES ($1, $2, $3)",
        user_id,
        username,
        email
    )
    .execute(&state.db_pool)
    .await
    .expect("Failed to create test user");

    sqlx::query!(
        "INSERT INTO teams (id, name) VALUES ($1, $2)",
        team_id,
        team_name
    )
    .execute(&state.db_pool)
    .await
    .expect("Failed to create test team");

    sqlx::query!(
        "INSERT INTO team_memberships (user_id, team_id, role) VALUES ($1, $2, $3)",
        user_id,
        team_id,
        "admin"
    )
    .execute(&state.db_pool)
    .await
    .expect("Failed to create team membership");

    // Create a key that's already expired
    let request = CreateApiKeyRequest {
        user_id,
        team_id,
        name: "Expired Key".to_string(),
        description: None,
        scopes: None,
        expires_at: Some(Utc::now() - Duration::days(1)),
    };

    let response = generate_key(Prefix::Secret, request, &state)
        .await
        .expect("Failed to generate key");

    // Try to validate the expired key (should fail)
    let result = validate_key(&response.key, &state).await;
    assert!(result.is_err());
}
