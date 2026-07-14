//! OAuth state management for CSRF and PKCE protection
//!
//! This module provides state storage with manual CSRF and PKCE implementation.

use chrono::{Duration, Utc};
use dashmap::DashMap;
use std::sync::Arc;
use uuid::Uuid;

use crate::error::Result;
use crate::features::auth::domain::oauth::OAuthFlowState;

/// Store for temporary OAuth state (CSRF and PKCE protection)
pub trait OAuthStateStore: Send + Sync {
    /// Store OAuth state and return the public CSRF token
    fn store_state(&self, state: OAuthFlowState) -> Result<String>;

    /// Get OAuth state by CSRF token
    fn get_state(&self, csrf_token: &str) -> Result<Option<OAuthFlowState>>;

    /// Remove OAuth state by CSRF token
    fn remove_state(&self, csrf_token: &str) -> Result<()>;

    /// Clean up expired states
    fn cleanup_expired(&self);
}

/// In-memory implementation of OAuth state store
pub struct InMemoryOAuthStateStore {
    states: Arc<DashMap<String, OAuthFlowState>>,
}

impl std::fmt::Debug for InMemoryOAuthStateStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryOAuthStateStore")
            .field("states", &"<DashMap>")
            .finish()
    }
}

impl InMemoryOAuthStateStore {
    #[must_use]
    pub fn new() -> Self {
        Self {
            states: Arc::new(DashMap::new()),
        }
    }

    /// Generate a new PKCE code verifier (43-128 characters)
    #[must_use]
    pub fn generate_pkce_verifier() -> String {
        use rand::Rng;
        const VERIFIER_LEN: usize = 43;
        let charset: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
        let mut rng = rand::rng();
        (0..VERIFIER_LEN)
            .map(|_| {
                let idx = rng.random_range(0..charset.len());
                charset[idx] as char
            })
            .collect()
    }

    /// Generate PKCE code challenge from verifier using SHA256
    #[must_use]
    pub fn generate_pkce_challenge(verifier: &str) -> String {
        use base64::Engine;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(verifier.as_bytes());
        let result = hasher.finalize();
        URL_SAFE_NO_PAD.encode(result)
    }

    /// Generate a random CSRF state token
    #[must_use]
    pub fn generate_csrf_token() -> String {
        Uuid::new_v4().to_string()
    }
}

impl Default for InMemoryOAuthStateStore {
    fn default() -> Self {
        Self::new()
    }
}

impl OAuthStateStore for InMemoryOAuthStateStore {
    fn store_state(&self, state: OAuthFlowState) -> Result<String> {
        // Store using the public csrf_token as key
        let token = state.csrf_token.clone();
        self.states.insert(token.clone(), state);
        Ok(token)
    }

    fn get_state(&self, csrf_token: &str) -> Result<Option<OAuthFlowState>> {
        self.cleanup_expired();
        Ok(self.states.get(csrf_token).map(|entry| entry.clone()))
    }

    fn remove_state(&self, csrf_token: &str) -> Result<()> {
        self.states.remove(csrf_token);
        Ok(())
    }

    fn cleanup_expired(&self) {
        let now = Utc::now();
        self.states.retain(|_, state| state.expires_at > now);
    }
}

/// Create a new OAuth flow state with all required fields
#[must_use]
pub fn create_oauth_flow_state(
    provider_name: String,
    redirect_url: Option<String>,
    expires_in_minutes: i64,
) -> (OAuthFlowState, String) {
    // Generate CSRF token
    let csrf_token = InMemoryOAuthStateStore::generate_csrf_token();

    // Generate PKCE verifier and challenge
    let pkce_verifier = InMemoryOAuthStateStore::generate_pkce_verifier();
    let pkce_challenge = InMemoryOAuthStateStore::generate_pkce_challenge(&pkce_verifier);

    let state = OAuthFlowState {
        csrf_token: csrf_token.clone(),
        pkce_verifier,
        pkce_challenge,
        provider_name,
        redirect_url,
        expires_at: Utc::now() + Duration::minutes(expires_in_minutes),
    };

    (state, csrf_token)
}
