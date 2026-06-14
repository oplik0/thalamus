//! OPAQUE authentication integration tests.
//!
//! These tests exercise the first-run setup flow and the full OPAQUE
//! registration/login flow using the Rust OPAQUE client implementation.

#[path = "common/mod.rs"]
mod common;

use axum::{body::Body, http::Request, http::StatusCode};
use base64::Engine;
use opaque_ke::{
    ClientLogin, ClientLoginFinishParameters, ClientRegistration,
    ClientRegistrationFinishParameters, CredentialResponse,
    RegistrationResponse as OpaqueRegistrationResponse,
};
use rand_08::rngs::OsRng;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

/// Base64 engine used by the OPAQUE JS bindings and the backend.
const BASE64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// Make an unauthenticated HTTP request and return (status, json body).
async fn make_request(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let request_builder = Request::builder().method(method).uri(uri);

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

/// Make an authenticated HTTP request and return (status, json body).
async fn make_authenticated_request(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let request_builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("Authorization", format!("Bearer {}", token));

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
async fn test_opaque_setup_and_login_flow(pool: PgPool) {
    common::init_test_logging();

    let state = common::transactional::init_test_state(pool).await;
    let app = thalamus::bootstrap::build_router(state.clone());
    type Cipher = thalamus::features::auth::infra::opaque_service::ThalamusCipherSuite;

    // 1. Setup is required before any authentication is configured.
    let (status, body) = make_request(&app, "GET", "/v1/auth/setup-status", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["needs_setup"], true);

    // 2. Create the first admin via the public setup endpoint.
    let (status, setup_body) = make_request(
        &app,
        "POST",
        "/v1/auth/setup",
        Some(serde_json::json!({
            "username": "admin",
            "email": "admin@example.com",
            "password": "SuperSecret1!"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "Setup should succeed: {setup_body}");
    assert!(
        setup_body["token"].is_string(),
        "Setup should return a token"
    );
    let admin_token = setup_body["token"].as_str().unwrap().to_string();

    // 3. Setup is no longer required after the first admin exists.
    let (status, body) = make_request(&app, "GET", "/v1/auth/setup-status", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["needs_setup"], false);

    // 4. OPAQUE login as the setup admin.
    let mut rng = OsRng;
    let client_login_start =
        ClientLogin::<Cipher>::start(&mut rng, b"SuperSecret1!").expect("login start");

    let (status, login_start_body) = make_request(
        &app,
        "POST",
        "/v1/auth/login/start",
        Some(serde_json::json!({
            "username": "admin",
            "message": BASE64.encode(client_login_start.message.serialize())
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "Login start should succeed: {login_start_body}"
    );

    let server_state = login_start_body["server_state"].as_str().unwrap();
    let credential_response_bytes = BASE64
        .decode(login_start_body["message"].as_str().unwrap())
        .unwrap();
    let credential_response =
        CredentialResponse::<Cipher>::deserialize(&credential_response_bytes).unwrap();

    let client_login_finish = client_login_start
        .state
        .finish(
            &mut rng,
            b"SuperSecret1!",
            credential_response,
            ClientLoginFinishParameters::default(),
        )
        .expect("login finish");

    let (status, login_finish_body) = make_request(
        &app,
        "POST",
        "/v1/auth/login/finish",
        Some(serde_json::json!({
            "username": "admin",
            "finish_login_request": BASE64.encode(client_login_finish.message.serialize()),
            "server_state": server_state
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "Login finish should succeed: {login_finish_body}"
    );
    assert!(login_finish_body["token"].is_string());

    // 5. Register a second user via the admin-only registration endpoints.
    let user2_id = Uuid::new_v4();
    let team2_id = Uuid::new_v4();
    sqlx::query!(
        "INSERT INTO teams (id, name) VALUES ($1, $2)",
        team2_id,
        "team2"
    )
    .execute(&state.db_pool)
    .await
    .expect("Failed to create second team");

    sqlx::query!(
        "INSERT INTO users (id, username, email) VALUES ($1, $2, $3)",
        user2_id,
        "operator",
        "operator@example.com"
    )
    .execute(&state.db_pool)
    .await
    .expect("Failed to create second user");

    sqlx::query!(
        "INSERT INTO team_memberships (user_id, team_id, role) VALUES ($1, $2, 'admin')",
        user2_id,
        team2_id
    )
    .execute(&state.db_pool)
    .await
    .expect("Failed to create second membership");

    let client_reg_start =
        ClientRegistration::<Cipher>::start(&mut rng, b"AnotherPass1!").expect("reg start");

    let (status, reg_start_body) = make_authenticated_request(
        &app,
        "POST",
        "/v1/auth/register/start",
        &admin_token,
        Some(serde_json::json!({
            "username": "operator",
            "message": BASE64.encode(client_reg_start.message.serialize())
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "Registration start should succeed: {reg_start_body}"
    );

    let reg_response_bytes = BASE64
        .decode(reg_start_body["message"].as_str().unwrap())
        .unwrap();
    let reg_response =
        OpaqueRegistrationResponse::<Cipher>::deserialize(&reg_response_bytes).unwrap();

    let client_reg_finish = client_reg_start
        .state
        .finish(
            &mut rng,
            b"AnotherPass1!",
            reg_response,
            ClientRegistrationFinishParameters::default(),
        )
        .expect("reg finish");

    let (status, reg_finish_body) = make_authenticated_request(
        &app,
        "POST",
        "/v1/auth/register/finish",
        &admin_token,
        Some(serde_json::json!({
            "username": "operator",
            "message": BASE64.encode(client_reg_finish.message.serialize())
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "Registration finish should succeed: {reg_finish_body}"
    );

    // 6. OPAQUE login as the second user.
    let client_login2_start =
        ClientLogin::<Cipher>::start(&mut rng, b"AnotherPass1!").expect("login2 start");

    let (status, login2_start_body) = make_request(
        &app,
        "POST",
        "/v1/auth/login/start",
        Some(serde_json::json!({
            "username": "operator",
            "message": BASE64.encode(client_login2_start.message.serialize())
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "Second login start should succeed: {login2_start_body}"
    );

    let server_state2 = login2_start_body["server_state"].as_str().unwrap();
    let credential_response2_bytes = BASE64
        .decode(login2_start_body["message"].as_str().unwrap())
        .unwrap();
    let credential_response2 =
        CredentialResponse::<Cipher>::deserialize(&credential_response2_bytes).unwrap();

    let client_login2_finish = client_login2_start
        .state
        .finish(
            &mut rng,
            b"AnotherPass1!",
            credential_response2,
            ClientLoginFinishParameters::default(),
        )
        .expect("login2 finish");

    let (status, login2_finish_body) = make_request(
        &app,
        "POST",
        "/v1/auth/login/finish",
        Some(serde_json::json!({
            "username": "operator",
            "finish_login_request": BASE64.encode(client_login2_finish.message.serialize()),
            "server_state": server_state2
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "Second login finish should succeed: {login2_finish_body}"
    );
    assert!(login2_finish_body["token"].is_string());
}
