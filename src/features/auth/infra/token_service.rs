use crate::bootstrap::AppState;
use crate::error::{Error, Result};
use crate::features::auth::domain::token::TokenClaims;
use crate::features::auth::infra::token_revocation::is_token_revoked;
use pasetors::claims::{Claims, ClaimsValidationRules};
use pasetors::keys::SymmetricKey;
use pasetors::token::UntrustedToken;
use pasetors::version4::V4;
use pasetors::{Local, local};
use std::convert::TryFrom;

/// Create a PASETO v4.local token from claims
pub fn create_token(claims: &TokenClaims, state: &AppState) -> Result<String> {
    // Convert TokenClaims to pasetors Claims
    let mut paseto_claims = Claims::new()?;

    // Standard claims
    paseto_claims.subject(&claims.sub.to_string())?;
    paseto_claims.issued_at(&claims.iat.to_rfc3339())?;
    paseto_claims.expiration(&claims.exp.to_rfc3339())?;
    paseto_claims.token_identifier(&claims.jti.to_string())?;

    if let Some(nbf) = claims.nbf {
        paseto_claims.not_before(&nbf.to_rfc3339())?;
    }

    // Custom claims
    paseto_claims.add_additional("dom", serde_json::to_value(&claims.dom)?)?;

    if let Some(roles) = &claims.roles {
        paseto_claims.add_additional("roles", serde_json::to_value(roles)?)?;
    }

    if let Some(scopes) = &claims.scopes {
        paseto_claims.add_additional("scopes", serde_json::to_value(scopes)?)?;
    }

    // Get the symmetric key from config
    let key_bytes = state.config.security.paseto_secret_key.as_bytes();
    // TODO: check if this is correct??
    if key_bytes.len() != 32 {
        return Err(Error::Internal(
            "paseto_secret_key must be exactly 32 bytes".to_string(),
        ));
    }
    let symmetric_key = SymmetricKey::<V4>::from(key_bytes)
        .map_err(|e| Error::Internal(format!("Failed to create symmetric key: {}", e)))?;

    // Create the token
    let token = local::encrypt(&symmetric_key, &paseto_claims, None, None)
        .map_err(|e| Error::Internal(format!("Failed to create token: {}", e)))?;

    Ok(token)
}

/// Validate and parse a PASETO v4.local token
pub async fn validate_token(token: &str, state: &AppState) -> Result<TokenClaims> {
    // Get the symmetric key from config
    let key_bytes = state.config.security.paseto_secret_key.as_bytes();
    if key_bytes.len() != 32 {
        return Err(Error::Internal(
            "paseto_secret_key must be exactly 32 bytes".to_string(),
        ));
    }
    let symmetric_key = SymmetricKey::<V4>::from(key_bytes)
        .map_err(|e| Error::Internal(format!("Failed to create symmetric key: {}", e)))?;

    // Decrypt and validate the token
    // Validate the token
    let validation_rules = ClaimsValidationRules::new();
    // Default rules validate expiration and not-before if present

    let untrusted_token = UntrustedToken::<Local, V4>::try_from(token)
        .map_err(|e| Error::Authentication(format!("Invalid token format: {}", e)))?;

    let trusted_token = local::decrypt(
        &symmetric_key,
        &untrusted_token,
        &validation_rules,
        None,
        None,
    )
    .map_err(|e| Error::Authentication(format!("Token validation failed: {}", e)))?;

    let claims = trusted_token
        .payload_claims()
        .ok_or_else(|| Error::Authentication("Token has no claims".to_string()))?;

    // Extract claims
    let sub_str = claims
        .get_claim("sub")
        .ok_or_else(|| Error::Authentication("Missing subject claim".to_string()))?
        .as_str()
        .ok_or_else(|| Error::Authentication("Invalid subject claim".to_string()))?;
    let sub = uuid::Uuid::parse_str(sub_str)
        .map_err(|_| Error::Authentication("Invalid subject UUID".to_string()))?;

    let dom_value = claims
        .get_claim("dom")
        .ok_or_else(|| Error::Authentication("Missing domain claim".to_string()))?;
    let dom: uuid::Uuid = serde_json::from_value(dom_value.clone())
        .map_err(|_| Error::Authentication("Invalid domain UUID".to_string()))?;

    let iat_str = claims
        .get_claim("iat")
        .ok_or_else(|| Error::Authentication("Missing issued at claim".to_string()))?
        .as_str()
        .ok_or_else(|| Error::Authentication("Invalid issued at claim".to_string()))?;
    let iat = chrono::DateTime::parse_from_rfc3339(iat_str)
        .map_err(|_| Error::Authentication("Invalid issued at timestamp".to_string()))?
        .with_timezone(&chrono::Utc);

    let exp_str = claims
        .get_claim("exp")
        .ok_or_else(|| Error::Authentication("Missing expiration claim".to_string()))?
        .as_str()
        .ok_or_else(|| Error::Authentication("Invalid expiration claim".to_string()))?;
    let exp = chrono::DateTime::parse_from_rfc3339(exp_str)
        .map_err(|_| Error::Authentication("Invalid expiration timestamp".to_string()))?
        .with_timezone(&chrono::Utc);

    let jti_str = claims
        .get_claim("jti")
        .ok_or_else(|| Error::Authentication("Missing token ID claim".to_string()))?
        .as_str()
        .ok_or_else(|| Error::Authentication("Invalid token ID claim".to_string()))?;
    let jti = uuid::Uuid::parse_str(jti_str)
        .map_err(|_| Error::Authentication("Invalid token ID UUID".to_string()))?;

    // Optional claims
    let nbf = claims
        .get_claim("nbf")
        .and_then(|v| v.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc));

    let roles = claims
        .get_claim("roles")
        .and_then(|v: &serde_json::Value| serde_json::from_value::<Vec<String>>(v.clone()).ok());

    let scopes = claims
        .get_claim("scopes")
        .and_then(|v: &serde_json::Value| serde_json::from_value::<Vec<String>>(v.clone()).ok());

    let token_claims = TokenClaims {
        sub,
        dom,
        roles,
        scopes,
        iat,
        exp,
        nbf,
        jti,
    };

    // Validate that the token is not expired (double-check)
    if !token_claims.is_valid() {
        return Err(Error::Authentication(
            "Token is expired or not yet valid".to_string(),
        ));
    }

    // Check if token has been revoked
    if is_token_revoked(token_claims.jti, state).await? {
        return Err(Error::Authentication("Token has been revoked".to_string()));
    }

    Ok(token_claims)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    // Note: These tests require a valid AppState with config
    // In real tests, you'd create a test AppState or mock the config

    #[test]
    fn test_token_claims_expiration() {
        let claims = TokenClaims::new(
            Uuid::new_v4(),
            Uuid::new_v4(),
            None,
            None,
            3600, // 1 hour
        );

        assert!(!claims.is_expired());
        assert!(claims.is_valid());
    }
}
