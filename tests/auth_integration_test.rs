//! Authentication integration tests
//!
//! These tests cover the complete authentication flow including:
//! - API key creation, validation, and revocation
//! - PASETO token exchange and validation
//! - OPAQUE authentication (registration and login)
//! - Scope-based authorization
//! - Token revocation and blacklist

#[path = "common/mod.rs"]
mod common;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use chrono::{Duration, Utc};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

/// Test helper to create a test user and team
async fn create_test_user_and_team(state: &thalamus::bootstrap::AppState) -> (Uuid, Uuid) {
    let user_id = Uuid::new_v4();
    let team_id = Uuid::new_v4();
    let username = format!("test_user_{}", user_id);
    let email = format!("{}@example.com", username);
    let team_name = format!("test_team_{}", team_id);

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

    (user_id, team_id)
}

/// Test helper to create an API key via HTTP API
async fn create_api_key_via_api(
    app: &axum::Router,
    auth_key: &str,
    name: &str,
    scopes: Vec<&str>,
) -> (StatusCode, serde_json::Value) {
    let scopes_json: Vec<String> = scopes.iter().map(|s| s.to_string()).collect();

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/api-keys")
                .header("Authorization", format!("Bearer {}", auth_key))
                .header("Content-Type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "name": name,
                        "description": "Test API key",
                        "scopes": scopes_json,
                        "expires_in_days": 30
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();

    (status, json)
}

/// Test helper to make authenticated request
async fn make_authenticated_request(
    app: &axum::Router,
    method: &str,
    uri: &str,
    auth_key: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let request_builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("Authorization", format!("Bearer {}", auth_key));

    let request = match body {
        Some(json) => request_builder
            .header("Content-Type", "application/json")
            .body(Body::from(json.to_string()))
            .unwrap(),
        None => request_builder.body(Body::empty()).unwrap(),
    };

    let response = (*app).clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();

    (status, json)
}

#[sqlx::test]
async fn test_api_key_authentication_flow(pool: PgPool) {
    common::init_test_logging();

    let state = common::transactional::init_test_state(pool).await;
    let app = thalamus::bootstrap::build_router(state.clone());

    // Create test user and team
    let (user_id, team_id) = create_test_user_and_team(&state).await;

    // Create an initial API key with full permissions
    use thalamus::features::auth::domain::api_key::CreateApiKeyRequest;
    use thalamus::features::auth::domain::keys::{Prefix, generate_key};

    let request = CreateApiKeyRequest {
        user_id,
        team_id,
        project_id: None,
        name: "Master Key".to_string(),
        description: Some("Key with full permissions".to_string()),
        scopes: Some(vec![
            "api_keys:create".to_string(),
            "api_keys:read".to_string(),
            "api_keys:revoke".to_string(),
        ]),
        expires_at: Some(Utc::now() + Duration::days(30)),
    };

    let master_key = generate_key(Prefix::Standard, request, &state)
        .await
        .expect("Failed to generate master key");

    // Test 1: Create a new API key via API
    let (status, create_response) = create_api_key_via_api(
        &app,
        &master_key.key,
        "Test Key",
        vec!["chat:read", "chat:write"],
    )
    .await;

    assert_eq!(status, StatusCode::OK, "Create API key should succeed");
    assert!(
        create_response["key"].is_string(),
        "Response should contain the key"
    );

    let new_key = create_response["key"].as_str().unwrap();

    // Test 2: List API keys
    let (status, list_response) =
        make_authenticated_request(&app, "GET", "/v1/api-keys", &master_key.key, None).await;

    assert_eq!(status, StatusCode::OK, "List keys should succeed");
    assert!(
        list_response.as_array().unwrap().len() >= 2,
        "Should have at least 2 keys"
    );

    // Test 3: Use the new key to access whoami
    let (status, whoami_response) =
        make_authenticated_request(&app, "GET", "/v1/auth/whoami", new_key, None).await;

    assert_eq!(status, StatusCode::OK, "Whoami should succeed");
    assert_eq!(
        whoami_response["user_id"].as_str().unwrap(),
        user_id.to_string()
    );

    // Test 4: Revoke the new key
    let key_id = create_response["key_id"].as_str().unwrap();
    let (status, _) = make_authenticated_request(
        &app,
        "POST",
        "/v1/api-keys/revoke",
        &master_key.key,
        Some(serde_json::json!({ "key_id": key_id })),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "Revoke should succeed");

    // Test 5: Verify revoked key no longer works
    let (status, _) =
        make_authenticated_request(&app, "GET", "/v1/auth/whoami", new_key, None).await;

    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "Revoked key should be rejected"
    );
}

#[sqlx::test]
async fn test_scope_based_authorization(pool: PgPool) {
    common::init_test_logging();

    let state = common::transactional::init_test_state(pool).await;
    let app = thalamus::bootstrap::build_router(state.clone());

    let (user_id, team_id) = create_test_user_and_team(&state).await;

    // Create key with only read scope
    use thalamus::features::auth::domain::api_key::CreateApiKeyRequest;
    use thalamus::features::auth::domain::keys::{Prefix, generate_key};

    let read_only_request = CreateApiKeyRequest {
        user_id,
        team_id,
        project_id: None,
        name: "Read Only Key".to_string(),
        description: None,
        scopes: Some(vec!["api_keys:read".to_string()]),
        expires_at: Some(Utc::now() + Duration::days(30)),
    };

    let read_only_key = generate_key(Prefix::Standard, read_only_request, &state)
        .await
        .expect("Failed to generate read-only key");

    // Attempt to create a key with read-only key (should fail)
    let (status, _) = create_api_key_via_api(&app, &read_only_key.key, "Should Fail", vec![]).await;

    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "Should be forbidden without api_keys:create scope"
    );

    // List keys with read-only key (should succeed)
    let (status, _) =
        make_authenticated_request(&app, "GET", "/v1/api-keys", &read_only_key.key, None).await;

    assert_eq!(
        status,
        StatusCode::OK,
        "Should succeed with api_keys:read scope"
    );
}

#[sqlx::test]
async fn test_token_exchange_flow(pool: PgPool) {
    common::init_test_logging();

    let state = common::transactional::init_test_state(pool).await;
    let app = thalamus::bootstrap::build_router(state.clone());

    let (user_id, team_id) = create_test_user_and_team(&state).await;

    // Create an API key
    use thalamus::features::auth::domain::api_key::CreateApiKeyRequest;
    use thalamus::features::auth::domain::keys::{Prefix, generate_key};

    let request = CreateApiKeyRequest {
        user_id,
        team_id,
        project_id: None,
        name: "Token Test Key".to_string(),
        description: None,
        scopes: Some(vec!["chat:read".to_string()]),
        expires_at: Some(Utc::now() + Duration::days(30)),
    };

    let api_key = generate_key(Prefix::Standard, request, &state)
        .await
        .expect("Failed to generate key");

    // Exchange API key for PASETO token
    let (status, token_response) =
        make_authenticated_request(&app, "POST", "/v1/auth/token", &api_key.key, None).await;

    assert_eq!(status, StatusCode::OK, "Token exchange should succeed");
    assert!(
        token_response["token"].is_string(),
        "Response should contain token"
    );

    let token = token_response["token"].as_str().unwrap();

    // Verify token starts with PASETO v4.local prefix
    assert!(
        token.starts_with("v4.local."),
        "Token should be PASETO v4.local format"
    );

    // Use PASETO token to access whoami
    let (status, whoami_response) =
        make_authenticated_request(&app, "GET", "/v1/auth/whoami", token, None).await;

    assert_eq!(status, StatusCode::OK, "Whoami with token should succeed");
    assert_eq!(
        whoami_response["user_id"].as_str().unwrap(),
        user_id.to_string()
    );
}

#[sqlx::test]
async fn test_invalid_authentication(pool: PgPool) {
    common::init_test_logging();

    let state = common::transactional::init_test_state(pool).await;
    let app = thalamus::bootstrap::build_router(state.clone());

    // Test 1: Missing Authorization header
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/auth/whoami")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // Test 2: Invalid Authorization format
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/auth/whoami")
                .header("Authorization", "Basic dXNlcjpwYXNz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // Test 3: Invalid API key
    let (status, _) = make_authenticated_request(
        &app,
        "GET",
        "/v1/auth/whoami",
        "thl_invalid_key_12345",
        None,
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Test 4: Invalid PASETO token
    let (status, _) = make_authenticated_request(
        &app,
        "GET",
        "/v1/auth/whoami",
        "v4.local.invalid_token_here",
        None,
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn test_expired_api_key(pool: PgPool) {
    common::init_test_logging();

    let state = common::transactional::init_test_state(pool).await;
    let app = thalamus::bootstrap::build_router(state.clone());

    let (user_id, team_id) = create_test_user_and_team(&state).await;

    // Create an expired API key
    use thalamus::features::auth::domain::api_key::CreateApiKeyRequest;
    use thalamus::features::auth::domain::keys::{Prefix, generate_key};

    let request = CreateApiKeyRequest {
        user_id,
        team_id,
        project_id: None,
        name: "Expired Key".to_string(),
        description: None,
        scopes: Some(vec!["chat:read".to_string()]),
        expires_at: Some(Utc::now() - Duration::days(1)), // Expired yesterday
    };

    let expired_key = generate_key(Prefix::Standard, request, &state)
        .await
        .expect("Failed to generate key");

    // Try to use expired key
    let (status, _) =
        make_authenticated_request(&app, "GET", "/v1/auth/whoami", &expired_key.key, None).await;

    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "Expired key should be rejected"
    );
}

// TODO: Tests to implement once features are ready:
// - test_opaque_registration_login_flow()
// - test_token_revocation_blacklist()
// - test_rate_limiting()
// - test_casbin_authorization()
