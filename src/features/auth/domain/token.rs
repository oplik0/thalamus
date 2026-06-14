use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// PASETO token claims
///
/// These claims integrate with Casbin for authorization:
/// - `sub` (subject) is the user ID
/// - `dom` (domain) is the team ID for Casbin's domain-based RBAC
/// - `roles` can be cached for performance, but should be validated against Casbin
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenClaims {
    /// Subject - User ID
    pub sub: Uuid,

    /// Domain - Team ID (for Casbin domain-based RBAC)
    pub dom: Uuid,

    /// Optional cached roles for this user in this team
    /// Should be validated against Casbin for critical operations
    #[serde(skip_serializing_if = "Option::is_none")]
    pub roles: Option<Vec<String>>,

    /// Optional scopes (similar to API keys)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scopes: Option<Vec<String>>,

    /// Issued at timestamp
    pub iat: DateTime<Utc>,

    /// Expiration timestamp
    pub exp: DateTime<Utc>,

    /// Not before timestamp (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nbf: Option<DateTime<Utc>>,

    /// Token ID (for revocation)
    pub jti: Uuid,
}

impl TokenClaims {
    /// Create new token claims
    #[must_use]
    pub fn new(
        user_id: Uuid,
        team_id: Uuid,
        roles: Option<Vec<String>>,
        scopes: Option<Vec<String>>,
        expires_in_seconds: i64,
    ) -> Self {
        let now = Utc::now();
        Self {
            sub: user_id,
            dom: team_id,
            roles,
            scopes,
            iat: now,
            exp: now + chrono::Duration::seconds(expires_in_seconds),
            nbf: None,
            jti: Uuid::new_v4(),
        }
    }

    /// Check if the token is expired
    #[must_use]
    pub fn is_expired(&self) -> bool {
        Utc::now() > self.exp
    }

    /// Check if the token is valid (not expired, not before is satisfied)
    #[must_use]
    pub fn is_valid(&self) -> bool {
        let now = Utc::now();

        if now > self.exp {
            return false;
        }

        if let Some(nbf) = self.nbf
            && now < nbf
        {
            return false;
        }

        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn test_token_claims_new() {
        let user_id = Uuid::new_v4();
        let team_id = Uuid::new_v4();
        let roles = vec!["admin".to_string(), "user".to_string()];
        let scopes = vec!["read".to_string(), "write".to_string()];

        let claims = TokenClaims::new(
            user_id,
            team_id,
            Some(roles.clone()),
            Some(scopes.clone()),
            3600,
        );

        assert_eq!(claims.sub, user_id);
        assert_eq!(claims.dom, team_id);
        assert_eq!(claims.roles, Some(roles));
        assert_eq!(claims.scopes, Some(scopes));
        assert!(!claims.is_expired());
        assert!(claims.is_valid());
        assert!(claims.nbf.is_none());
    }

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

    #[test]
    fn test_token_claims_already_expired() {
        // Create claims that expired 1 hour ago
        let claims = TokenClaims::new(
            Uuid::new_v4(),
            Uuid::new_v4(),
            None,
            None,
            -3600, // Negative = already expired
        );

        assert!(claims.is_expired());
        assert!(!claims.is_valid());
    }

    #[test]
    fn test_token_claims_not_yet_valid() {
        let user_id = Uuid::new_v4();
        let team_id = Uuid::new_v4();
        let now = Utc::now();

        let claims = TokenClaims {
            sub: user_id,
            dom: team_id,
            roles: None,
            scopes: None,
            iat: now,
            exp: now + chrono::Duration::hours(1),
            nbf: Some(now + chrono::Duration::minutes(5)), // Not valid for 5 minutes
            jti: Uuid::new_v4(),
        };

        assert!(!claims.is_expired());
        assert!(!claims.is_valid()); // Not yet valid due to nbf
    }

    #[test]
    fn test_token_claims_with_past_nbf() {
        let user_id = Uuid::new_v4();
        let team_id = Uuid::new_v4();
        let now = Utc::now();

        let claims = TokenClaims {
            sub: user_id,
            dom: team_id,
            roles: None,
            scopes: None,
            iat: now - chrono::Duration::hours(1),
            exp: now + chrono::Duration::hours(1),
            nbf: Some(now - chrono::Duration::minutes(5)), // Already valid
            jti: Uuid::new_v4(),
        };

        assert!(!claims.is_expired());
        assert!(claims.is_valid());
    }

    #[test]
    fn test_token_claims_exactly_at_expiration() {
        // This test is timing-sensitive, so we use a very short duration
        let claims = TokenClaims::new(
            Uuid::new_v4(),
            Uuid::new_v4(),
            None,
            None,
            0, // Expires immediately
        );

        // Should be expired or very close to it
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(claims.is_expired());
    }

    #[test]
    fn test_token_claims_serialization() {
        let user_id = Uuid::new_v4();
        let team_id = Uuid::new_v4();
        let roles = vec!["admin".to_string()];
        let scopes = vec!["read".to_string()];

        let claims = TokenClaims::new(user_id, team_id, Some(roles), Some(scopes), 3600);

        let json = serde_json::to_string(&claims).expect("Should serialize");
        assert!(json.contains(&user_id.to_string()));
        assert!(json.contains(&team_id.to_string()));
        assert!(json.contains("admin"));
        assert!(json.contains("read"));

        let deserialized: TokenClaims = serde_json::from_str(&json).expect("Should deserialize");
        assert_eq!(deserialized.sub, claims.sub);
        assert_eq!(deserialized.dom, claims.dom);
        assert_eq!(deserialized.roles, claims.roles);
        assert_eq!(deserialized.scopes, claims.scopes);
        assert_eq!(deserialized.jti, claims.jti);
    }

    #[test]
    fn test_token_claims_without_optional_fields() {
        let user_id = Uuid::new_v4();
        let team_id = Uuid::new_v4();

        let claims = TokenClaims::new(
            user_id, team_id, None, // No roles
            None, // No scopes
            3600,
        );

        let json = serde_json::to_string(&claims).expect("Should serialize");
        // Roles and scopes should be skipped in serialization when None
        assert!(!json.contains("roles"));
        assert!(!json.contains("scopes"));

        let deserialized: TokenClaims = serde_json::from_str(&json).expect("Should deserialize");
        assert!(deserialized.roles.is_none());
        assert!(deserialized.scopes.is_none());
    }
}
