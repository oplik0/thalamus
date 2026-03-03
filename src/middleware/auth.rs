use crate::bootstrap::AppState;
use crate::error::{Error, Result};
use crate::features::auth::infra::http_signature::HttpSignatureVerifier;
use crate::features::auth::infra::key_storage::validate_key;
use crate::features::auth::infra::token_service::validate_token;
use axum::{
    extract::FromRequestParts,
    http::{header::AUTHORIZATION, request::Parts},
};
use uuid::Uuid;

/// Authenticated user/service information
///
/// This struct unifies API key and Token authentication.
/// It provides a common interface for accessing user, team, and scope information.
#[derive(Debug, Clone)]
pub struct Auth {
    pub user_id: Uuid,
    pub team_id: Uuid,
    pub scopes: Option<Vec<String>>,
    pub roles: Option<Vec<String>>,
    pub key_id: Option<String>, // Only present if authenticated via API key
    pub token_id: Option<Uuid>, // Only present if authenticated via Token
}

impl Auth {
    /// Check if the authenticated entity has the required scope
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes
            .as_ref()
            .map(|s| s.contains(&scope.to_string()))
            .unwrap_or(false)
    }

    /// Check if the authenticated entity has any of the required scopes
    pub fn has_any_scope(&self, scopes: &[&str]) -> bool {
        self.scopes
            .as_ref()
            .map(|s| scopes.iter().any(|scope| s.contains(&scope.to_string())))
            .unwrap_or(false)
    }

    /// Check if the authenticated entity has all of the required scopes
    pub fn has_all_scopes(&self, scopes: &[&str]) -> bool {
        self.scopes
            .as_ref()
            .map(|s| scopes.iter().all(|scope| s.contains(&scope.to_string())))
            .unwrap_or(false)
    }
}

/// Extractor for authentication (API Key or PASETO Token)
///
/// Use this in your route handlers to automatically authenticate requests:
///
/// ```rust
/// use axum::Json;
/// use serde_json::json;
/// use axum::response::IntoResponse;
/// use thalamus::middleware::ApiKeyAuth;
///
/// async fn protected_route(
///     ApiKeyAuth(auth): ApiKeyAuth,
/// ) -> impl IntoResponse {
///     Json(json!({
///         "user_id": auth.user_id,
///         "team_id": auth.team_id,
///     }))
/// }
/// ```
#[derive(Debug, Clone)]
pub struct ApiKeyAuth(pub Auth);

impl FromRequestParts<AppState> for ApiKeyAuth {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self> {
        // First, check for HTTP Signature authentication
        if parts.headers.contains_key("signature") && parts.headers.contains_key("signature-input")
        {
            return Self::authenticate_http_signature(parts, state).await;
        }

        // Fall back to Bearer token authentication
        Self::authenticate_bearer(parts, state).await
    }
}

impl ApiKeyAuth {
    /// Authenticate using Bearer token (PASETO or API key)
    async fn authenticate_bearer(parts: &mut Parts, state: &AppState) -> Result<Self> {
        // Extract the Authorization header
        let auth_header = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| Error::Authentication("Missing Authorization header".to_string()))?;

        // Check for Bearer token format
        let token_str = if let Some(token) = auth_header.strip_prefix("Bearer ") {
            token
        } else {
            return Err(Error::Authentication(
                "Authorization header must use Bearer scheme".to_string(),
            ));
        };

        // Determine authentication type based on prefix
        if token_str.starts_with("v4.public.") || token_str.starts_with("v4.local.") {
            // PASETO Token
            let claims = validate_token(token_str, state).await?;

            Ok(ApiKeyAuth(Auth {
                user_id: claims.sub,
                team_id: claims.dom,
                scopes: claims.scopes,
                roles: claims.roles,
                key_id: None,
                token_id: Some(claims.jti),
            }))
        } else {
            // API Key (assume API key if not PASETO)
            // We could check for specific prefixes like "thl_" but let validate_key handle that
            let validated = validate_key(token_str, state).await?;

            Ok(ApiKeyAuth(Auth {
                user_id: validated.user_id,
                team_id: validated.team_id,
                scopes: validated.scopes,
                roles: None, // API keys don't currently carry roles, but could be fetched
                key_id: Some(validated.key_id),
                token_id: None,
            }))
        }
    }

    /// Authenticate using HTTP Signature (RFC 9421)
    async fn authenticate_http_signature(parts: &mut Parts, state: &AppState) -> Result<Self> {
        let method = &parts.method;
        let uri = &parts.uri;

        let verified = HttpSignatureVerifier::verify(method, uri, &parts.headers, state).await?;

        Ok(ApiKeyAuth(Auth {
            user_id: verified.user_id,
            team_id: verified.team_id,
            scopes: verified.scopes,
            roles: None, // HTTP signatures don't currently carry roles
            key_id: Some(verified.key_id),
            token_id: None,
        }))
    }
}

/// Extractor for optional authentication
#[derive(Debug, Clone)]
pub struct OptionalApiKeyAuth(pub Option<Auth>);

impl FromRequestParts<AppState> for OptionalApiKeyAuth {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self> {
        // Try to extract the Authorization header
        let auth_header = match parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
        {
            Some(header) => header,
            None => return Ok(OptionalApiKeyAuth(None)),
        };

        // Check for Bearer token format
        let token_str = match auth_header.strip_prefix("Bearer ") {
            Some(token) => token,
            None => return Ok(OptionalApiKeyAuth(None)),
        };

        // Determine authentication type based on prefix
        if token_str.starts_with("v4.public.") || token_str.starts_with("v4.local.") {
            // PASETO Token
            match validate_token(token_str, state).await {
                Ok(claims) => Ok(OptionalApiKeyAuth(Some(Auth {
                    user_id: claims.sub,
                    team_id: claims.dom,
                    scopes: claims.scopes,
                    roles: claims.roles,
                    key_id: None,
                    token_id: Some(claims.jti),
                }))),
                Err(_) => Ok(OptionalApiKeyAuth(None)),
            }
        } else {
            // API Key
            match validate_key(token_str, state).await {
                Ok(validated) => Ok(OptionalApiKeyAuth(Some(Auth {
                    user_id: validated.user_id,
                    team_id: validated.team_id,
                    scopes: validated.scopes,
                    roles: None,
                    key_id: Some(validated.key_id),
                    token_id: None,
                }))),
                Err(_) => Ok(OptionalApiKeyAuth(None)),
            }
        }
    }
}

/// Middleware to check if a key has a specific scope
pub fn require_scope(auth: &Auth, required_scope: &str) -> Result<()> {
    if auth.has_scope(required_scope) {
        Ok(())
    } else {
        Err(Error::Authorization(format!(
            "Missing required scope: {}",
            required_scope
        )))
    }
}

/// Middleware to check if a key has any of the specified scopes
pub fn require_any_scope(auth: &Auth, required_scopes: &[&str]) -> Result<()> {
    if auth.has_any_scope(required_scopes) {
        Ok(())
    } else {
        Err(Error::Authorization(format!(
            "Missing any of required scopes: {}",
            required_scopes.join(", ")
        )))
    }
}

/// Middleware to check if a key has all of the specified scopes
pub fn require_all_scopes(auth: &Auth, required_scopes: &[&str]) -> Result<()> {
    if auth.has_all_scopes(required_scopes) {
        Ok(())
    } else {
        let missing: Vec<_> = required_scopes
            .iter()
            .filter(|scope| !auth.has_scope(scope))
            .map(|s| s.to_string())
            .collect();

        Err(Error::Authorization(format!(
            "Missing required scopes: {}",
            missing.join(", ")
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn test_auth_has_scope() {
        let auth = Auth {
            user_id: Uuid::new_v4(),
            team_id: Uuid::new_v4(),
            scopes: Some(vec!["read".to_string(), "write".to_string()]),
            roles: None,
            key_id: Some("test_key".to_string()),
            token_id: None,
        };

        assert!(auth.has_scope("read"));
        assert!(auth.has_scope("write"));
        assert!(!auth.has_scope("admin"));
    }

    #[test]
    fn test_auth_has_scope_no_scopes() {
        let auth = Auth {
            user_id: Uuid::new_v4(),
            team_id: Uuid::new_v4(),
            scopes: None,
            roles: None,
            key_id: Some("test_key".to_string()),
            token_id: None,
        };

        assert!(!auth.has_scope("read"));
        assert!(!auth.has_scope(""));
    }

    #[test]
    fn test_auth_has_any_scope() {
        let auth = Auth {
            user_id: Uuid::new_v4(),
            team_id: Uuid::new_v4(),
            scopes: Some(vec!["read".to_string()]),
            roles: None,
            key_id: Some("test_key".to_string()),
            token_id: None,
        };

        assert!(auth.has_any_scope(&["read", "write"]));
        assert!(auth.has_any_scope(&["read"]));
        assert!(!auth.has_any_scope(&["admin", "delete"]));
        assert!(!auth.has_any_scope(&[]));
    }

    #[test]
    fn test_auth_has_any_scope_no_scopes() {
        let auth = Auth {
            user_id: Uuid::new_v4(),
            team_id: Uuid::new_v4(),
            scopes: None,
            roles: None,
            key_id: Some("test_key".to_string()),
            token_id: None,
        };

        assert!(!auth.has_any_scope(&["read", "write"]));
    }

    #[test]
    fn test_auth_has_all_scopes() {
        let auth = Auth {
            user_id: Uuid::new_v4(),
            team_id: Uuid::new_v4(),
            scopes: Some(vec!["read".to_string(), "write".to_string()]),
            roles: None,
            key_id: Some("test_key".to_string()),
            token_id: None,
        };

        assert!(auth.has_all_scopes(&["read", "write"]));
        assert!(auth.has_all_scopes(&["read"]));
        assert!(!auth.has_all_scopes(&["read", "write", "admin"]));
        assert!(auth.has_all_scopes(&[]));
    }

    #[test]
    fn test_auth_has_all_scopes_no_scopes() {
        let auth = Auth {
            user_id: Uuid::new_v4(),
            team_id: Uuid::new_v4(),
            scopes: None,
            roles: None,
            key_id: Some("test_key".to_string()),
            token_id: None,
        };

        // When auth has no scopes, it can't satisfy any scope requirements
        assert!(!auth.has_all_scopes(&["read"]));
        // Empty requirements should still return false when auth has no scopes
        // because the auth entity has no scopes at all
        assert!(!auth.has_all_scopes(&[]));
    }

    #[test]
    fn test_require_scope() {
        let auth = Auth {
            user_id: Uuid::new_v4(),
            team_id: Uuid::new_v4(),
            scopes: Some(vec!["read".to_string(), "write".to_string()]),
            roles: None,
            key_id: Some("test_key".to_string()),
            token_id: None,
        };

        assert!(require_scope(&auth, "read").is_ok());
        assert!(require_scope(&auth, "write").is_ok());
        assert!(require_scope(&auth, "admin").is_err());
    }

    #[test]
    fn test_require_scope_error_message() {
        let auth = Auth {
            user_id: Uuid::new_v4(),
            team_id: Uuid::new_v4(),
            scopes: Some(vec!["read".to_string()]),
            roles: None,
            key_id: Some("test_key".to_string()),
            token_id: None,
        };

        let err = require_scope(&auth, "admin").unwrap_err();
        let err_string = format!("{}", err);
        assert!(err_string.contains("Missing required scope: admin"));
    }

    #[test]
    fn test_require_any_scope() {
        let auth = Auth {
            user_id: Uuid::new_v4(),
            team_id: Uuid::new_v4(),
            scopes: Some(vec!["read".to_string()]),
            roles: None,
            key_id: Some("test_key".to_string()),
            token_id: None,
        };

        assert!(require_any_scope(&auth, &["read", "write"]).is_ok());
        assert!(require_any_scope(&auth, &["admin", "delete"]).is_err());
    }

    #[test]
    fn test_require_any_scope_error_message() {
        let auth = Auth {
            user_id: Uuid::new_v4(),
            team_id: Uuid::new_v4(),
            scopes: Some(vec!["read".to_string()]),
            roles: None,
            key_id: Some("test_key".to_string()),
            token_id: None,
        };

        let err = require_any_scope(&auth, &["admin", "delete"]).unwrap_err();
        let err_string = format!("{}", err);
        assert!(err_string.contains("Missing any of required scopes: admin, delete"));
    }

    #[test]
    fn test_require_all_scopes() {
        let auth = Auth {
            user_id: Uuid::new_v4(),
            team_id: Uuid::new_v4(),
            scopes: Some(vec!["read".to_string(), "write".to_string()]),
            roles: None,
            key_id: Some("test_key".to_string()),
            token_id: None,
        };

        assert!(require_all_scopes(&auth, &["read", "write"]).is_ok());
        assert!(require_all_scopes(&auth, &["read"]).is_ok());
        assert!(require_all_scopes(&auth, &["read", "write", "admin"]).is_err());
    }

    #[test]
    fn test_require_all_scopes_error_message() {
        let auth = Auth {
            user_id: Uuid::new_v4(),
            team_id: Uuid::new_v4(),
            scopes: Some(vec!["read".to_string()]),
            roles: None,
            key_id: Some("test_key".to_string()),
            token_id: None,
        };

        let err = require_all_scopes(&auth, &["read", "write"]).unwrap_err();
        let err_string = format!("{}", err);
        assert!(err_string.contains("Missing required scopes: write"));
    }

    #[test]
    fn test_auth_clone() {
        let auth = Auth {
            user_id: Uuid::new_v4(),
            team_id: Uuid::new_v4(),
            scopes: Some(vec!["read".to_string()]),
            roles: Some(vec!["user".to_string()]),
            key_id: Some("test_key".to_string()),
            token_id: Some(Uuid::new_v4()),
        };

        let cloned = auth.clone();
        assert_eq!(auth.user_id, cloned.user_id);
        assert_eq!(auth.team_id, cloned.team_id);
        assert_eq!(auth.scopes, cloned.scopes);
        assert_eq!(auth.roles, cloned.roles);
        assert_eq!(auth.key_id, cloned.key_id);
        assert_eq!(auth.token_id, cloned.token_id);
    }

    #[test]
    fn test_api_key_auth_clone() {
        let auth = Auth {
            user_id: Uuid::new_v4(),
            team_id: Uuid::new_v4(),
            scopes: Some(vec!["read".to_string()]),
            roles: None,
            key_id: Some("test_key".to_string()),
            token_id: None,
        };

        let api_key_auth = ApiKeyAuth(auth);
        let cloned = api_key_auth.clone();
        assert_eq!(api_key_auth.0.user_id, cloned.0.user_id);
    }
}
