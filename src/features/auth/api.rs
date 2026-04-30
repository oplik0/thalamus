use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{delete, get, post},
};
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::bootstrap::AppState;
use crate::error::{Error, Result};
use crate::features::auth::api_oauth::oauth_routes;
use crate::features::auth::domain::api_key::{CreateApiKeyRequest, CreateApiKeyResponse};
use crate::features::auth::domain::keys::{Prefix, generate_key};
use crate::features::auth::domain::opaque::{
    LoginFinishRequest, LoginRequest, LoginResponse, RegistrationRecord, RegistrationRequest,
    RegistrationResponse,
};
use crate::features::auth::domain::token::TokenClaims;
use crate::features::auth::infra::{
    SignatureAlgorithm, create_refresh_token, create_signing_key, create_token, get_signing_key,
    list_user_keys, list_user_refresh_tokens, list_user_signing_keys, login_finish, login_start,
    registration_finish, registration_start, revoke_key, revoke_refresh_token, revoke_signing_key,
    revoke_token, rotate_key_immediate, rotate_key_with_grace_period, rotate_refresh_token,
};
use crate::middleware::{ApiKeyAuth, require_scope};

/// Request body for creating a new API key
#[derive(Debug, Deserialize)]
pub struct CreateKeyRequest {
    pub name: String,
    pub description: Option<String>,
    pub scopes: Option<Vec<String>>,
    pub expires_in_days: Option<i64>,
}

/// Response for listing API keys (without sensitive data)
#[derive(Debug, Serialize)]
pub struct ApiKeyInfo {
    pub id: Uuid,
    pub key_prefix: String,
    pub name: String,
    pub description: Option<String>,
    pub scopes: Option<Vec<String>>,
    pub is_active: bool,
    pub last_used_at: Option<String>,
    pub expires_at: Option<String>,
    pub created_at: String,
}

/// Create a new API key
///
/// Requires authentication with a key that has the 'api_keys:create' scope
pub async fn create_key(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Json(req): Json<CreateKeyRequest>,
) -> Result<Json<CreateApiKeyResponse>> {
    // Check if the authenticated key has permission to create keys
    require_scope(&auth, "api_keys:create")?;

    let expires_at = req
        .expires_in_days
        .map(|days| Utc::now() + Duration::days(days));

    let request = CreateApiKeyRequest {
        user_id: auth.user_id,
        team_id: auth.team_id,
        name: req.name,
        description: req.description,
        scopes: req.scopes,
        expires_at,
    };

    let response = generate_key(Prefix::Standard, request, &state).await?;

    Ok(Json(response))
}

/// List all API keys for the authenticated user
pub async fn list_keys(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
) -> Result<Json<Vec<ApiKeyInfo>>> {
    require_scope(&auth, "api_keys:read")?;

    let keys = list_user_keys(auth.user_id, &state).await?;

    let key_infos = keys
        .into_iter()
        .map(|key| ApiKeyInfo {
            id: key.id,
            key_prefix: key.key_prefix,
            name: key.name,
            description: key.description,
            scopes: key.scopes,
            is_active: key.is_active,
            last_used_at: key.last_used_at.map(|dt| dt.to_rfc3339()),
            expires_at: key.expires_at.map(|dt| dt.to_rfc3339()),
            created_at: key.created_at.to_rfc3339(),
        })
        .collect();

    Ok(Json(key_infos))
}

/// Request body for revoking an API key
#[derive(Debug, Deserialize)]
pub struct RevokeKeyRequest {
    pub key_id: String,
}

/// Revoke an API key
pub async fn revoke_key_handler(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Json(req): Json<RevokeKeyRequest>,
) -> Result<Json<serde_json::Value>> {
    require_scope(&auth, "api_keys:revoke")?;

    revoke_key(&req.key_id, &state).await?;

    Ok(Json(serde_json::json!({
        "message": "API key revoked successfully",
        "key_id": req.key_id,
    })))
}

/// Get information about the currently authenticated key
pub async fn whoami(ApiKeyAuth(auth): ApiKeyAuth) -> Result<Json<serde_json::Value>> {
    Ok(Json(serde_json::json!({
        "user_id": auth.user_id,
        "team_id": auth.team_id,
        "key_id": auth.key_id,
        "token_id": auth.token_id,
        "scopes": auth.scopes,
        "roles": auth.roles,
    })))
}

/// Exchange an API key for a PASETO token
///
/// This endpoint allows clients to exchange a valid API key for a short-lived
/// PASETO token. This is useful for reducing the overhead of Argon2 verification
/// on subsequent requests.
pub async fn token_exchange(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
) -> Result<Json<serde_json::Value>> {
    // Create token claims from the authenticated key's info
    let claims = TokenClaims::new(
        auth.user_id,
        auth.team_id,
        auth.roles,  // Use roles from the Auth (if any)
        auth.scopes, // Use scopes from the Auth
        3600 * 24,   // 24 hours
    );

    let token = create_token(&claims, &state)?;

    Ok(Json(serde_json::json!({
        "token": token,
        "expires_in": 86400, // 24 hours in seconds
    })))
}

/// Start OPAQUE registration
pub async fn register_start_handler(
    State(state): State<AppState>,
    Json(req): Json<RegistrationRequest>,
) -> Result<Json<RegistrationResponse>> {
    let response = registration_start(req, &state).await?;
    Ok(Json(response))
}

/// Finish OPAQUE registration
pub async fn register_finish_handler(
    State(state): State<AppState>,
    Json(req): Json<RegistrationRecord>,
) -> Result<Json<serde_json::Value>> {
    registration_finish(req, &state).await?;
    Ok(Json(serde_json::json!({
        "message": "Registration successful"
    })))
}

/// Start OPAQUE login
pub async fn login_start_handler(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<LoginResponse>> {
    let response = login_start(req, &state).await?;
    Ok(Json(response))
}

/// Finish OPAQUE login
pub async fn login_finish_handler(
    State(state): State<AppState>,
    Json(req): Json<LoginFinishRequest>,
) -> Result<Json<serde_json::Value>> {
    let token = login_finish(req, &state).await?;
    Ok(Json(serde_json::json!({
        "token": token
    })))
}

/// Request body for rotating an API key
#[derive(Debug, Deserialize)]
pub struct RotateKeyRequest {
    pub key_id: String,
    pub grace_period_minutes: Option<i64>,
    pub reason: Option<String>,
}

/// Rotate an API key
pub async fn rotate_key_handler(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Json(req): Json<RotateKeyRequest>,
) -> Result<Json<serde_json::Value>> {
    require_scope(&auth, "api_keys:rotate")?;

    let grace_period = req.grace_period_minutes.unwrap_or(60); // Default 1 hour

    if grace_period > 0 {
        let result =
            rotate_key_with_grace_period(&req.key_id, grace_period, req.reason.as_deref(), &state)
                .await?;

        Ok(Json(serde_json::json!({
            "message": "API key rotated with grace period",
            "old_key_id": result.old_key_id,
            "old_key_expires_at": result.old_key_expires_at.to_rfc3339(),
            "new_key": {
                "id": result.new_key.id,
                "key": result.new_key.key,
                "key_prefix": result.new_key.key_prefix,
                "name": result.new_key.name,
                "scopes": result.new_key.scopes,
                "created_at": result.new_key.created_at.to_rfc3339(),
                "expires_at": result.new_key.expires_at.map(|dt| dt.to_rfc3339()),
            }
        })))
    } else {
        let new_key = rotate_key_immediate(&req.key_id, req.reason.as_deref(), &state).await?;

        Ok(Json(serde_json::json!({
            "message": "API key rotated immediately",
            "new_key": {
                "id": new_key.id,
                "key": new_key.key,
                "key_prefix": new_key.key_prefix,
                "name": new_key.name,
                "scopes": new_key.scopes,
                "created_at": new_key.created_at.to_rfc3339(),
                "expires_at": new_key.expires_at.map(|dt| dt.to_rfc3339()),
            }
        })))
    }
}

/// Request body for creating a refresh token
#[derive(Debug, Deserialize)]
pub struct CreateRefreshTokenRequest {
    pub expires_in_days: Option<i64>,
}

/// Response for refresh token creation
#[derive(Debug, Serialize)]
pub struct RefreshTokenResponse {
    pub refresh_token: String,
    pub expires_at: String,
}

/// Create a refresh token (for token exchange with short-lived access tokens)
pub async fn create_refresh_token_handler(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Json(req): Json<CreateRefreshTokenRequest>,
) -> Result<Json<RefreshTokenResponse>> {
    require_scope(&auth, "tokens:create")?;

    let expires_in = req.expires_in_days.unwrap_or(30); // Default 30 days
    let family = Uuid::new_v4();

    let (token, info) = create_refresh_token(
        auth.user_id,
        auth.team_id,
        family,
        None, // No parent for initial token
        auth.scopes.clone(),
        auth.roles.clone(),
        expires_in,
        &state,
    )
    .await?;

    Ok(Json(RefreshTokenResponse {
        refresh_token: token,
        expires_at: info.expires_at.to_rfc3339(),
    }))
}

/// Request body for refreshing tokens
#[derive(Debug, Deserialize)]
pub struct RefreshTokenRequest {
    pub refresh_token: String,
}

/// Refresh an access token using a refresh token
pub async fn refresh_token_handler(
    State(state): State<AppState>,
    Json(req): Json<RefreshTokenRequest>,
) -> Result<Json<serde_json::Value>> {
    // Rotate the refresh token (creates new, invalidates old)
    let (new_refresh_token, refresh_info) =
        rotate_refresh_token(&req.refresh_token, 30, &state).await?;

    // Create new access token
    let claims = TokenClaims::new(
        refresh_info.user_id,
        refresh_info.team_id,
        refresh_info.roles,
        refresh_info.scopes,
        900, // 15 minutes for access token
    );

    let access_token = create_token(&claims, &state)?;

    Ok(Json(serde_json::json!({
        "access_token": access_token,
        "refresh_token": new_refresh_token,
        "expires_in": 900,
        "token_type": "Bearer",
    })))
}

/// Logout - revoke the current token
pub async fn logout_handler(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
) -> Result<Json<serde_json::Value>> {
    // If authenticated via token, revoke it
    if let Some(token_id) = auth.token_id {
        revoke_token(
            token_id,
            auth.user_id,
            Utc::now() + Duration::hours(24), // Expires in 24 hours
            "logout",
            Some(auth.user_id),
            &state,
        )
        .await?;
    }

    Ok(Json(serde_json::json!({
        "message": "Logged out successfully"
    })))
}

/// List refresh tokens for the current user
pub async fn list_refresh_tokens(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
) -> Result<Json<serde_json::Value>> {
    require_scope(&auth, "tokens:read")?;

    let tokens = list_user_refresh_tokens(auth.user_id, &state).await?;

    let token_infos: Vec<_> = tokens
        .into_iter()
        .map(|t| {
            serde_json::json!({
                "id": t.id,
                "family": t.family,
                "scopes": t.scopes,
                "roles": t.roles,
                "expires_at": t.expires_at.to_rfc3339(),
                "is_active": t.is_active,
                "revoked_at": t.revoked_at.map(|dt| dt.to_rfc3339()),
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "tokens": token_infos
    })))
}

/// Revoke a refresh token
#[derive(Debug, Deserialize)]
pub struct RevokeRefreshTokenRequest {
    pub token_id: Uuid,
}

pub async fn revoke_refresh_token_handler(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Json(req): Json<RevokeRefreshTokenRequest>,
) -> Result<Json<serde_json::Value>> {
    require_scope(&auth, "tokens:revoke")?;

    revoke_refresh_token(req.token_id, "user_revoked", &state).await?;

    Ok(Json(serde_json::json!({
        "message": "Refresh token revoked",
        "token_id": req.token_id,
    })))
}

/// Request body for creating a signing key
#[derive(Debug, Deserialize)]
pub struct CreateSigningKeyRequest {
    pub algorithm: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub scopes: Option<Vec<String>>,
    pub expires_in_days: Option<i64>,
}

/// Response for signing key creation (includes private key - only returned once!)
#[derive(Debug, Serialize)]
pub struct CreateSigningKeyResponse {
    pub key_id: String,
    pub private_key: String,
    pub public_key: String,
    pub algorithm: String,
    pub fingerprint: String,
    pub name: Option<String>,
    pub scopes: Option<Vec<String>>,
    pub expires_at: Option<String>,
    pub warning: String,
}

/// Create a new signing key for HTTP Signatures
pub async fn create_signing_key_handler(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Json(req): Json<CreateSigningKeyRequest>,
) -> Result<Json<CreateSigningKeyResponse>> {
    require_scope(&auth, "signing_keys:create")?;

    let algorithm = SignatureAlgorithm::from_str(&req.algorithm)?;

    let key_pair = create_signing_key(
        auth.user_id,
        auth.team_id,
        algorithm,
        req.name.clone(),
        req.description.clone(),
        req.scopes.clone(),
        req.expires_in_days,
        &state,
    )
    .await?;

    Ok(Json(CreateSigningKeyResponse {
        key_id: key_pair.key_id,
        private_key: key_pair.private_key,
        public_key: key_pair.public_key,
        algorithm: key_pair.algorithm.as_str().to_string(),
        fingerprint: key_pair.fingerprint,
        name: req.name,
        scopes: req.scopes,
        expires_at: req
            .expires_in_days
            .map(|days| (Utc::now() + Duration::days(days)).to_rfc3339()),
        warning: "The private key is only shown once. Store it securely!".to_string(),
    }))
}

/// List signing keys for the current user
pub async fn list_signing_keys(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
) -> Result<Json<serde_json::Value>> {
    require_scope(&auth, "signing_keys:read")?;

    let keys = list_user_signing_keys(auth.user_id, false, &state).await?;

    let key_infos: Vec<_> = keys
        .into_iter()
        .map(|k| {
            serde_json::json!({
                "id": k.id,
                "key_id": k.key_id,
                "algorithm": k.algorithm,
                "fingerprint": k.fingerprint,
                "name": k.name,
                "scopes": k.scopes,
                "is_active": k.is_active,
                "expires_at": k.expires_at.map(|dt| dt.to_rfc3339()),
                "last_used_at": k.last_used_at.map(|dt| dt.to_rfc3339()),
                "use_count": k.use_count,
                "created_at": k.created_at.to_rfc3339(),
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "keys": key_infos
    })))
}

/// Get a specific signing key (public info only)
pub async fn get_signing_key_handler(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Path(key_id): Path<String>,
) -> Result<Json<serde_json::Value>> {
    require_scope(&auth, "signing_keys:read")?;

    let key = get_signing_key(&key_id, &state).await?;

    // Only return the user's own keys
    if key.user_id != auth.user_id {
        return Err(Error::Authorization(
            "Not authorized to view this key".to_string(),
        ));
    }

    Ok(Json(serde_json::json!({
        "id": key.id,
        "key_id": key.key_id,
        "algorithm": key.algorithm,
        "fingerprint": key.fingerprint,
        "public_key": key.public_key,
        "name": key.name,
        "scopes": key.scopes,
        "is_active": key.is_active,
        "expires_at": key.expires_at.map(|dt| dt.to_rfc3339()),
        "last_used_at": key.last_used_at.map(|dt| dt.to_rfc3339()),
        "use_count": key.use_count,
        "created_at": key.created_at.to_rfc3339(),
    })))
}

/// Revoke a signing key
pub async fn revoke_signing_key_handler(
    State(state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Path(key_id): Path<String>,
) -> Result<Json<serde_json::Value>> {
    require_scope(&auth, "signing_keys:revoke")?;

    revoke_signing_key(&key_id, auth.user_id, "user_revoked", &state).await?;

    Ok(Json(serde_json::json!({
        "message": "Signing key revoked",
        "key_id": key_id,
    })))
}

/// Create the API router
pub fn router() -> Router<AppState> {
    Router::new()
        // API Keys
        .route("/v1/api-keys", post(create_key))
        .route("/v1/api-keys", get(list_keys))
        .route("/v1/api-keys/revoke", post(revoke_key_handler))
        .route("/v1/api-keys/rotate", post(rotate_key_handler))
        // Auth
        .route("/v1/auth/whoami", get(whoami))
        .route("/v1/auth/token", post(token_exchange))
        .route("/v1/auth/token/refresh", post(refresh_token_handler))
        .route("/v1/auth/logout", post(logout_handler))
        // Refresh Tokens
        .route(
            "/v1/auth/refresh-tokens",
            post(create_refresh_token_handler),
        )
        .route("/v1/auth/refresh-tokens", get(list_refresh_tokens))
        .route(
            "/v1/auth/refresh-tokens/revoke",
            post(revoke_refresh_token_handler),
        )
        // OPAQUE
        .route("/v1/auth/register/start", post(register_start_handler))
        .route("/v1/auth/register/finish", post(register_finish_handler))
        .route("/v1/auth/login/start", post(login_start_handler))
        .route("/v1/auth/login/finish", post(login_finish_handler))
        // Signing Keys (HTTP Signatures)
        .route("/v1/signing-keys", post(create_signing_key_handler))
        .route("/v1/signing-keys", get(list_signing_keys))
        .route("/v1/signing-keys/{key_id}", get(get_signing_key_handler))
        .route(
            "/v1/signing-keys/{key_id}",
            delete(revoke_signing_key_handler),
        )
        .merge(oauth_routes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_router_compiles() {
        // Just ensure the router can be constructed
        let _router = router();
    }
}
