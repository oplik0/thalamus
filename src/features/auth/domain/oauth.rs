//! OAuth domain types and traits
//!
//! This module wraps oauth2 crate types while providing domain-specific types
//! for the application.

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
    #[error("OAuth2 error: {0}")]
    OAuth2(String),
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

/// OAuth flow state for CSRF and PKCE protection
/// This wraps oauth2's CsrfToken and PkceCodeVerifier
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthFlowState {
    /// CSRF state token (secret)
    #[serde(skip_serializing)]
    pub csrf_token_secret: String,
    /// Public CSRF state token (sent to provider)
    pub csrf_token: String,
    /// PKCE verifier (secret)
    #[serde(skip_serializing)]
    pub pkce_verifier: String,
    /// PKCE challenge (sent to provider)
    pub pkce_challenge: String,
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
    pub token: OAuthTokenResponse,
    /// User info from provider
    pub user_info: OAuthUserInfo,
}

/// Wrapper for OAuth token response from oauth2 crate
#[derive(Debug, Clone)]
pub struct OAuthTokenResponse {
    /// Access token string
    pub access_token: String,
    /// Refresh token (optional)
    pub refresh_token: Option<String>,
    /// Token expiration time
    pub expires_at: Option<DateTime<Utc>>,
    /// Token type (usually "Bearer")
    pub token_type: String,
    /// Scopes granted
    pub scopes: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_oauth_flow_state_expiration() {
        let state = OAuthFlowState {
            csrf_token_secret: "secret".to_string(),
            csrf_token: "test_state".to_string(),
            pkce_verifier: "test_verifier".to_string(),
            pkce_challenge: "challenge".to_string(),
            provider_name: "github".to_string(),
            redirect_url: Some("http://localhost/callback".to_string()),
            expires_at: Utc::now() + chrono::Duration::minutes(10),
        };

        assert!(!state.is_expired());
    }

    #[test]
    fn test_oauth_flow_state_already_expired() {
        let state = OAuthFlowState {
            csrf_token_secret: "secret".to_string(),
            csrf_token: "test_state".to_string(),
            pkce_verifier: "test_verifier".to_string(),
            pkce_challenge: "challenge".to_string(),
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
