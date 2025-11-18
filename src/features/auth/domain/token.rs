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
    pub fn is_expired(&self) -> bool {
        Utc::now() > self.exp
    }

    /// Check if the token is valid (not expired, not before is satisfied)
    pub fn is_valid(&self) -> bool {
        let now = Utc::now();

        if now > self.exp {
            return false;
        }

        if let Some(nbf) = self.nbf {
            if now < nbf {
                return false;
            }
        }

        true
    }
}
