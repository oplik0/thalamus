//! OAuth domain types and traits

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;
use uuid::Uuid;

/// Errors that can occur during OAuth operations
#[derive(Debug, Error)]
pub enum OAuthError {
    #[error("Provider error: {0}")]
    Provider(String),
    #[error("Token exchange failed: {0}")]
    TokenExchange(String),
    #[error("User info fetch failed: {0}")]
    UserInfoFetch(String),
    #[error("Invalid state parameter")]
    InvalidState,
    #[error("Provider not found: {0}")]
    ProviderNotFound(String),
    #[error("User not found")]
    UserNotFound,
    #[error("Team mapping failed: {0}")]
    TeamMapping(String),
}

/// User information from OAuth provider
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthUserInfo {
    /// Provider's user ID
    pub provider_user_id: String,
    /// User's email
    pub email: String,
    /// User's username/login
    pub username: String,
    /// Optional avatar URL
    pub avatar_url: Option<String>,
    /// Organization memberships (for org-based team mapping)
    pub organizations: Vec<String>,
}

/// OAuth token pair
#[derive(Debug, Clone)]
pub struct OAuthToken {
    /// Access token
    pub access_token: String,
    /// Refresh token (optional)
    pub refresh_token: Option<String>,
    /// Token expiration time
    pub expires_at: Option<DateTime<Utc>>,
}

/// Strategy for mapping OAuth users to teams
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamMappingStrategy {
    /// Create a personal team for each OAuth user
    AutoCreate { team_prefix: String },
    /// All users join a fixed team
    FixedTeam { team_id: Uuid },
    /// Map provider orgs to teams
    OrgBased { mappings: HashMap<String, Uuid> },
}

/// OAuth flow state for CSRF protection
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthFlowState {
    /// State token for CSRF protection
    pub state_token: String,
    /// PKCE verifier
    pub pkce_verifier: String,
    /// Provider name
    pub provider_name: String,
    /// Optional redirect URL after login
    pub redirect_url: Option<String>,
    /// Expiration time
    pub expires_at: DateTime<Utc>,
}

impl OAuthFlowState {
    /// Check if the state has expired
    pub fn is_expired(&self) -> bool {
        Utc::now() > self.expires_at
    }
}

/// Result of a successful OAuth callback
#[derive(Debug, Clone)]
pub struct OAuthResult {
    /// The authenticated user ID
    pub user_id: Uuid,
    /// The user's team ID
    pub team_id: Uuid,
    /// Whether this is a new user
    pub is_new_user: bool,
    /// The OAuth token (for storage)
    pub token: OAuthToken,
    /// User info from provider
    pub user_info: OAuthUserInfo,
}

/// OAuth provider trait - implement for each provider (GitHub, GHE, OIDC, etc.)
#[async_trait]
pub trait OAuthProvider: Send + Sync {
    /// Get the provider name
    fn name(&self) -> &str;

    /// Get the provider type
    fn provider_type(&self) -> &str;

    /// Generate the authorization URL
    fn get_authorization_url(
        &self,
        state: &str,
        pkce_challenge: &str,
        redirect_uri: &str,
    ) -> String;

    /// Exchange authorization code for tokens
    async fn exchange_code(
        &self,
        code: &str,
        pkce_verifier: &str,
        redirect_uri: &str,
    ) -> Result<OAuthToken, OAuthError>;

    /// Fetch user information using access token
    async fn get_user_info(&self, token: &OAuthToken) -> Result<OAuthUserInfo, OAuthError>;

    /// Fetch user's organization memberships
    async fn get_user_organizations(&self, token: &OAuthToken) -> Result<Vec<String>, OAuthError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_oauth_flow_state_expiration() {
        let state = OAuthFlowState {
            state_token: "test_state".to_string(),
            pkce_verifier: "test_verifier".to_string(),
            provider_name: "github".to_string(),
            redirect_url: Some("http://localhost/callback".to_string()),
            expires_at: Utc::now() + chrono::Duration::minutes(10),
        };

        assert!(!state.is_expired());
    }

    #[test]
    fn test_oauth_flow_state_already_expired() {
        let state = OAuthFlowState {
            state_token: "test_state".to_string(),
            pkce_verifier: "test_verifier".to_string(),
            provider_name: "github".to_string(),
            redirect_url: None,
            expires_at: Utc::now() - chrono::Duration::minutes(1),
        };

        assert!(state.is_expired());
    }

    #[test]
    fn test_oauth_error_display() {
        let err = OAuthError::InvalidState;
        assert_eq!(err.to_string(), "Invalid state parameter");

        let err = OAuthError::ProviderNotFound("github".to_string());
        assert_eq!(err.to_string(), "Provider not found: github");

        let err = OAuthError::UserNotFound;
        assert_eq!(err.to_string(), "User not found");
    }

    #[test]
    fn test_team_mapping_strategy_serialization() {
        let auto_create = TeamMappingStrategy::AutoCreate {
            team_prefix: "personal_".to_string(),
        };
        let json = serde_json::to_string(&auto_create).unwrap();
        assert!(json.contains("auto_create"));
        assert!(json.contains("personal_"));

        let fixed = TeamMappingStrategy::FixedTeam {
            team_id: Uuid::new_v4(),
        };
        let json = serde_json::to_string(&fixed).unwrap();
        assert!(json.contains("fixed_team"));

        let mut mappings = HashMap::new();
        mappings.insert("org1".to_string(), Uuid::new_v4());
        let org_based = TeamMappingStrategy::OrgBased { mappings };
        let json = serde_json::to_string(&org_based).unwrap();
        assert!(json.contains("org_based"));
    }

    #[test]
    fn test_oauth_user_info_serialization() {
        let user_info = OAuthUserInfo {
            provider_user_id: "12345".to_string(),
            email: "test@example.com".to_string(),
            username: "testuser".to_string(),
            avatar_url: Some("https://example.com/avatar.png".to_string()),
            organizations: vec!["org1".to_string(), "org2".to_string()],
        };

        let json = serde_json::to_string(&user_info).expect("Should serialize");
        let deserialized: OAuthUserInfo = serde_json::from_str(&json).expect("Should deserialize");

        assert_eq!(deserialized.provider_user_id, user_info.provider_user_id);
        assert_eq!(deserialized.email, user_info.email);
        assert_eq!(deserialized.username, user_info.username);
        assert_eq!(deserialized.avatar_url, user_info.avatar_url);
        assert_eq!(deserialized.organizations, user_info.organizations);
    }
}
