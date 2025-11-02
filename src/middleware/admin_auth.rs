//! Admin authentication middleware
//!
//! Ensures that only API keys with admin scopes can access admin routes.

use crate::bootstrap::AppState;
use crate::features::auth::infra::validate_key;
use axum::{
    extract::{Request, State},
    http::{StatusCode, header::AUTHORIZATION},
    middleware::Next,
    response::{IntoResponse, Response},
};

/// Middleware that requires admin scope for the request
///
/// This middleware checks that the request has a valid API key with the "admin" scope.
/// It should be applied to admin routes to prevent unauthorized access.
pub async fn require_admin(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    match authenticate_and_check_scope(&state, &request, &["admin"]).await {
        Ok(_) => next.run(request).await,
        Err(response) => response,
    }
}

/// Middleware that requires task monitoring scope
///
/// This middleware checks that the request has a valid API key with either
/// "admin" or "tasks:monitor" scope.
pub async fn require_task_monitor(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    match authenticate_and_check_scope(&state, &request, &["admin", "tasks:monitor"]).await {
        Ok(_) => next.run(request).await,
        Err(response) => response,
    }
}

/// Helper function to authenticate and check scopes
async fn authenticate_and_check_scope(
    state: &AppState,
    request: &Request,
    required_scopes: &[&str],
) -> Result<(), Response> {
    // Extract the Authorization header
    let auth_header = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| {
            (StatusCode::UNAUTHORIZED, "Missing Authorization header").into_response()
        })?;

    // Parse Bearer token
    let token = auth_header.strip_prefix("Bearer ").ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            "Invalid Authorization header format. Expected: Bearer <token>",
        )
            .into_response()
    })?;

    // Validate the key
    let validated_key = validate_key(token, state).await.map_err(|e| {
        (
            StatusCode::UNAUTHORIZED,
            format!("Authentication failed: {}", e),
        )
            .into_response()
    })?;

    // Check if the key has any of the required scopes
    let has_required_scope = validated_key
        .scopes
        .as_ref()
        .map(|scopes| {
            required_scopes
                .iter()
                .any(|scope| scopes.contains(&scope.to_string()))
        })
        .unwrap_or(false);

    if !has_required_scope {
        return Err((
            StatusCode::FORBIDDEN,
            format!("Required scope: {}", required_scopes.join(" or ")),
        )
            .into_response());
    }

    Ok(())
}
