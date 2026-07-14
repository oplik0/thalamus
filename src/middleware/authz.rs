//! Casbin authorization middleware
//!
//! Provides middleware for enforcing Casbin-based authorization on routes.
//! This middleware should be applied after authentication middleware.

use crate::bootstrap::AppState;
use crate::error::Error;
use crate::features::authorization::domain::{AuthRequest, Authorizer};
use crate::middleware::auth::Auth;
use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::sync::Arc;

/// Authorization middleware that checks Casbin policies
///
/// This middleware extracts the authenticated user from the request extensions
/// and checks if they have permission to access the requested resource.
///
/// # Usage
///
/// Apply this middleware to routes that need authorization checks:
///
/// ```ignore
/// use axum::{
///     routing::get,
///     Router,
///     middleware,
/// };
/// use thalamus::bootstrap::AppState;
/// use thalamus::middleware::authz::casbin_auth_middleware;
///
/// async fn protected_handler() -> &'static str { "protected" }
///
/// // Create AppState with proper initialization in real code
/// let state: AppState = todo!();
/// let app: Router<AppState> = Router::new()
///     .route("/protected", get(protected_handler))
///     .layer(middleware::from_fn_with_state(state, casbin_auth_middleware));
/// ```
///
/// # Request Format
///
/// The middleware expects:
/// - `Auth` in request extensions (from authentication middleware)
/// - Request path as the "object"
/// - Request method as the "action"
/// - Team ID from `Auth` as the "domain"
pub async fn casbin_auth_middleware(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    // Try to get the authenticated user from extensions
    let auth = match request.extensions().get::<Auth>() {
        Some(auth) => auth.clone(),
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                "Authentication required for authorization check",
            )
                .into_response();
        }
    };

    // Get the authorizer from state
    let authorizer = if let Some(authorizer) = &state.authorizer {
        authorizer.clone()
    } else {
        tracing::error!("Authorizer not initialized in AppState");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Authorization system not initialized",
        )
            .into_response();
    };

    // Build the authorization request
    let path = request.uri().path().to_string();
    let method = request.method().to_string();
    let team_id = auth.team_id.to_string();

    // Use username as subject, or user_id if username not available
    let subject = auth
        .key_id
        .clone()
        .unwrap_or_else(|| auth.user_id.to_string());

    let auth_request = AuthRequest::new(&subject, &team_id, &path, &method);

    tracing::debug!(
        subject = %subject,
        domain = %team_id,
        object = %path,
        action = %method,
        "Checking Casbin authorization"
    );

    // Check authorization
    match authorizer.is_authorized(&auth_request).await {
        Ok(true) => {
            tracing::debug!("Authorization granted");
            next.run(request).await
        }
        Ok(false) => {
            tracing::warn!(
                subject = %subject,
                domain = %team_id,
                object = %path,
                action = %method,
                "Authorization denied"
            );
            (
                StatusCode::FORBIDDEN,
                "Access denied by authorization policy",
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "Authorization check failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Authorization check failed",
            )
                .into_response()
        }
    }
}

/// Middleware factory for requiring specific Casbin permissions
///
/// This creates middleware that checks for a specific permission pattern
/// rather than deriving it from the request path/method.
///
/// # Arguments
/// * `object` - The resource pattern to check (e.g., "/v1/chat/completions")
/// * `action` - The action to check (e.g., "POST")
///
/// # Example
///
/// ```ignore
/// use axum::{
///     routing::post,
///     Router,
///     middleware,
/// };
/// use thalamus::bootstrap::AppState;
/// use thalamus::middleware::authz::require_permission;
///
/// async fn chat_handler() -> &'static str { "chat" }
///
/// // Create AppState with proper initialization in real code
/// let state: AppState = todo!();
/// let app: Router<AppState> = Router::new()
///     .route("/chat", post(chat_handler))
///     .layer(middleware::from_fn_with_state(
///         state,
///         require_permission("/v1/chat/completions", "POST")
///     ));
/// ```
pub fn require_permission(
    object: impl Into<String>,
    action: impl Into<String>,
) -> impl Fn(
    State<AppState>,
    Request,
    Next,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>>
+ Clone {
    let object = Arc::new(object.into());
    let action = Arc::new(action.into());

    move |State(state): State<AppState>, request: Request, next: Next| {
        let object = object.clone();
        let action = action.clone();

        Box::pin(async move {
            let auth = match request.extensions().get::<Auth>() {
                Some(auth) => auth.clone(),
                None => {
                    return (
                        StatusCode::UNAUTHORIZED,
                        "Authentication required for authorization check",
                    )
                        .into_response();
                }
            };

            let authorizer = match &state.authorizer {
                Some(authorizer) => authorizer.clone(),
                None => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Authorization system not initialized",
                    )
                        .into_response();
                }
            };

            let team_id = auth.team_id.to_string();
            let subject = auth
                .key_id
                .clone()
                .unwrap_or_else(|| auth.user_id.to_string());

            let auth_request = AuthRequest::new(&subject, &team_id, &*object, &*action);

            match authorizer.is_authorized(&auth_request).await {
                Ok(true) => next.run(request).await,
                Ok(false) => (
                    StatusCode::FORBIDDEN,
                    "Access denied by authorization policy",
                )
                    .into_response(),
                Err(_) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Authorization check failed",
                )
                    .into_response(),
            }
        })
    }
}

/// Extension trait to add authorization checks to Auth
///
/// This trait provides convenient methods for checking authorization
/// directly from the Auth struct.
pub trait AuthzExt {
    /// Check if this auth has permission for a specific action on an object
    ///
    /// # Arguments
    /// * `state` - The application state containing the authorizer
    /// * `object` - The resource being accessed
    /// * `action` - The action being performed
    ///
    /// # Returns
    /// `true` if authorized, `false` otherwise
    fn is_authorized(
        &self,
        state: &AppState,
        object: &str,
        action: &str,
    ) -> impl std::future::Future<Output = Result<bool, Error>> + Send;

    /// Enforce authorization, returning an error if not authorized
    ///
    /// # Errors
    /// Returns an Authorization error if not permitted
    fn enforce(
        &self,
        state: &AppState,
        object: &str,
        action: &str,
    ) -> impl std::future::Future<Output = Result<(), Error>> + Send;
}

impl AuthzExt for Auth {
    async fn is_authorized(
        &self,
        state: &AppState,
        object: &str,
        action: &str,
    ) -> Result<bool, Error> {
        let authorizer = state
            .authorizer
            .as_ref()
            .ok_or_else(|| Error::Config("Authorization not initialized".to_string()))?;

        let team_id = self.team_id.to_string();
        let subject = self
            .key_id
            .clone()
            .unwrap_or_else(|| self.user_id.to_string());

        let auth_request = AuthRequest::new(subject, team_id, object, action);
        authorizer.is_authorized(&auth_request).await
    }

    async fn enforce(&self, state: &AppState, object: &str, action: &str) -> Result<(), Error> {
        let authorized = self.is_authorized(state, object, action).await?;
        if authorized {
            Ok(())
        } else {
            Err(Error::Authorization(format!(
                "Access denied: cannot {action} {object}"
            )))
        }
    }
}
