//! Signing key management for HTTP Signatures
//!
//! This module provides:
//! - Key pair generation (Ed25519, RSA-PSS, ECDSA P-256)
//! - Key storage (public only, private returned once)
//! - Key lookup and validation

use crate::bootstrap::AppState;
use crate::error::{Error, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::{DateTime, Duration, Utc};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Supported signature algorithms
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureAlgorithm {
    /// Ed25519 (recommended, fastest, smallest signatures)
    Ed25519,
    /// RSA-PSS with SHA-512 (widely supported)
    RsaPssSha512,
    /// ECDSA P-256 with SHA-256 (good balance)
    EcdsaP256Sha256,
}

impl SignatureAlgorithm {
    /// Parse from string
    pub fn from_str(s: &str) -> Result<Self> {
        match s {
            "ed25519" => Ok(Self::Ed25519),
            "rsa-pss-sha512" => Ok(Self::RsaPssSha512),
            "ecdsa-p256-sha256" => Ok(Self::EcdsaP256Sha256),
            _ => Err(Error::InvalidInput(format!("Unknown algorithm: {}", s))),
        }
    }

    /// Get the string representation
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ed25519 => "ed25519",
            Self::RsaPssSha512 => "rsa-pss-sha512",
            Self::EcdsaP256Sha256 => "ecdsa-p256-sha256",
        }
    }
}

/// Result of generating a new signing key pair
#[derive(Debug, Clone)]
pub struct GeneratedKeyPair {
    /// The key ID (user-facing identifier)
    pub key_id: String,
    /// The private key (PEM format) - ONLY RETURNED ONCE
    pub private_key: String,
    /// The public key (PEM format)
    pub public_key: String,
    /// The algorithm used
    pub algorithm: SignatureAlgorithm,
    /// Key fingerprint (SHA-256 of public key)
    pub fingerprint: String,
}

/// Signing key information (from database)
#[derive(Debug, Clone)]
pub struct SigningKey {
    pub id: Uuid,
    pub key_id: String,
    pub user_id: Uuid,
    pub team_id: Uuid,
    pub public_key: String,
    pub algorithm: String,
    pub fingerprint: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub scopes: Option<Vec<String>>,
    pub is_active: bool,
    pub revoked_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub use_count: i32,
    pub created_at: DateTime<Utc>,
}

/// Generate a new Ed25519 key pair
pub fn generate_ed25519_key_pair() -> Result<(String, String)> {
    use ed25519_dalek::SigningKey;
    use rand_08::rngs::OsRng;

    let mut rng = OsRng;
    let signing_key = SigningKey::generate(&mut rng);
    let verifying_key = signing_key.verifying_key();

    // Format as PEM-like (base64 encoded raw bytes)
    let private_bytes = signing_key.to_bytes();
    let public_bytes = verifying_key.to_bytes();

    let private_pem = format!(
        "-----BEGIN ED25519 PRIVATE KEY-----\n{}\n-----END ED25519 PRIVATE KEY-----",
        format_pem_body(&BASE64.encode(&private_bytes))
    );

    let public_pem = format!(
        "-----BEGIN ED25519 PUBLIC KEY-----\n{}\n-----END ED25519 PUBLIC KEY-----",
        format_pem_body(&BASE64.encode(&public_bytes))
    );

    Ok((private_pem, public_pem))
}

/// Generate a new RSA-PSS key pair (4096 bits)
pub fn generate_rsa_key_pair() -> Result<(String, String)> {
    use rand_08::rngs::OsRng;
    use rsa::RsaPrivateKey;
    use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey};

    let mut rng = OsRng;
    let private_key = RsaPrivateKey::new(&mut rng, 4096)
        .map_err(|e| Error::Internal(format!("Failed to generate RSA key: {}", e)))?;

    let public_key = private_key.to_public_key();

    let private_pem = private_key
        .to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
        .map_err(|e| Error::Internal(format!("Failed to encode private key: {}", e)))?
        .to_string();

    let public_pem = public_key
        .to_public_key_pem(rsa::pkcs8::LineEnding::LF)
        .map_err(|e| Error::Internal(format!("Failed to encode public key: {}", e)))?;

    Ok((private_pem, public_pem))
}

/// Generate a new ECDSA P-256 key pair
pub fn generate_ecdsa_key_pair() -> Result<(String, String)> {
    use p256::ecdsa::SigningKey;
    use p256::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
    use rand_08::rngs::OsRng;

    let mut rng = OsRng;
    let signing_key = SigningKey::random(&mut rng);

    let private_pem = signing_key
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|e| Error::Internal(format!("Failed to encode private key: {}", e)))?
        .to_string();

    let verifying_key = signing_key.verifying_key();
    let public_pem = verifying_key
        .to_public_key_pem(LineEnding::LF)
        .map_err(|e| Error::Internal(format!("Failed to encode public key: {}", e)))?;

    Ok((private_pem, public_pem))
}

/// Generate a key ID (user-friendly identifier)
fn generate_key_id() -> String {
    // Generate a short, user-friendly ID: key_abc123xyz
    let random_bytes: Vec<u8> = (0..12).map(|_| rand::random::<u8>()).collect();
    format!("key_{}", hex::encode(&random_bytes[..6]))
}

/// Compute key fingerprint (SHA-256 of public key bytes)
fn compute_fingerprint(public_key_pem: &str) -> Result<String> {
    // Extract just the base64 content from PEM
    let base64_content: String = public_key_pem
        .lines()
        .filter(|line| !line.starts_with("-----") && !line.trim().is_empty())
        .collect();

    let key_bytes = BASE64
        .decode(&base64_content)
        .map_err(|e| Error::Internal(format!("Invalid PEM encoding: {}", e)))?;

    let mut hasher = Sha256::new();
    hasher.update(&key_bytes);
    Ok(hex::encode(hasher.finalize()))
}

/// Format base64 content in 64-character lines (PEM style)
fn format_pem_body(base64: &str) -> String {
    base64
        .as_bytes()
        .chunks(64)
        .map(|chunk| std::str::from_utf8(chunk).unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Create a new signing key
pub async fn create_signing_key(
    user_id: Uuid,
    team_id: Uuid,
    algorithm: SignatureAlgorithm,
    name: Option<String>,
    description: Option<String>,
    scopes: Option<Vec<String>>,
    expires_in_days: Option<i64>,
    state: &AppState,
) -> Result<GeneratedKeyPair> {
    // Generate the key pair
    let (private_pem, public_pem) = match algorithm {
        SignatureAlgorithm::Ed25519 => generate_ed25519_key_pair()?,
        SignatureAlgorithm::RsaPssSha512 => generate_rsa_key_pair()?,
        SignatureAlgorithm::EcdsaP256Sha256 => generate_ecdsa_key_pair()?,
    };

    let key_id = generate_key_id();
    let fingerprint = compute_fingerprint(&public_pem)?;
    let expires_at = expires_in_days.map(|days| Utc::now() + Duration::days(days));

    // Store in database
    sqlx::query!(
        r#"
        INSERT INTO signing_keys (
            key_id, user_id, team_id, public_key, algorithm,
            key_fingerprint, name, description, scopes, expires_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
        "#,
        key_id,
        user_id,
        team_id,
        public_pem,
        algorithm.as_str(),
        fingerprint,
        name,
        description,
        scopes.as_deref(),
        expires_at
    )
    .execute(&state.db_pool)
    .await?;

    tracing::info!(
        key_id = %key_id,
        user_id = %user_id,
        algorithm = %algorithm.as_str(),
        "Signing key created"
    );

    Ok(GeneratedKeyPair {
        key_id,
        private_key: private_pem,
        public_key: public_pem,
        algorithm,
        fingerprint,
    })
}

/// Get a signing key by key ID
pub async fn get_signing_key(key_id: &str, state: &AppState) -> Result<SigningKey> {
    let row = sqlx::query!(
        r#"
        SELECT
            id, key_id, user_id, team_id, public_key, algorithm,
            key_fingerprint, name, description,
            scopes as "scopes: Vec<String>",
            is_active, revoked_at, expires_at, last_used_at, use_count, created_at
        FROM signing_keys
        WHERE key_id = $1
        "#,
        key_id
    )
    .fetch_optional(&state.db_pool)
    .await?;

    let row = row.ok_or_else(|| Error::NotFound("Signing key not found".to_string()))?;

    Ok(SigningKey {
        id: row.id,
        key_id: row.key_id,
        user_id: row.user_id,
        team_id: row.team_id,
        public_key: row.public_key,
        algorithm: row.algorithm,
        fingerprint: row.key_fingerprint,
        name: row.name,
        description: row.description,
        scopes: row.scopes,
        is_active: row.is_active.unwrap_or(false),
        revoked_at: row.revoked_at,
        expires_at: row.expires_at,
        last_used_at: row.last_used_at,
        use_count: row.use_count.unwrap_or(0),
        created_at: row.created_at,
    })
}

/// List signing keys for a user
pub async fn list_user_signing_keys(
    user_id: Uuid,
    include_inactive: bool,
    state: &AppState,
) -> Result<Vec<SigningKey>> {
    // Use a single query with conditional filtering to avoid type mismatches
    let rows = sqlx::query!(
        r#"
        SELECT
            id, key_id, user_id, team_id, public_key, algorithm,
            key_fingerprint, name, description,
            scopes as "scopes: Vec<String>",
            is_active, revoked_at, expires_at, last_used_at, use_count, created_at
        FROM signing_keys
        WHERE user_id = $1
            AND ($2 = true OR (is_active = true AND revoked_at IS NULL))
        ORDER BY created_at DESC
        "#,
        user_id,
        include_inactive
    )
    .fetch_all(&state.db_pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| SigningKey {
            id: row.id,
            key_id: row.key_id,
            user_id: row.user_id,
            team_id: row.team_id,
            public_key: row.public_key,
            algorithm: row.algorithm,
            fingerprint: row.key_fingerprint,
            name: row.name,
            description: row.description,
            scopes: row.scopes,
            is_active: row.is_active.unwrap_or(false),
            revoked_at: row.revoked_at,
            expires_at: row.expires_at,
            last_used_at: row.last_used_at,
            use_count: row.use_count.unwrap_or(0),
            created_at: row.created_at,
        })
        .collect())
}

/// Revoke a signing key
pub async fn revoke_signing_key(
    key_id: &str,
    user_id: Uuid,
    reason: &str,
    state: &AppState,
) -> Result<()> {
    let result = sqlx::query!(
        r#"
        UPDATE signing_keys
        SET is_active = false,
            revoked_at = NOW()
        WHERE key_id = $1 AND user_id = $2
        "#,
        key_id,
        user_id
    )
    .execute(&state.db_pool)
    .await?;

    if result.rows_affected() == 0 {
        return Err(Error::NotFound("Signing key not found".to_string()));
    }

    tracing::info!(
        key_id = %key_id,
        user_id = %user_id,
        reason = %reason,
        "Signing key revoked"
    );

    Ok(())
}

/// Look up a signing key by fingerprint (for quick lookup)
pub async fn get_signing_key_by_fingerprint(
    fingerprint: &str,
    state: &AppState,
) -> Result<Option<SigningKey>> {
    let row = sqlx::query!(
        r#"
        SELECT
            id, key_id, user_id, team_id, public_key, algorithm,
            key_fingerprint, name, description,
            scopes as "scopes: Vec<String>",
            is_active, revoked_at, expires_at, last_used_at, use_count, created_at
        FROM signing_keys
        WHERE key_fingerprint = $1
        "#,
        fingerprint
    )
    .fetch_optional(&state.db_pool)
    .await?;

    Ok(row.map(|row| SigningKey {
        id: row.id,
        key_id: row.key_id,
        user_id: row.user_id,
        team_id: row.team_id,
        public_key: row.public_key,
        algorithm: row.algorithm,
        fingerprint: row.key_fingerprint,
        name: row.name,
        description: row.description,
        scopes: row.scopes,
        is_active: row.is_active.unwrap_or(false),
        revoked_at: row.revoked_at,
        expires_at: row.expires_at,
        last_used_at: row.last_used_at,
        use_count: row.use_count.unwrap_or(0),
        created_at: row.created_at,
    }))
}

// Helper module for hex encoding
mod hex {
    pub fn encode<T: AsRef<[u8]>>(input: T) -> String {
        input
            .as_ref()
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use tokio_test::assert_ok;

    use super::*;

    #[test]
    fn test_generate_key_id() {
        let id1 = generate_key_id();
        let id2 = generate_key_id();

        assert!(id1.starts_with("key_"));
        assert!(id2.starts_with("key_"));
        assert_ne!(id1, id2); // Should be unique
    }

    #[test]
    fn test_compute_fingerprint() {
        let public_key =
            "-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEAtest\n-----END PUBLIC KEY-----";
        let fingerprint = assert_ok!(compute_fingerprint(public_key));
        // Should be 64 hex characters (SHA-256)
        assert_eq!(fingerprint.len(), 64);
        // Should be valid hex
        assert!(fingerprint.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_format_pem_body() {
        let input = "a".repeat(100);
        let formatted = format_pem_body(&input);

        let lines: Vec<_> = formatted.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].len(), 64);
        assert_eq!(lines[1].len(), 36);
    }
}
