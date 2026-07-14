//! First-run setup endpoints for OPAQUE-based admin authentication.
//!
//! These endpoints are only usable when the system has no configured
//! authentication: no OAuth providers and no users with an OPAQUE registration.

use axum::{Json, Router, extract::State, routing};
use base64::Engine;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::bootstrap::AppState;
use crate::error::{Error, Result};
use crate::features::auth::domain::opaque::RegistrationRecord;
use crate::features::auth::domain::token::TokenClaims;
use crate::features::auth::infra::opaque_service::ThalamusCipherSuite;
use crate::features::auth::infra::{create_token, get_server_setup, registration_finish};
use opaque_ke::{ClientRegistration, ClientRegistrationFinishParameters, ServerRegistration};

/// Response from the setup-status endpoint.
#[derive(Debug, Serialize)]
pub struct SetupStatusResponse {
    pub needs_setup: bool,
}

/// Request to create the first admin user via OPAQUE setup.
#[derive(Debug, Deserialize)]
pub struct SetupRequest {
    pub username: String,
    pub email: String,
    pub password: String,
}

/// Response after successful setup.
#[derive(Debug, Serialize)]
pub struct SetupResponse {
    pub token: String,
    pub user_id: Uuid,
    pub team_id: Uuid,
}

/// Base64 engine used by `@serenity-kit/opaque`.
const BASE64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// Check whether the first-run setup is required.
///
/// Setup is required only when there are no OAuth providers configured and no
/// user has completed OPAQUE registration.
pub async fn setup_status(State(state): State<AppState>) -> Result<Json<SetupStatusResponse>> {
    let oauth_count = state.oauth_service.list_providers().len() as i64;

    let opaque_count: i64 =
        sqlx::query_scalar!("SELECT COUNT(*) FROM users WHERE opaque_registration IS NOT NULL")
            .fetch_one(&state.db_pool)
            .await?
            .unwrap_or(0);

    Ok(Json(SetupStatusResponse {
        needs_setup: oauth_count == 0 && opaque_count == 0,
    }))
}

/// Perform first-run setup: create the first user, team, and OPAQUE password.
pub async fn setup(
    State(state): State<AppState>,
    Json(req): Json<SetupRequest>,
) -> Result<Json<SetupResponse>> {
    // Re-validate the setup condition defensively.
    let oauth_count = state.oauth_service.list_providers().len() as i64;
    let opaque_count: i64 =
        sqlx::query_scalar!("SELECT COUNT(*) FROM users WHERE opaque_registration IS NOT NULL")
            .fetch_one(&state.db_pool)
            .await?
            .unwrap_or(0);

    if oauth_count > 0 || opaque_count > 0 {
        return Err(Error::Authorization(
            "Setup is only available before any authentication is configured".to_string(),
        ));
    }

    if req.username.is_empty() || req.password.len() < 8 {
        return Err(Error::InvalidInput(
            "Username is required and password must be at least 8 characters".to_string(),
        ));
    }

    let server_setup = get_server_setup(&state)?;

    // Run OPAQUE registration server-side. This is the first-run setup and
    // there is no other way to authenticate yet, so accepting the plaintext
    // password here is acceptable; the password never leaves the browser in
    // normal operation.
    let mut rng = rand_08::rngs::OsRng;
    let client_registration_start =
        ClientRegistration::<ThalamusCipherSuite>::start(&mut rng, req.password.as_bytes())
            .map_err(|e| Error::Authentication(format!("OPAQUE registration start failed: {e}")))?;

    let server_registration_start = ServerRegistration::<ThalamusCipherSuite>::start(
        &server_setup,
        client_registration_start.message,
        req.username.as_bytes(),
    )
    .map_err(|e| Error::Authentication(format!("OPAQUE server registration start failed: {e}")))?;

    let client_registration_finish = client_registration_start
        .state
        .finish(
            &mut rng,
            req.password.as_bytes(),
            server_registration_start.message,
            ClientRegistrationFinishParameters::default(),
        )
        .map_err(|e| {
            Error::Authentication(format!("OPAQUE client registration finish failed: {e}"))
        })?;

    let registration_record = RegistrationRecord {
        username: req.username.clone(),
        message: BASE64.encode(client_registration_finish.message.serialize()),
    };

    // Create the user, team, and membership first, then store the OPAQUE
    // record. `registration_finish` expects the user row to exist.
    let user_id = Uuid::new_v4();
    let team_id = Uuid::new_v4();

    sqlx::query!(
        "INSERT INTO teams (id, name, description, rate_limit_rpm, rate_limit_burst, logging_policy) VALUES ($1, $2, $3, $4, $5, $6)",
        team_id,
        "default",
        "Default team",
        1000,
        50,
        "metadata"
    )
    .execute(&state.db_pool)
    .await?;

    sqlx::query!(
        "INSERT INTO users (id, username, email, is_service_account, is_active) VALUES ($1, $2, $3, $4, $5)",
        user_id,
        req.username,
        req.email,
        false,
        true
    )
    .execute(&state.db_pool)
    .await?;

    sqlx::query!(
        "INSERT INTO team_memberships (user_id, team_id, role) VALUES ($1, $2, 'admin')",
        user_id,
        team_id
    )
    .execute(&state.db_pool)
    .await?;

    // Store the OPAQUE registration record.
    registration_finish(registration_record, &state).await?;

    // Issue a PASETO token so the UI is immediately authenticated.
    let claims = TokenClaims::new(
        user_id,
        team_id,
        Some(vec!["admin".to_string()]),
        Some(vec![
            "api_keys:read".to_string(),
            "api_keys:create".to_string(),
            "api_keys:revoke".to_string(),
            "api_keys:rotate".to_string(),
            "signing_keys:read".to_string(),
            "signing_keys:create".to_string(),
            "signing_keys:revoke".to_string(),
            "tokens:read".to_string(),
            "tokens:create".to_string(),
            "tokens:revoke".to_string(),
            "oauth:link".to_string(),
            "oauth:unlink".to_string(),
            "admin".to_string(),
        ]),
        3600 * 24,
    );

    let token = create_token(&claims, &state)?;

    Ok(Json(SetupResponse {
        token,
        user_id,
        team_id,
    }))
}

/// Create the setup router.
pub fn setup_router() -> Router<AppState> {
    Router::new()
        .route("/v1/auth/setup-status", routing::get(setup_status))
        .route("/v1/auth/setup", routing::post(setup))
}
