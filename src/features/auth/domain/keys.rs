use base64::Engine;
use rand::prelude::*;
use rand_hc::Hc128Rng;

use crate::bootstrap::AppState;
use crate::error::Result;
use crate::features::auth::domain::api_key::{CreateApiKeyRequest, CreateApiKeyResponse};
use crate::features::auth::infra::store_key;

const KEY_LENGTH: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Prefix {
    Standard,
    Secret,
    Full,
}

pub const PREFIXES: [&str; 3] = ["thl_", "thl_sk_", "thalamus_"];

impl Prefix {
    pub fn as_str(&self) -> &str {
        match self {
            Prefix::Standard => PREFIXES[0],
            Prefix::Secret => PREFIXES[1],
            Prefix::Full => PREFIXES[2],
        }
    }
}

impl std::fmt::Display for Prefix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Generate a new API key with the given prefix and store it in the database
pub async fn generate_key(
    prefix: Prefix,
    request: CreateApiKeyRequest,
    state: &AppState,
) -> Result<CreateApiKeyResponse> {
    let mut key_bytes = vec![0u8; KEY_LENGTH];
    let mut rng: Hc128Rng = Hc128Rng::from_os_rng();
    rng.fill(&mut key_bytes[..]);
    // Encode the key in URL-safe base64. Padding is not necessary here.
    let key_base64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&key_bytes);
    let full_key = format!("{}{}", prefix, key_base64);

    // Store the key and return the response
    store_key(&full_key, request, state).await
}
