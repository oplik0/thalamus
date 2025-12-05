use axum::{
    Json, Router,
    extract::State,
    routing::{get, post},
};
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::bootstrap::AppState;
use crate::error::Result;
use crate::features::auth::domain::api_key::{CreateApiKeyRequest, CreateApiKeyResponse};
use crate::features::auth::domain::keys::{Prefix, generate_key};
use crate::features::auth::infra::{list_user_keys, revoke_key};
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
        "scopes": auth.scopes,
    })))
}

/// Create the API router
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/keys", post(create_key))
        .route("/api/keys", get(list_keys))
        .route("/api/keys/revoke", post(revoke_key_handler))
        .route("/api/auth/whoami", get(whoami))
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
