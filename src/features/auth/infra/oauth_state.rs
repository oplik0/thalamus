//! OAuth state management for CSRF protection

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{Duration, Utc};
use dashmap::DashMap;
use std::sync::Arc;
use uuid::Uuid;

use crate::error::Result;
use crate::features::auth::domain::oauth::OAuthFlowState;

/// Store for temporary OAuth state (CSRF protection)
pub trait OAuthStateStore: Send + Sync {
    /// Store OAuth state and return the state token
    fn store_state(&self, state: OAuthFlowState) -> Result<String>;

    /// Get OAuth state by token
    fn get_state(&self, state_token: &str) -> Result<Option<OAuthFlowState>>;

    /// Remove OAuth state by token
    fn remove_state(&self, state_token: &str) -> Result<()>;

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
    pub fn new() -> Self {
        Self {
            states: Arc::new(DashMap::new()),
        }
    }

    /// Generate a new PKCE code verifier
    pub fn generate_pkce_verifier() -> String {
        use rand::RngCore;
        let mut bytes = vec![0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        URL_SAFE_NO_PAD.encode(bytes)
    }

    /// Generate PKCE code challenge from verifier
    pub fn generate_pkce_challenge(verifier: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(verifier.as_bytes());
        let result = hasher.finalize();
        URL_SAFE_NO_PAD.encode(result)
    }

    /// Generate a random state token
    pub fn generate_state_token() -> String {
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
        let token = state.state_token.clone();
        self.states.insert(token.clone(), state);
        Ok(token)
    }

    fn get_state(&self, state_token: &str) -> Result<Option<OAuthFlowState>> {
        self.cleanup_expired();
        Ok(self.states.get(state_token).map(|entry| entry.clone()))
    }

    fn remove_state(&self, state_token: &str) -> Result<()> {
        self.states.remove(state_token);
        Ok(())
    }

    fn cleanup_expired(&self) {
        let now = Utc::now();
        self.states.retain(|_, state| state.expires_at > now);
    }
}

/// Create a new OAuth flow state with all required fields
pub fn create_oauth_flow_state(
    provider_name: String,
    redirect_url: Option<String>,
    expires_in_minutes: i64,
) -> (OAuthFlowState, String) {
    let state_token = InMemoryOAuthStateStore::generate_state_token();
    let pkce_verifier = InMemoryOAuthStateStore::generate_pkce_verifier();

    let state = OAuthFlowState {
        state_token: state_token.clone(),
        pkce_verifier,
        provider_name,
        redirect_url,
        expires_at: Utc::now() + Duration::minutes(expires_in_minutes),
    };

    (state, state_token)
}
