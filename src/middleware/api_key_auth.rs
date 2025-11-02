use crate::bootstrap::AppState;
use crate::error::{Error, Result};
use crate::features::auth::domain::api_key::ValidatedApiKey;
use crate::features::auth::infra::validate_key;
use axum::{
    extract::FromRequestParts,
    http::{header::AUTHORIZATION, request::Parts},
};

/// Extractor for API key authentication
///
/// Use this in your route handlers to automatically validate API keys:
///
/// ```rust
/// async fn protected_route(
///     ApiKeyAuth(key_info): ApiKeyAuth,
/// ) -> impl IntoResponse {
///     Json(json!({
///         "user_id": key_info.user_id,
///         "team_id": key_info.team_id,
///     }))
/// }
/// ```
#[derive(Debug, Clone)]
pub struct ApiKeyAuth(pub ValidatedApiKey);

impl FromRequestParts<AppState> for ApiKeyAuth {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self> {
        // Extract the Authorization header
        let auth_header = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| Error::Authentication("Missing Authorization header".to_string()))?;

        // Check for Bearer token format
        let api_key = if let Some(key) = auth_header.strip_prefix("Bearer ") {
            key
        } else {
            return Err(Error::Authentication(
                "Authorization header must use Bearer scheme".to_string(),
            ));
        };

        // Validate the key
        let validated = validate_key(api_key, state).await?;

        Ok(ApiKeyAuth(validated))
    }
}

/// Extractor for optional API key authentication
///
/// Use this when API key authentication is optional:
///
/// ```rust
/// async fn optional_route(
///     OptionalApiKeyAuth(key_info): OptionalApiKeyAuth,
/// ) -> impl IntoResponse {
///     match key_info {
///         Some(info) => format!("Authenticated as user {}", info.user_id),
///         None => "Anonymous access".to_string(),
///     }
/// }
/// ```
#[derive(Debug, Clone)]
pub struct OptionalApiKeyAuth(pub Option<ValidatedApiKey>);

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
        let api_key = match auth_header.strip_prefix("Bearer ") {
            Some(key) => key,
            None => return Ok(OptionalApiKeyAuth(None)),
        };

        // Validate the key
        match validate_key(api_key, state).await {
            Ok(validated) => Ok(OptionalApiKeyAuth(Some(validated))),
            Err(_) => Ok(OptionalApiKeyAuth(None)),
        }
    }
}

/// Middleware to check if a key has a specific scope
///
/// Use this to enforce scope-based authorization:
///
/// ```rust
/// async fn admin_route(
///     ApiKeyAuth(key_info): ApiKeyAuth,
/// ) -> Result<impl IntoResponse> {
///     require_scope(&key_info, "admin:write")?;
///     // ... admin operation
/// }
/// ```
pub fn require_scope(key_info: &ValidatedApiKey, required_scope: &str) -> Result<()> {
    match &key_info.scopes {
        Some(scopes) if scopes.contains(&required_scope.to_string()) => Ok(()),
        _ => Err(Error::Authorization(format!(
            "Missing required scope: {}",
            required_scope
        ))),
    }
}

/// Middleware to check if a key has any of the specified scopes
pub fn require_any_scope(key_info: &ValidatedApiKey, required_scopes: &[&str]) -> Result<()> {
    match &key_info.scopes {
        Some(scopes) => {
            if required_scopes
                .iter()
                .any(|scope| scopes.contains(&scope.to_string()))
            {
                Ok(())
            } else {
                Err(Error::Authorization(format!(
                    "Missing any of required scopes: {}",
                    required_scopes.join(", ")
                )))
            }
        }
        None => Err(Error::Authorization(
            "Key has no scopes assigned".to_string(),
        )),
    }
}

/// Middleware to check if a key has all of the specified scopes
pub fn require_all_scopes(key_info: &ValidatedApiKey, required_scopes: &[&str]) -> Result<()> {
    match &key_info.scopes {
        Some(scopes) => {
            let missing: Vec<_> = required_scopes
                .iter()
                .filter(|scope| !scopes.contains(&scope.to_string()))
                .collect();

            if missing.is_empty() {
                Ok(())
            } else {
                Err(Error::Authorization(format!(
                    "Missing required scopes: {}",
                    missing
                        .iter()
                        .map(|s| s.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )))
            }
        }
        None => Err(Error::Authorization(
            "Key has no scopes assigned".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn test_require_scope() {
        let key_info = ValidatedApiKey {
            id: Uuid::new_v4(),
            key_id: "test_key".to_string(),
            user_id: Uuid::new_v4(),
            team_id: Uuid::new_v4(),
            scopes: Some(vec!["read".to_string(), "write".to_string()]),
        };

        assert!(require_scope(&key_info, "read").is_ok());
        assert!(require_scope(&key_info, "write").is_ok());
        assert!(require_scope(&key_info, "admin").is_err());
    }

    #[test]
    fn test_require_any_scope() {
        let key_info = ValidatedApiKey {
            id: Uuid::new_v4(),
            key_id: "test_key".to_string(),
            user_id: Uuid::new_v4(),
            team_id: Uuid::new_v4(),
            scopes: Some(vec!["read".to_string()]),
        };

        assert!(require_any_scope(&key_info, &["read", "write"]).is_ok());
        assert!(require_any_scope(&key_info, &["admin", "delete"]).is_err());
    }

    #[test]
    fn test_require_all_scopes() {
        let key_info = ValidatedApiKey {
            id: Uuid::new_v4(),
            key_id: "test_key".to_string(),
            user_id: Uuid::new_v4(),
            team_id: Uuid::new_v4(),
            scopes: Some(vec!["read".to_string(), "write".to_string()]),
        };

        assert!(require_all_scopes(&key_info, &["read", "write"]).is_ok());
        assert!(require_all_scopes(&key_info, &["read"]).is_ok());
        assert!(require_all_scopes(&key_info, &["read", "write", "admin"]).is_err());
    }
}
