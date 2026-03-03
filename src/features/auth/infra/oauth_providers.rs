//! OAuth provider implementations (GitHub, GitHub Enterprise)

use async_trait::async_trait;
use chrono::Utc;
use serde::Deserialize;

use crate::features::auth::domain::oauth::{OAuthError, OAuthProvider, OAuthToken, OAuthUserInfo};

/// GitHub.com OAuth provider
#[derive(Debug)]
pub struct GitHubOAuthProvider {
    name: String,
    client_id: String,
    client_secret: String,
    scopes: Vec<String>,
}

impl GitHubOAuthProvider {
    pub fn new(
        name: String,
        client_id: String,
        client_secret: String,
        scopes: Vec<String>,
    ) -> Self {
        Self {
            name,
            client_id,
            client_secret,
            scopes,
        }
    }

    const AUTH_URL: &str = "https://github.com/login/oauth/authorize";
    const TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
    const USER_API: &str = "https://api.github.com/user";
    const ORGS_API: &str = "https://api.github.com/user/orgs";
}

#[async_trait]
impl OAuthProvider for GitHubOAuthProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn provider_type(&self) -> &str {
        "github"
    }

    fn get_authorization_url(
        &self,
        state: &str,
        pkce_challenge: &str,
        redirect_uri: &str,
    ) -> String {
        let scopes = self.scopes.join(" ");
        format!(
            "{}?client_id={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256&scope={}",
            Self::AUTH_URL,
            urlencoding::encode(&self.client_id),
            urlencoding::encode(redirect_uri),
            urlencoding::encode(state),
            urlencoding::encode(pkce_challenge),
            urlencoding::encode(&scopes)
        )
    }

    async fn exchange_code(
        &self,
        code: &str,
        pkce_verifier: &str,
        redirect_uri: &str,
    ) -> Result<OAuthToken, OAuthError> {
        // Implementation using reqwest
        let client = reqwest::Client::new();

        let params = [
            ("client_id", self.client_id.as_str()),
            ("client_secret", self.client_secret.as_str()),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", pkce_verifier),
        ];

        let response = client
            .post(Self::TOKEN_URL)
            .header("Accept", "application/json")
            .form(&params)
            .send()
            .await
            .map_err(|e| OAuthError::TokenExchange(e.to_string()))?;

        if !response.status().is_success() {
            return Err(OAuthError::TokenExchange(format!(
                "GitHub returned status: {}",
                response.status()
            )));
        }

        let token_response: GitHubTokenResponse = response
            .json()
            .await
            .map_err(|e| OAuthError::TokenExchange(e.to_string()))?;

        Ok(OAuthToken {
            access_token: token_response.access_token,
            refresh_token: token_response.refresh_token,
            expires_at: token_response
                .expires_in
                .map(|secs| Utc::now() + chrono::Duration::seconds(secs)),
        })
    }

    async fn get_user_info(&self, token: &OAuthToken) -> Result<OAuthUserInfo, OAuthError> {
        let client = reqwest::Client::new();

        let response = client
            .get(Self::USER_API)
            .header("Authorization", format!("Bearer {}", token.access_token))
            .header("Accept", "application/vnd.github.v3+json")
            .send()
            .await
            .map_err(|e| OAuthError::UserInfoFetch(e.to_string()))?;

        if !response.status().is_success() {
            return Err(OAuthError::UserInfoFetch(format!(
                "GitHub returned status: {}",
                response.status()
            )));
        }

        let user: GitHubUser = response
            .json()
            .await
            .map_err(|e| OAuthError::UserInfoFetch(e.to_string()))?;

        Ok(OAuthUserInfo {
            provider_user_id: user.id.to_string(),
            email: user.email.unwrap_or_default(),
            username: user.login,
            avatar_url: user.avatar_url,
            organizations: Vec::new(), // Will be populated separately
        })
    }

    async fn get_user_organizations(&self, token: &OAuthToken) -> Result<Vec<String>, OAuthError> {
        let client = reqwest::Client::new();

        let response = client
            .get(Self::ORGS_API)
            .header("Authorization", format!("Bearer {}", token.access_token))
            .header("Accept", "application/vnd.github.v3+json")
            .send()
            .await
            .map_err(|e| OAuthError::UserInfoFetch(e.to_string()))?;

        if !response.status().is_success() {
            return Err(OAuthError::UserInfoFetch(format!(
                "GitHub returned status: {}",
                response.status()
            )));
        }

        let orgs: Vec<GitHubOrg> = response
            .json()
            .await
            .map_err(|e| OAuthError::UserInfoFetch(e.to_string()))?;

        Ok(orgs.into_iter().map(|o| o.login).collect())
    }
}

/// GitHub Enterprise OAuth provider
#[derive(Debug)]
pub struct GitHubEnterpriseProvider {
    inner: GitHubOAuthProvider,
    base_url: String,
}

impl GitHubEnterpriseProvider {
    pub fn new(
        name: String,
        client_id: String,
        client_secret: String,
        scopes: Vec<String>,
        base_url: String,
    ) -> Self {
        Self {
            inner: GitHubOAuthProvider::new(name, client_id, client_secret, scopes),
            base_url,
        }
    }

    fn auth_url(&self) -> String {
        format!("{}/login/oauth/authorize", self.base_url)
    }

    fn token_url(&self) -> String {
        format!("{}/login/oauth/access_token", self.base_url)
    }

    fn user_api(&self) -> String {
        format!("{}/api/v3/user", self.base_url)
    }

    fn orgs_api(&self) -> String {
        format!("{}/api/v3/user/orgs", self.base_url)
    }
}

#[async_trait]
impl OAuthProvider for GitHubEnterpriseProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn provider_type(&self) -> &str {
        "github_enterprise"
    }

    fn get_authorization_url(
        &self,
        state: &str,
        pkce_challenge: &str,
        redirect_uri: &str,
    ) -> String {
        let scopes = self.inner.scopes.join(" ");
        format!(
            "{}?client_id={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256&scope={}",
            self.auth_url(),
            urlencoding::encode(&self.inner.client_id),
            urlencoding::encode(redirect_uri),
            urlencoding::encode(state),
            urlencoding::encode(pkce_challenge),
            urlencoding::encode(&scopes)
        )
    }

    async fn exchange_code(
        &self,
        code: &str,
        pkce_verifier: &str,
        redirect_uri: &str,
    ) -> Result<OAuthToken, OAuthError> {
        // Similar to GitHub but with different URLs
        let client = reqwest::Client::new();

        let params = [
            ("client_id", self.inner.client_id.as_str()),
            ("client_secret", self.inner.client_secret.as_str()),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", pkce_verifier),
        ];

        let response = client
            .post(&self.token_url())
            .header("Accept", "application/json")
            .form(&params)
            .send()
            .await
            .map_err(|e| OAuthError::TokenExchange(e.to_string()))?;

        if !response.status().is_success() {
            return Err(OAuthError::TokenExchange(format!(
                "GHE returned status: {}",
                response.status()
            )));
        }

        let token_response: GitHubTokenResponse = response
            .json()
            .await
            .map_err(|e| OAuthError::TokenExchange(e.to_string()))?;

        Ok(OAuthToken {
            access_token: token_response.access_token,
            refresh_token: token_response.refresh_token,
            expires_at: token_response
                .expires_in
                .map(|secs| Utc::now() + chrono::Duration::seconds(secs)),
        })
    }

    async fn get_user_info(&self, token: &OAuthToken) -> Result<OAuthUserInfo, OAuthError> {
        let client = reqwest::Client::new();

        let response = client
            .get(&self.user_api())
            .header("Authorization", format!("Bearer {}", token.access_token))
            .header("Accept", "application/vnd.github.v3+json")
            .send()
            .await
            .map_err(|e| OAuthError::UserInfoFetch(e.to_string()))?;

        if !response.status().is_success() {
            return Err(OAuthError::UserInfoFetch(format!(
                "GHE returned status: {}",
                response.status()
            )));
        }

        let user: GitHubUser = response
            .json()
            .await
            .map_err(|e| OAuthError::UserInfoFetch(e.to_string()))?;

        Ok(OAuthUserInfo {
            provider_user_id: user.id.to_string(),
            email: user.email.unwrap_or_default(),
            username: user.login,
            avatar_url: user.avatar_url,
            organizations: Vec::new(),
        })
    }

    async fn get_user_organizations(&self, token: &OAuthToken) -> Result<Vec<String>, OAuthError> {
        let client = reqwest::Client::new();

        let response = client
            .get(&self.orgs_api())
            .header("Authorization", format!("Bearer {}", token.access_token))
            .header("Accept", "application/vnd.github.v3+json")
            .send()
            .await
            .map_err(|e| OAuthError::UserInfoFetch(e.to_string()))?;

        if !response.status().is_success() {
            return Err(OAuthError::UserInfoFetch(format!(
                "GHE returned status: {}",
                response.status()
            )));
        }

        let orgs: Vec<GitHubOrg> = response
            .json()
            .await
            .map_err(|e| OAuthError::UserInfoFetch(e.to_string()))?;

        Ok(orgs.into_iter().map(|o| o.login).collect())
    }
}

// Response types for GitHub API
#[derive(Debug, Deserialize)]
struct GitHubTokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct GitHubUser {
    id: i64,
    login: String,
    email: Option<String>,
    avatar_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GitHubOrg {
    login: String,
}
