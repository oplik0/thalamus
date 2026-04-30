//! OAuth provider implementations using the oauth2 crate
//!
//! This module provides OAuth2 client implementations using the oauth2 crate
//! for GitHub, GitHub Enterprise, and OIDC providers.

use std::sync::OnceLock;

use oauth2::{AuthUrl, TokenUrl};
use reqwest::Client;
use serde::Deserialize;

use crate::error::Error;
use crate::features::auth::domain::oauth::{OAuthError, OAuthTokenResponse, OAuthUserInfo};

/// OIDC discovery document
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct OidcDiscoveryDocument {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    userinfo_endpoint: String,
    jwks_uri: Option<String>,
}

/// OIDC token response (includes id_token)
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct OidcTokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: Option<i64>,
    token_type: String,
    scope: Option<String>,
}

/// OIDC UserInfo response
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct OidcUserInfo {
    sub: String,
    email: Option<String>,
    name: Option<String>,
    given_name: Option<String>,
    family_name: Option<String>,
    picture: Option<String>,
    email_verified: Option<bool>,
}

/// Common interface for all OAuth provider implementations.
///
/// Implemented for each concrete provider type and for [`ProviderRef`] so that
/// callers can dispatch through the enum without repeating match arms.
#[allow(async_fn_in_trait)]
pub trait OAuthProviderOps {
    async fn get_authorization_url(
        &self,
        csrf_state: &str,
        pkce_challenge: &str,
        redirect_uri: &str,
    ) -> String;

    async fn exchange_code(
        &self,
        code: &str,
        pkce_verifier: &str,
        redirect_uri: &str,
    ) -> Result<OAuthTokenResponse, OAuthError>;

    async fn get_user_info(&self, access_token: &str) -> Result<OAuthUserInfo, OAuthError>;

    async fn get_user_organizations(&self, access_token: &str) -> Result<Vec<String>, OAuthError>;
}

/// GitHub.com OAuth provider using oauth2 crate
#[derive(Clone)]
pub struct GitHubOAuthProvider {
    /// Provider name for identification
    pub name: String,
    /// Client ID
    client_id: String,
    /// Client secret
    client_secret: String,
    /// Scopes to request
    scopes: Vec<String>,
    /// Authorization URL
    auth_url: AuthUrl,
    /// Token URL
    token_url: TokenUrl,
    /// Base URL for API calls
    api_base_url: String,
}

impl std::fmt::Debug for GitHubOAuthProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubOAuthProvider")
            .field("name", &self.name)
            .field("scopes", &self.scopes)
            .field("api_base_url", &self.api_base_url)
            .finish()
    }
}

impl GitHubOAuthProvider {
    pub fn new(
        name: String,
        client_id: String,
        client_secret: String,
        scopes: Vec<String>,
    ) -> crate::error::Result<Self> {
        Ok(Self {
            name,
            client_id,
            client_secret,
            scopes,
            auth_url: AuthUrl::new("https://github.com/login/oauth/authorize".to_string())
                .map_err(|e| Error::Config(format!("Invalid GitHub auth URL: {}", e)))?,
            token_url: TokenUrl::new("https://github.com/login/oauth/access_token".to_string())
                .map_err(|e| Error::Config(format!("Invalid GitHub token URL: {}", e)))?,
            api_base_url: "https://api.github.com".to_string(),
        })
    }

    /// Generate the authorization URL with PKCE
    pub async fn get_authorization_url(
        &self,
        csrf_state: &str,
        pkce_challenge: &str,
        redirect_uri: &str,
    ) -> String {
        let scopes = self.scopes.join(" ");
        format!(
            "{}?client_id={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256&scope={}",
            self.auth_url,
            urlencoding::encode(&self.client_id),
            urlencoding::encode(redirect_uri),
            urlencoding::encode(csrf_state),
            urlencoding::encode(pkce_challenge),
            urlencoding::encode(&scopes)
        )
    }

    /// Exchange authorization code for tokens
    pub async fn exchange_code(
        &self,
        code: &str,
        pkce_verifier: &str,
        redirect_uri: &str,
    ) -> Result<OAuthTokenResponse, OAuthError> {
        let client = Client::new();

        let params = [
            ("client_id", self.client_id.as_str()),
            ("client_secret", self.client_secret.as_str()),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", pkce_verifier),
        ];

        let response = client
            .post(self.token_url.to_string())
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

        Ok(OAuthTokenResponse {
            access_token: token_response.access_token,
            refresh_token: token_response.refresh_token,
            expires_at: token_response
                .expires_in
                .map(|secs| chrono::Utc::now() + chrono::Duration::seconds(secs)),
            token_type: "Bearer".to_string(),
            scopes: self.scopes.clone(),
        })
    }

    /// Fetch user information using access token
    pub async fn get_user_info(&self, access_token: &str) -> Result<OAuthUserInfo, OAuthError> {
        let client = Client::new();

        let response = client
            .get(format!("{}/user", self.api_base_url))
            .header("Authorization", format!("Bearer {}", access_token))
            .header("Accept", "application/vnd.github.v3+json")
            .header("User-Agent", "Thalamus-OAuth-App")
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

    /// Fetch user's organization memberships
    pub async fn get_user_organizations(
        &self,
        access_token: &str,
    ) -> Result<Vec<String>, OAuthError> {
        let client = Client::new();

        let response = client
            .get(format!("{}/user/orgs", self.api_base_url))
            .header("Authorization", format!("Bearer {}", access_token))
            .header("Accept", "application/vnd.github.v3+json")
            .header("User-Agent", "Thalamus-OAuth-App")
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
#[derive(Clone)]
pub struct GitHubEnterpriseProvider {
    /// Provider name
    pub name: String,
    /// Client ID
    client_id: String,
    /// Client secret
    client_secret: String,
    /// Scopes to request
    scopes: Vec<String>,
    /// Authorization URL
    auth_url: AuthUrl,
    /// Token URL
    token_url: TokenUrl,
    /// Base URL for GitHub Enterprise
    base_url: String,
    /// API base URL
    api_base_url: String,
}

impl std::fmt::Debug for GitHubEnterpriseProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubEnterpriseProvider")
            .field("name", &self.name)
            .field("base_url", &self.base_url)
            .field("scopes", &self.scopes)
            .finish()
    }
}

impl GitHubEnterpriseProvider {
    pub fn new(
        name: String,
        client_id: String,
        client_secret: String,
        scopes: Vec<String>,
        base_url: String,
    ) -> crate::error::Result<Self> {
        let auth_url = format!("{}/login/oauth/authorize", base_url);
        let token_url = format!("{}/login/oauth/access_token", base_url);

        Ok(Self {
            name,
            client_id,
            client_secret,
            scopes,
            auth_url: AuthUrl::new(auth_url)
                .map_err(|e| Error::Config(format!("Invalid GitHub Enterprise auth URL: {}", e)))?,
            token_url: TokenUrl::new(token_url).map_err(|e| {
                Error::Config(format!("Invalid GitHub Enterprise token URL: {}", e))
            })?,
            base_url: base_url.clone(),
            api_base_url: format!("{}/api/v3", base_url),
        })
    }

    /// Generate the authorization URL with PKCE
    pub async fn get_authorization_url(
        &self,
        csrf_state: &str,
        pkce_challenge: &str,
        redirect_uri: &str,
    ) -> String {
        let scopes = self.scopes.join(" ");
        format!(
            "{}?client_id={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256&scope={}",
            self.auth_url,
            urlencoding::encode(&self.client_id),
            urlencoding::encode(redirect_uri),
            urlencoding::encode(csrf_state),
            urlencoding::encode(pkce_challenge),
            urlencoding::encode(&scopes)
        )
    }

    /// Exchange authorization code for tokens
    pub async fn exchange_code(
        &self,
        code: &str,
        pkce_verifier: &str,
        redirect_uri: &str,
    ) -> Result<OAuthTokenResponse, OAuthError> {
        let client = Client::new();

        let params = [
            ("client_id", self.client_id.as_str()),
            ("client_secret", self.client_secret.as_str()),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", pkce_verifier),
        ];

        let response = client
            .post(self.token_url.to_string())
            .header("Accept", "application/json")
            .form(&params)
            .send()
            .await
            .map_err(|e| OAuthError::TokenExchange(e.to_string()))?;

        if !response.status().is_success() {
            return Err(OAuthError::TokenExchange(format!(
                "GitHub Enterprise returned status: {}",
                response.status()
            )));
        }

        let token_response: GitHubTokenResponse = response
            .json()
            .await
            .map_err(|e| OAuthError::TokenExchange(e.to_string()))?;

        Ok(OAuthTokenResponse {
            access_token: token_response.access_token,
            refresh_token: token_response.refresh_token,
            expires_at: token_response
                .expires_in
                .map(|secs| chrono::Utc::now() + chrono::Duration::seconds(secs)),
            token_type: "Bearer".to_string(),
            scopes: self.scopes.clone(),
        })
    }

    /// Fetch user information using access token
    pub async fn get_user_info(&self, access_token: &str) -> Result<OAuthUserInfo, OAuthError> {
        let client = Client::new();

        let response = client
            .get(format!("{}/user", self.api_base_url))
            .header("Authorization", format!("Bearer {}", access_token))
            .header("Accept", "application/vnd.github.v3+json")
            .header("User-Agent", "Thalamus-OAuth-App")
            .send()
            .await
            .map_err(|e| OAuthError::UserInfoFetch(e.to_string()))?;

        if !response.status().is_success() {
            return Err(OAuthError::UserInfoFetch(format!(
                "GitHub Enterprise returned status: {}",
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

    /// Fetch user's organization memberships
    pub async fn get_user_organizations(
        &self,
        access_token: &str,
    ) -> Result<Vec<String>, OAuthError> {
        let client = Client::new();

        let response = client
            .get(format!("{}/user/orgs", self.api_base_url))
            .header("Authorization", format!("Bearer {}", access_token))
            .header("Accept", "application/vnd.github.v3+json")
            .header("User-Agent", "Thalamus-OAuth-App")
            .send()
            .await
            .map_err(|e| OAuthError::UserInfoFetch(e.to_string()))?;

        if !response.status().is_success() {
            return Err(OAuthError::UserInfoFetch(format!(
                "GitHub Enterprise returned status: {}",
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

/// OIDC provider using OpenID Connect discovery and userinfo endpoint
#[derive(Clone)]
pub struct OidcProvider {
    pub name: String,
    client_id: String,
    client_secret: String,
    scopes: Vec<String>,
    issuer_url: String,
    redirect_uri: Option<String>,
    authorization_endpoint: Option<String>,
    token_endpoint: Option<String>,
    userinfo_endpoint: Option<String>,
    discovered_endpoints: OnceLock<(String, String, String)>,
}

impl std::fmt::Debug for OidcProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcProvider")
            .field("name", &self.name)
            .field("issuer_url", &self.issuer_url)
            .finish()
    }
}

impl OidcProvider {
    pub fn new(
        name: String,
        client_id: String,
        client_secret: String,
        scopes: Vec<String>,
        issuer_url: String,
        redirect_uri: Option<String>,
        authorization_endpoint: Option<String>,
        token_endpoint: Option<String>,
        userinfo_endpoint: Option<String>,
    ) -> crate::error::Result<Self> {
        Ok(Self {
            name,
            client_id,
            client_secret,
            scopes,
            issuer_url,
            redirect_uri,
            authorization_endpoint,
            token_endpoint,
            userinfo_endpoint,
            discovered_endpoints: OnceLock::new(),
        })
    }

    async fn fetch_discovery(issuer_url: &str) -> crate::error::Result<OidcDiscoveryDocument> {
        let client = Client::new();
        let discovery_url = format!(
            "{}/.well-known/openid-configuration",
            issuer_url.trim_end_matches('/')
        );

        let response = client
            .get(&discovery_url)
            .send()
            .await
            .map_err(|e| Error::Config(format!("OIDC discovery failed: {}", e)))?
            .error_for_status()
            .map_err(|e| Error::Config(format!("OIDC discovery returned error: {}", e)))?;

        response
            .json()
            .await
            .map_err(|e| Error::Config(format!("Failed to parse OIDC discovery: {}", e)))
    }

    async fn ensure_discovered(&self) -> crate::error::Result<&(String, String, String)> {
        if let Some(endpoints) = self.discovered_endpoints.get() {
            return Ok(endpoints);
        }

        let (auth, token, userinfo) = if let (Some(a), Some(t), Some(u)) = (
            &self.authorization_endpoint,
            &self.token_endpoint,
            &self.userinfo_endpoint,
        ) {
            (a.clone(), t.clone(), u.clone())
        } else {
            let doc = Self::fetch_discovery(&self.issuer_url).await?;
            (
                self.authorization_endpoint
                    .clone()
                    .unwrap_or(doc.authorization_endpoint),
                self.token_endpoint.clone().unwrap_or(doc.token_endpoint),
                self.userinfo_endpoint
                    .clone()
                    .unwrap_or(doc.userinfo_endpoint),
            )
        };

        Ok(self
            .discovered_endpoints
            .get_or_init(|| (auth, token, userinfo)))
    }

    pub async fn get_authorization_url(
        &self,
        csrf_state: &str,
        pkce_challenge: &str,
        redirect_uri: &str,
    ) -> String {
        let auth_url = self
            .ensure_discovered()
            .await
            .map(|e| e.0.clone())
            .unwrap_or_else(|_| "https://error.invalid".to_string());
        let final_redirect_uri = self.redirect_uri.as_deref().unwrap_or(redirect_uri);
        let scopes = self.scopes.join(" ");
        format!(
            "{}?client_id={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256&scope={}&response_type=code",
            auth_url,
            urlencoding::encode(&self.client_id),
            urlencoding::encode(final_redirect_uri),
            urlencoding::encode(csrf_state),
            urlencoding::encode(pkce_challenge),
            urlencoding::encode(&scopes)
        )
    }

    pub async fn exchange_code(
        &self,
        code: &str,
        pkce_verifier: &str,
        redirect_uri: &str,
    ) -> Result<OAuthTokenResponse, OAuthError> {
        let token_url = self
            .ensure_discovered()
            .await
            .map_err(|e| OAuthError::TokenExchange(format!("OIDC discovery failed: {}", e)))?
            .1
            .clone();

        let client = Client::new();

        let params = [
            ("client_id", self.client_id.as_str()),
            ("client_secret", self.client_secret.as_str()),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", pkce_verifier),
            ("grant_type", "authorization_code"),
        ];

        let response = client
            .post(&token_url)
            .header("Accept", "application/json")
            .form(&params)
            .send()
            .await
            .map_err(|e| OAuthError::TokenExchange(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(OAuthError::TokenExchange(format!(
                "OIDC token endpoint returned {}: {}",
                status, body
            )));
        }

        let token_response: OidcTokenResponse = response
            .json()
            .await
            .map_err(|e| OAuthError::TokenExchange(e.to_string()))?;

        Ok(OAuthTokenResponse {
            access_token: token_response.access_token,
            refresh_token: token_response.refresh_token,
            expires_at: token_response
                .expires_in
                .map(|secs| chrono::Utc::now() + chrono::Duration::seconds(secs)),
            token_type: token_response.token_type,
            scopes: self.scopes.clone(),
        })
    }

    pub async fn get_user_info(&self, access_token: &str) -> Result<OAuthUserInfo, OAuthError> {
        let userinfo_url = self
            .ensure_discovered()
            .await
            .map_err(|e| OAuthError::UserInfoFetch(format!("OIDC discovery failed: {}", e)))?
            .2
            .clone();

        let client = Client::new();

        let response = client
            .get(&userinfo_url)
            .header("Authorization", format!("Bearer {}", access_token))
            .send()
            .await
            .map_err(|e| OAuthError::UserInfoFetch(e.to_string()))?;

        if !response.status().is_success() {
            return Err(OAuthError::UserInfoFetch(format!(
                "OIDC userinfo returned status: {}",
                response.status()
            )));
        }

        let user_info: OidcUserInfo = response
            .json()
            .await
            .map_err(|e| OAuthError::UserInfoFetch(e.to_string()))?;

        let email = user_info.email.unwrap_or_else(|| {
            user_info
                .name
                .as_ref()
                .map(|n| n.to_lowercase())
                .unwrap_or_else(|| format!("{}@unknown", user_info.sub))
        });

        let username = user_info
            .name
            .clone()
            .or_else(|| {
                let combined = [user_info.given_name.clone(), user_info.family_name.clone()]
                    .into_iter()
                    .filter_map(|s| s)
                    .collect::<Vec<_>>()
                    .join(" ");
                if combined.is_empty() {
                    Some(user_info.sub.clone())
                } else {
                    Some(combined)
                }
            })
            .unwrap_or_else(|| user_info.sub.clone());

        Ok(OAuthUserInfo {
            provider_user_id: user_info.sub,
            email,
            username,
            avatar_url: user_info.picture,
            organizations: Vec::new(),
        })
    }

    pub async fn get_user_organizations(
        &self,
        _access_token: &str,
    ) -> Result<Vec<String>, OAuthError> {
        Ok(Vec::new())
    }
}
