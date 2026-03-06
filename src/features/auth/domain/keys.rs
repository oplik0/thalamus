use base64::{Engine as _, engine::general_purpose};
use rand_08::RngCore;
use rand_08::rngs::OsRng;

use crate::bootstrap::AppState;
use crate::error::Result;
use crate::features::auth::domain::api_key::{CreateApiKeyRequest, CreateApiKeyResponse};
use crate::features::auth::infra::key_storage::store_key;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Prefix {
    Standard,
    Secret,
    Full,
}

pub const PREFIXES: [&str; 3] = ["thl", "thl_sk", "thalamus"];

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
    let mut secret_bytes = vec![0u8; 32];
    let mut public_bytes = vec![0u8; 16];
    let mut rng = OsRng;
    rng.fill_bytes(&mut secret_bytes);
    rng.fill_bytes(&mut public_bytes);

    let secret_part = general_purpose::URL_SAFE_NO_PAD.encode(&secret_bytes);
    let public_part = general_purpose::URL_SAFE_NO_PAD.encode(&public_bytes);

    let api_key = format!("{}_{}_{}", prefix.as_str(), public_part, secret_part);

    store_key(&api_key, request.clone(), state).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prefix_as_str() {
        assert_eq!(Prefix::Standard.as_str(), "thl");
        assert_eq!(Prefix::Secret.as_str(), "thl_sk");
        assert_eq!(Prefix::Full.as_str(), "thalamus");
    }

    #[test]
    fn test_prefix_display() {
        assert_eq!(format!("{}", Prefix::Standard), "thl");
        assert_eq!(format!("{}", Prefix::Secret), "thl_sk");
        assert_eq!(format!("{}", Prefix::Full), "thalamus");
    }

    #[test]
    fn test_key_generation_format() {
        // We can't easily test generate_key because it requires AppState and DB
        // But we can verify the logic manually if we extracted the formatting logic

        let prefix = Prefix::Standard;
        let public_str = "public";
        let secret_str = "secret";
        let full_key = format!("{}{}_{}", prefix, public_str, secret_str);

        assert_eq!(full_key, "thl_public_secret");
        assert!(full_key.starts_with("thl_"));
    }

    #[test]
    fn test_prefixes_array() {
        assert_eq!(PREFIXES.len(), 3);
        assert_eq!(PREFIXES[0], "thl");
        assert_eq!(PREFIXES[1], "thl_sk");
        assert_eq!(PREFIXES[2], "thalamus");
    }

    #[test]
    fn test_prefix_equality() {
        assert_eq!(Prefix::Standard, Prefix::Standard);
        assert_ne!(Prefix::Standard, Prefix::Secret);
        assert_eq!(Prefix::Secret, Prefix::Secret);
    }

    #[test]
    fn test_prefix_clone() {
        let prefix = Prefix::Secret;
        let cloned = prefix.clone();
        assert_eq!(prefix, cloned);
    }

    #[test]
    fn test_prefix_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(Prefix::Standard);
        set.insert(Prefix::Secret);
        set.insert(Prefix::Full);
        set.insert(Prefix::Standard); // Duplicate
        assert_eq!(set.len(), 3);
    }
}
