use chrono::{Duration, Utc};
use thalamus::bootstrap::init_app_state;
use thalamus::features::auth::domain::api_key::CreateApiKeyRequest;
use thalamus::features::auth::domain::keys::{Prefix, generate_key};
use thalamus::features::auth::infra::{list_user_keys, revoke_key, validate_key};
use uuid::Uuid;

#[tokio::test]
async fn test_api_key_lifecycle() {
    // Initialize app state (requires a test database)
    let state = init_app_state("config.example.k")
        .await
        .expect("Failed to initialize app state");

    let user_id = Uuid::new_v4();
    let team_id = Uuid::new_v4();

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
    revoke_key(&response.key, &state)
        .await
        .expect("Failed to revoke key");

    // Try to validate the revoked key (should fail)
    let result = validate_key(&response.key, &state).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_expired_key() {
    let state = init_app_state("config.example.k")
        .await
        .expect("Failed to initialize app state");

    let user_id = Uuid::new_v4();
    let team_id = Uuid::new_v4();

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
