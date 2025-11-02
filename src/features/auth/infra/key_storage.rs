use crate::bootstrap::AppState;
use crate::error::{Error, Result};
use crate::features::auth::domain::api_key::{
    ApiKey, CreateApiKeyRequest, CreateApiKeyResponse, ValidatedApiKey,
};
use argon2::password_hash::{SaltString, rand_core::OsRng};
/// API Key generation and validation
///
/// Uses database-stored random tokens with prefixes for easy identification
use argon2::{Argon2, Params, PasswordHash, PasswordHasher, PasswordVerifier};
use chrono::Utc;
use uuid::Uuid;

/// Store a new API key in the database
pub async fn store_key(
    full_key: &str,
    request: CreateApiKeyRequest,
    state: &AppState,
) -> Result<CreateApiKeyResponse> {
    // Hash the key for storage
    let salt = SaltString::generate(&mut OsRng);

    let argon2 = Argon2::new_with_secret(
        // TODO: use config secret here
        b"some_secret_key_for_argon2",
        argon2::Algorithm::Argon2id,
        argon2::Version::V0x13,
        // we have a random input so this is overkill anyway
        Params::new(1024, 2, 1, Some(64)).unwrap(),
        // unwrapping since the Params::new can only fail on invalid params
    )
    .map_err(|e| Error::Internal(format!("Failed to create Argon2 instance: {}", e)))?;

    let key_hash = argon2
        .hash_password(full_key.as_bytes(), &salt)
        .map_err(|e| Error::Internal(format!("Failed to hash key: {}", e)))?
        .to_string();

    // Extract the key_id (the part after the prefix, used for lookups)
    let key_id = full_key.to_string();

    // Extract prefix for display (first 8-12 chars depending on prefix length)
    let key_prefix = if full_key.len() >= 12 {
        full_key[..12].to_string()
    } else {
        full_key.to_string()
    };

    let id = Uuid::new_v4();
    let created_at = Utc::now();

    // Store the hashed key in the database
    sqlx::query!(
        r#"
        INSERT INTO api_keys (
            id, key_id, key_hash, key_prefix,
            user_id, team_id, name, description,
            scopes, is_active, expires_at, created_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, true, $10, $11)
        "#,
        id,
        key_id,
        key_hash,
        key_prefix,
        request.user_id,
        request.team_id,
        request.name,
        request.description,
        request.scopes.as_deref(),
        request.expires_at,
        created_at
    )
    .execute(&state.db_pool)
    .await?;

    Ok(CreateApiKeyResponse {
        id,
        key: full_key.to_string(),
        key_prefix,
        name: request.name,
        created_at,
        expires_at: request.expires_at,
    })
}

/// Validate an API key and return the associated key information
pub async fn validate_key(key: &str, state: &AppState) -> Result<ValidatedApiKey> {
    // Look up the key in the database by key_id
    let result = sqlx::query_as!(
        ApiKey,
        r#"
        SELECT
            id, key_id, key_hash, key_prefix,
            user_id, team_id, name, description,
            scopes, is_active as "is_active!", last_used_at,
            expires_at, created_at, revoked_at
        FROM api_keys
        WHERE key_id = $1
        "#,
        key
    )
    .fetch_optional(&state.db_pool)
    .await?;

    let api_key = result.ok_or_else(|| Error::Authentication("Invalid API key".to_string()))?;

    // Check if key is active
    if !api_key.is_active {
        return Err(Error::Authentication("API key is not active".to_string()));
    }

    // Check if key is revoked
    if api_key.revoked_at.is_some() {
        return Err(Error::Authentication(
            "API key has been revoked".to_string(),
        ));
    }

    // Check if key is expired
    if let Some(expires_at) = api_key.expires_at {
        if expires_at < Utc::now() {
            return Err(Error::Authentication("API key has expired".to_string()));
        }
    }

    // Verify the key hash
    let parsed_hash = PasswordHash::new(&api_key.key_hash)
        .map_err(|e| Error::Internal(format!("Failed to parse stored hash: {}", e)))?;

    let argon2 = Argon2::new_with_secret(
        b"some_secret_key_for_argon2",
        argon2::Algorithm::Argon2id,
        argon2::Version::V0x13,
        // note: these params are overriden by the parsed ones in verify
        Params::default(),
    )
    .map_err(|e| Error::Internal(format!("Failed to create Argon2 instance: {}", e)))?;

    argon2
        .verify_password(key.as_bytes(), &parsed_hash)
        .map_err(|_| Error::Authentication("Invalid API key".to_string()))?;

    // Queue background task to update last_used_at
    // This doesn't block the response - the task runs asynchronously
    let task = crate::features::auth::infra::UpdateKeyUsageTask::new(api_key.id);
    if let Err(e) = state.tasks.queue(task).await {
        tracing::warn!(
            key_id = %api_key.id,
            error = %e,
            "Failed to queue key usage update task"
        );
        // Don't fail authentication if we can't queue the task
    }

    Ok(ValidatedApiKey {
        id: api_key.id,
        key_id: api_key.key_id,
        user_id: api_key.user_id,
        team_id: api_key.team_id,
        scopes: api_key.scopes,
    })
}

/// Revoke an API key
pub async fn revoke_key(key_id: &str, state: &AppState) -> Result<()> {
    sqlx::query!(
        r#"
        UPDATE api_keys
        SET is_active = false, revoked_at = $1
        WHERE key_id = $2
        "#,
        Utc::now(),
        key_id
    )
    .execute(&state.db_pool)
    .await?;

    Ok(())
}

/// List all API keys for a user
pub async fn list_user_keys(user_id: Uuid, state: &AppState) -> Result<Vec<ApiKey>> {
    let keys = sqlx::query_as!(
        ApiKey,
        r#"
        SELECT
            id, key_id, key_hash, key_prefix,
            user_id, team_id, name, description,
            scopes, is_active as "is_active!", last_used_at,
            expires_at, created_at, revoked_at
        FROM api_keys
        WHERE user_id = $1
        ORDER BY created_at DESC
        "#,
        user_id
    )
    .fetch_all(&state.db_pool)
    .await?;

    Ok(keys)
}

/// List all API keys for a team
pub async fn list_team_keys(team_id: Uuid, state: &AppState) -> Result<Vec<ApiKey>> {
    let keys = sqlx::query_as!(
        ApiKey,
        r#"
        SELECT
            id, key_id, key_hash, key_prefix,
            user_id, team_id, name, description,
            scopes, is_active as "is_active!", last_used_at,
            expires_at, created_at, revoked_at
        FROM api_keys
        WHERE team_id = $1
        ORDER BY created_at DESC
        "#,
        team_id
    )
    .fetch_all(&state.db_pool)
    .await?;

    Ok(keys)
}
