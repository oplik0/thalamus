//! OAuth API routes

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};

use crate::bootstrap::AppState;
use crate::error::Result;
use crate::middleware::{ApiKeyAuth, require_scope};

/// Query parameters for OAuth login initiation
#[derive(Debug, Deserialize)]
pub struct OAuthLoginQuery {
    /// Optional redirect URL after login
    pub redirect_url: Option<String>,
}

/// Response for OAuth login initiation
#[derive(Debug, Serialize)]
pub struct OAuthLoginResponse {
    /// The authorization URL to redirect the user to
    pub authorization_url: String,
    /// The state token (for CSRF protection)
    pub state: String,
}

/// Query parameters for OAuth callback
#[derive(Debug, Deserialize)]
pub struct OAuthCallbackQuery {
    /// Authorization code from provider
    pub code: String,
    /// State token for CSRF validation
    pub state: String,
}

/// Response for successful OAuth callback
#[derive(Debug, Serialize)]
pub struct OAuthCallbackResponse {
    /// PASETO token for session
    pub token: String,
    /// User ID
    pub user_id: String,
    /// Team ID
    pub team_id: String,
    /// Whether this is a new user
    pub is_new_user: bool,
}

/// OAuth provider information
#[derive(Debug, Serialize)]
pub struct OAuthProviderInfo {
    pub name: String,
    pub provider_type: String,
}

/// List configured OAuth providers
pub async fn list_providers(State(state): State<AppState>) -> Result<Json<Vec<OAuthProviderInfo>>> {
    let providers = state.oauth_service.list_providers();

    let provider_infos: Vec<OAuthProviderInfo> = providers
        .into_iter()
        .map(|p| OAuthProviderInfo {
            name: p.name,
            provider_type: p.provider_type,
        })
        .collect();

    Ok(Json(provider_infos))
}

/// Start OAuth login flow
pub async fn oauth_login(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    Query(query): Query<OAuthLoginQuery>,
) -> Result<Json<OAuthLoginResponse>> {
    let config = state.config.as_ref();
    let base_url = config
        .server
        .base_url
        .clone()
        .unwrap_or_else(|| format!("http://{}:{}", config.server.host, config.server.port));

    let response = state
        .oauth_service
        .initiate_oauth_login(&provider, query.redirect_url, &base_url)
        .await?;

    Ok(Json(OAuthLoginResponse {
        authorization_url: response.authorization_url,
        state: response.state,
    }))
}

/// Handle OAuth callback - redirects to frontend with token
pub async fn oauth_callback(
    State(state): State<AppState>,
    Path(_provider): Path<String>,
    Query(query): Query<OAuthCallbackQuery>,
) -> Result<Response> {
    let config = state.config.as_ref();
    let base_url = config
        .server
        .base_url
        .clone()
        .unwrap_or_else(|| format!("http://{}:{}", config.server.host, config.server.port));

    let result = state
        .oauth_service
        .handle_oauth_callback(&query.state, &query.code, &base_url, &state)
        .await?;

    // Redirect to the frontend callback URL with the token
    // The frontend will extract the token and complete the login
    // Build the full URL - the redirect_url from OAuth state is relative
    let redirect_path = result.redirect_url.unwrap_or_else(|| "/".to_string());

    // Build the full redirect URL (prepend base URL if relative)
    let final_url = if redirect_path.starts_with("http") {
        redirect_path
    } else {
        format!("{base_url}{redirect_path}")
    };

    // Add token as query params
    let final_url = format!(
        "{}?token={}&user_id={}&team_id={}&is_new_user={}",
        final_url,
        urlencoding::encode(&result.token),
        result.user_id,
        result.team_id,
        result.is_new_user
    );

    tracing::debug!(redirect_url = %final_url, "Redirecting OAuth callback to frontend");

    Ok((StatusCode::FOUND, [(header::LOCATION, final_url)]).into_response())
}

/// Link OAuth account to existing user (requires authentication)
pub async fn link_oauth_account(
    State(_state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Path(provider): Path<String>,
) -> Result<Json<serde_json::Value>> {
    require_scope(&auth, "oauth:link")?;

    // TODO: Implement OAuth linking
    Ok(Json(serde_json::json!({
        "message": "OAuth account linking not yet implemented",
        "provider": provider
    })))
}

/// Unlink OAuth account (requires authentication)
pub async fn unlink_oauth_account(
    State(_state): State<AppState>,
    ApiKeyAuth(auth): ApiKeyAuth,
    Path(provider): Path<String>,
) -> Result<Json<serde_json::Value>> {
    require_scope(&auth, "oauth:unlink")?;

    // TODO: Implement OAuth unlinking
    Ok(Json(serde_json::json!({
        "message": "OAuth account unlinking not yet implemented",
        "provider": provider
    })))
}

/// Create the OAuth API router
pub fn oauth_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/auth/oauth/providers", get(list_providers))
        .route("/v1/auth/oauth/{provider}/login", get(oauth_login))
        .route("/v1/auth/oauth/{provider}/callback", get(oauth_callback))
        .route("/v1/auth/oauth/{provider}/link", post(link_oauth_account))
        .route(
            "/v1/auth/oauth/{provider}/unlink",
            delete(unlink_oauth_account),
        )
}
