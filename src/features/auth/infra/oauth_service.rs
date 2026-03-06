//! OAuth service for handling OAuth flows and user provisioning
//!
//! This service uses oauth2 crate types and our concrete provider implementations.

use std::sync::Arc;
use uuid::Uuid;

use crate::bootstrap::AppState;
use crate::error::{Error, Result};
use crate::features::auth::domain::oauth::OAuthUserInfo;
use crate::features::auth::domain::token::TokenClaims;
use crate::features::auth::infra::oauth_providers::{
    GitHubEnterpriseProvider, GitHubOAuthProvider,
};
use crate::features::auth::infra::token_service::create_token;
use crate::features::auth::infra::{
    InMemoryOAuthStateStore, OAuthStateStore, create_oauth_flow_state,
};
use crate::shared::config::types::{OAuthProvider as ConfigProvider, OAuthProviderType};

/// OAuth service handling authentication flows
pub struct OAuthService {
    /// GitHub provider (if configured)
    github_provider: Option<Arc<GitHubOAuthProvider>>,
    /// GitHub Enterprise provider (if configured)
    github_enterprise_provider: Option<Arc<GitHubEnterpriseProvider>>,
    /// State store for CSRF/PKCE
    state_store: Arc<dyn OAuthStateStore>,
}

impl std::fmt::Debug for OAuthService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthService")
            .field("github_provider", &self.github_provider.is_some())
            .field(
                "github_enterprise_provider",
                &self.github_enterprise_provider.is_some(),
            )
            .field("state_store", &"<OAuthStateStore>")
            .finish()
    }
}

/// Response from initiating OAuth login
#[derive(Debug, Clone)]
pub struct OAuthInitiateResponse {
    pub authorization_url: String,
    pub state: String,
}

/// User and token information after successful OAuth
#[derive(Debug, Clone)]
pub struct OAuthAuthResponse {
    pub token: String,
    pub user_id: Uuid,
    pub team_id: Uuid,
    pub is_new_user: bool,
}

/// Provider info for listing
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProviderInfo {
    pub name: String,
    pub provider_type: String,
}

/// Enum to hold provider references
enum ProviderRef<'a> {
    GitHub(&'a GitHubOAuthProvider),
    GitHubEnterprise(&'a GitHubEnterpriseProvider),
}

impl OAuthService {
    /// Create a new OAuth service from configuration
    pub fn new(config_providers: &[ConfigProvider]) -> Result<Self> {
        let mut github_provider: Option<Arc<GitHubOAuthProvider>> = None;
        let mut github_enterprise_provider: Option<Arc<GitHubEnterpriseProvider>> = None;

        for provider_config in config_providers {
            match provider_config.provider_type {
                OAuthProviderType::GitHub => {
                    github_provider = Some(Arc::new(GitHubOAuthProvider::new(
                        provider_config.name.clone(),
                        provider_config.client_id.clone(),
                        provider_config.client_secret.clone(),
                        provider_config.scopes.clone(),
                    )));
                }
                OAuthProviderType::GitHubEnterprise => {
                    let base_url = provider_config.enterprise_url.clone().ok_or_else(|| {
                        Error::Config("enterprise_url required for GitHub Enterprise".to_string())
                    })?;
                    github_enterprise_provider = Some(Arc::new(GitHubEnterpriseProvider::new(
                        provider_config.name.clone(),
                        provider_config.client_id.clone(),
                        provider_config.client_secret.clone(),
                        provider_config.scopes.clone(),
                        base_url,
                    )));
                }
                OAuthProviderType::Oidc => {
                    return Err(Error::Config(
                        "OIDC provider not yet implemented".to_string(),
                    ));
                }
            }
        }

        Ok(Self {
            github_provider,
            github_enterprise_provider,
            state_store: Arc::new(InMemoryOAuthStateStore::new()),
        })
    }

    /// Get provider by name
    fn get_provider(&self, name: &str) -> Option<ProviderRef<'_>> {
        // Check GitHub provider
        if let Some(ref provider) = self.github_provider {
            if provider.name == name {
                return Some(ProviderRef::GitHub(provider.as_ref()));
            }
        }
        // Check GitHub Enterprise provider
        if let Some(ref provider) = self.github_enterprise_provider {
            if provider.name == name {
                return Some(ProviderRef::GitHubEnterprise(provider.as_ref()));
            }
        }
        None
    }

    /// Initiate OAuth login flow
    pub async fn initiate_oauth_login(
        &self,
        provider_name: &str,
        redirect_url: Option<String>,
        callback_base_url: &str,
    ) -> Result<OAuthInitiateResponse> {
        // Check provider exists
        let _ = self.get_provider(provider_name).ok_or_else(|| {
            Error::NotFound(format!("OAuth provider '{}' not found", provider_name))
        })?;

        // Create OAuth state with CSRF and PKCE
        let (state, state_token) = create_oauth_flow_state(
            provider_name.to_string(),
            redirect_url,
            10, // 10 minutes expiration
        );

        // Store state (clone to keep a copy for later)
        let state_clone = state.clone();
        self.state_store.store_state(state)?;

        // Build redirect URI
        let redirect_uri = format!(
            "{}/v1/auth/oauth/{}/callback",
            callback_base_url, provider_name
        );

        // Get provider for generating URL
        let provider = self.get_provider(provider_name).ok_or_else(|| {
            Error::NotFound(format!("OAuth provider '{}' not found", provider_name))
        })?;

        // Generate authorization URL using the provider
        let auth_url = match provider {
            ProviderRef::GitHub(p) => p.get_authorization_url(
                &state_clone.csrf_token,
                &state_clone.pkce_challenge,
                &redirect_uri,
            ),
            ProviderRef::GitHubEnterprise(p) => p.get_authorization_url(
                &state_clone.csrf_token,
                &state_clone.pkce_challenge,
                &redirect_uri,
            ),
        };

        Ok(OAuthInitiateResponse {
            authorization_url: auth_url,
            state: state_token,
        })
    }

    /// Handle OAuth callback
    pub async fn handle_oauth_callback(
        &self,
        state_token: &str,
        code: &str,
        callback_base_url: &str,
        state: &AppState,
    ) -> Result<OAuthAuthResponse> {
        // Verify state
        let oauth_state = self
            .state_store
            .get_state(state_token)?
            .ok_or_else(|| Error::Authentication("Invalid or expired state".to_string()))?;

        if oauth_state.is_expired() {
            return Err(Error::Authentication("OAuth state expired".to_string()));
        }

        // Remove state (one-time use)
        self.state_store.remove_state(state_token)?;

        // Get provider
        let provider = self
            .get_provider(&oauth_state.provider_name)
            .ok_or_else(|| {
                Error::NotFound(format!(
                    "OAuth provider '{}' not found",
                    oauth_state.provider_name
                ))
            })?;

        // Build redirect URI
        let redirect_uri = format!(
            "{}/v1/auth/oauth/{}/callback",
            callback_base_url, oauth_state.provider_name
        );

        // Exchange code for token
        let token = match provider {
            ProviderRef::GitHub(p) => p
                .exchange_code(code, &oauth_state.pkce_verifier, &redirect_uri)
                .await
                .map_err(|e| {
                    Error::Authentication(format!("OAuth token exchange failed: {}", e))
                })?,
            ProviderRef::GitHubEnterprise(p) => p
                .exchange_code(code, &oauth_state.pkce_verifier, &redirect_uri)
                .await
                .map_err(|e| {
                    Error::Authentication(format!("OAuth token exchange failed: {}", e))
                })?,
        };

        // Get user info
        let user_info = match provider {
            ProviderRef::GitHub(p) => p
                .get_user_info(&token.access_token)
                .await
                .map_err(|e| Error::Authentication(format!("Failed to get user info: {}", e)))?,
            ProviderRef::GitHubEnterprise(p) => p
                .get_user_info(&token.access_token)
                .await
                .map_err(|e| Error::Authentication(format!("Failed to get user info: {}", e)))?,
        };

        // Get organizations for team mapping
        let orgs = match provider {
            ProviderRef::GitHub(p) => p
                .get_user_organizations(&token.access_token)
                .await
                .unwrap_or_default(),
            ProviderRef::GitHubEnterprise(p) => p
                .get_user_organizations(&token.access_token)
                .await
                .unwrap_or_default(),
        };

        // Provision user
        let (user_id, team_id, is_new_user) = self
            .provision_user(&oauth_state.provider_name, &user_info, &orgs, state)
            .await?;

        // Create PASETO token
        let claims = TokenClaims::new(
            user_id,
            team_id,
            None,      // Roles can be fetched from Casbin
            None,      // Scopes
            3600 * 24, // 24 hours
        );

        let paseto_token = create_token(&claims, state)?;

        Ok(OAuthAuthResponse {
            token: paseto_token,
            user_id,
            team_id,
            is_new_user,
        })
    }

    /// Provision or update user from OAuth info
    async fn provision_user(
        &self,
        _provider_name: &str,
        user_info: &OAuthUserInfo,
        _orgs: &[String],
        state: &AppState,
    ) -> Result<(Uuid, Uuid, bool)> {
        // Check if user already exists by email
        let existing_user = sqlx::query!("SELECT id FROM users WHERE email = $1", user_info.email)
            .fetch_optional(&state.db_pool)
            .await?;

        if let Some(user) = existing_user {
            // Get user's team from team_memberships
            let team_id = match sqlx::query_scalar!(
                "SELECT team_id FROM team_memberships WHERE user_id = $1 LIMIT 1",
                user.id
            )
            .fetch_optional(&state.db_pool)
            .await?
            {
                Some(id) => id,
                None => {
                    // User exists but has no team - create one
                    let new_team_id = Uuid::new_v4();
                    sqlx::query!(
                        "INSERT INTO teams (id, name, description) VALUES ($1, $2, $3)",
                        new_team_id,
                        format!("team-{}", user.id),
                        "Auto-created team for existing user"
                    )
                    .execute(&state.db_pool)
                    .await?;
                    sqlx::query!(
                        "INSERT INTO team_memberships (user_id, team_id, role) VALUES ($1, $2, 'admin')",
                        user.id,
                        new_team_id
                    )
                    .execute(&state.db_pool)
                    .await?;
                    new_team_id
                }
            };

            // Update last login
            sqlx::query!(
                "UPDATE users SET last_login_at = NOW() WHERE id = $1",
                user.id
            )
            .execute(&state.db_pool)
            .await?;

            return Ok((user.id, team_id, false));
        }

        // Create new user
        let user_id = Uuid::new_v4();
        let team_id = Uuid::new_v4();

        // Create team first
        sqlx::query!(
            "INSERT INTO teams (id, name, description) VALUES ($1, $2, $3)",
            team_id,
            format!("team-{}", user_info.username),
            format!("Auto-created team for {}", user_info.username)
        )
        .execute(&state.db_pool)
        .await?;

        // Create user
        sqlx::query!(
            r#"
            INSERT INTO users (id, username, email, is_active, last_login_at)
            VALUES ($1, $2, $3, true, NOW())
            "#,
            user_id,
            user_info.username,
            user_info.email
        )
        .execute(&state.db_pool)
        .await?;

        // Add user to team
        sqlx::query!(
            "INSERT INTO team_memberships (user_id, team_id, role) VALUES ($1, $2, 'admin')",
            user_id,
            team_id
        )
        .execute(&state.db_pool)
        .await?;

        // Note: OAuth identity tracking would require the oauth_identities table
        // which is created by the migration. For now, we just create the user.

        Ok((user_id, team_id, true))
    }

    /// List configured OAuth providers (public info only)
    pub fn list_providers(&self) -> Vec<ProviderInfo> {
        let mut providers = Vec::new();

        if let Some(ref p) = self.github_provider {
            providers.push(ProviderInfo {
                name: p.name.clone(),
                provider_type: "github".to_string(),
            });
        }

        if let Some(ref p) = self.github_enterprise_provider {
            providers.push(ProviderInfo {
                name: p.name.clone(),
                provider_type: "github_enterprise".to_string(),
            });
        }

        providers
    }
}
