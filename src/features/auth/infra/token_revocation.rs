//! Token revocation and refresh token management
//!
//! This module provides:
//! - Token blacklist for explicit revocation (logout, admin revoke)
//! - Refresh token rotation with reuse detection
//! - Cleanup of expired revocations

use crate::bootstrap::AppState;
use crate::error::{Error, Result};
use chrono::{DateTime, Duration, Utc};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Check if a token JTI has been revoked
pub async fn is_token_revoked(jti: Uuid, state: &AppState) -> Result<bool> {
    let result = sqlx::query_scalar!(
        r#"
        SELECT EXISTS(
            SELECT 1 FROM token_revocations
            WHERE token_jti = $1
        )
        "#,
        jti
    )
    .fetch_one(&state.db_pool)
    .await?;

    Ok(result.unwrap_or(false))
}

/// Revoke a token by its JTI
pub async fn revoke_token(
    jti: Uuid,
    user_id: Uuid,
    expires_at: DateTime<Utc>,
    reason: &str,
    revoked_by: Option<Uuid>,
    state: &AppState,
) -> Result<()> {
    sqlx::query!(
        r#"
        INSERT INTO token_revocations (token_jti, user_id, expires_at, reason, revoked_by)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (token_jti) DO NOTHING
        "#,
        jti,
        user_id,
        expires_at,
        reason,
        revoked_by
    )
    .execute(&state.db_pool)
    .await?;

    tracing::info!(
        token_jti = %jti,
        user_id = %user_id,
        reason = %reason,
        "Token revoked"
    );

    Ok(())
}

/// Revoke all tokens for a user (e.g., password change, security incident)
pub async fn revoke_all_user_tokens(user_id: Uuid, reason: &str, state: &AppState) -> Result<u64> {
    // Note: This only works if we have a sessions/tokens table
    // For now, we invalidate all refresh tokens
    let result = sqlx::query!(
        r#"
        UPDATE refresh_tokens
        SET is_active = false,
            revoked_at = NOW(),
            revoked_reason = $2
        WHERE user_id = $1
          AND is_active = true
          AND revoked_at IS NULL
        "#,
        user_id,
        reason
    )
    .execute(&state.db_pool)
    .await?;

    tracing::info!(
        user_id = %user_id,
        reason = %reason,
        affected = result.rows_affected(),
        "All user refresh tokens revoked"
    );

    Ok(result.rows_affected())
}

/// Clean up expired token revocations (for maintenance)
pub async fn cleanup_expired_revocations(state: &AppState) -> Result<u64> {
    let result = sqlx::query!(
        r#"
        DELETE FROM token_revocations
        WHERE expires_at < NOW()
        "#
    )
    .execute(&state.db_pool)
    .await?;

    if result.rows_affected() > 0 {
        tracing::info!(
            count = result.rows_affected(),
            "Cleaned up expired token revocations"
        );
    }

    Ok(result.rows_affected())
}

/// Refresh token information
#[derive(Debug, Clone)]
pub struct RefreshTokenInfo {
    pub id: Uuid,
    pub user_id: Uuid,
    pub team_id: Uuid,
    pub family: Uuid,
    pub parent_token_jti: Option<Uuid>,
    pub scopes: Option<Vec<String>>,
    pub roles: Option<Vec<String>>,
    pub expires_at: DateTime<Utc>,
    pub is_active: bool,
    pub revoked_at: Option<DateTime<Utc>>,
}

/// Create a new refresh token
pub async fn create_refresh_token(
    user_id: Uuid,
    team_id: Uuid,
    family: Uuid,
    parent_token_jti: Option<Uuid>,
    scopes: Option<Vec<String>>,
    roles: Option<Vec<String>>,
    expires_in_days: i64,
    state: &AppState,
) -> Result<(String, RefreshTokenInfo)> {
    // Generate a random token
    let token_bytes: Vec<u8> = (0..32).map(|_| rand::random::<u8>()).collect();
    let token = base64::encode(&token_bytes);

    // Hash the token for storage
    let mut hasher = Sha256::new();
    hasher.update(&token_bytes);
    let token_hash = hex::encode(hasher.finalize());

    let expires_at = Utc::now() + Duration::days(expires_in_days);

    let id = Uuid::new_v4();

    sqlx::query!(
        r#"
        INSERT INTO refresh_tokens (
            id, user_id, team_id, token_hash, family, parent_token_jti,
            scopes, roles, expires_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        "#,
        id,
        user_id,
        team_id,
        token_hash,
        family,
        parent_token_jti,
        scopes.as_deref(),
        roles.as_deref(),
        expires_at
    )
    .execute(&state.db_pool)
    .await?;

    let info = RefreshTokenInfo {
        id,
        user_id,
        team_id,
        family,
        parent_token_jti,
        scopes,
        roles,
        expires_at,
        is_active: true,
        revoked_at: None,
    };

    tracing::debug!(
        refresh_token_id = %id,
        user_id = %user_id,
        "Refresh token created"
    );

    Ok((token, info))
}

/// Validate and consume a refresh token
/// Returns the token info if valid, or an error if invalid/revoked
pub async fn validate_refresh_token(token: &str, state: &AppState) -> Result<RefreshTokenInfo> {
    // Decode and hash the token
    let token_bytes = base64::decode(token)
        .map_err(|_| Error::Authentication("Invalid refresh token format".to_string()))?;

    let mut hasher = Sha256::new();
    hasher.update(&token_bytes);
    let token_hash = hex::encode(hasher.finalize());

    // Look up the token
    let row = sqlx::query!(
        r#"
        SELECT
            id, user_id, team_id, family, parent_token_jti,
            scopes as "scopes: Vec<String>",
            roles as "roles: Vec<String>",
            expires_at, is_active, revoked_at, last_used_at
        FROM refresh_tokens
        WHERE token_hash = $1
        "#,
        token_hash
    )
    .fetch_optional(&state.db_pool)
    .await?;

    let row = match row {
        Some(r) => r,
        None => return Err(Error::Authentication("Invalid refresh token".to_string())),
    };

    // Check if token is active
    if row.is_active == Some(false) {
        return Err(Error::Authentication(
            "Refresh token has been revoked".to_string(),
        ));
    }

    if row.revoked_at.is_some() {
        return Err(Error::Authentication(
            "Refresh token has been revoked".to_string(),
        ));
    }

    // Check expiration
    if row.expires_at < Utc::now() {
        return Err(Error::Authentication(
            "Refresh token has expired".to_string(),
        ));
    }

    // Update last used timestamp asynchronously
    let token_id = row.id;
    let pool = state.db_pool.clone();
    tokio::spawn(async move {
        let _ = sqlx::query!(
            "UPDATE refresh_tokens SET last_used_at = NOW() WHERE id = $1",
            token_id
        )
        .execute(&pool)
        .await;
    });

    Ok(RefreshTokenInfo {
        id: row.id,
        user_id: row.user_id,
        team_id: row.team_id,
        family: row.family,
        parent_token_jti: row.parent_token_jti,
        scopes: row.scopes,
        roles: row.roles,
        expires_at: row.expires_at,
        is_active: row.is_active.unwrap_or(false),
        revoked_at: row.revoked_at,
    })
}

/// Revoke a refresh token by its ID
pub async fn revoke_refresh_token(token_id: Uuid, reason: &str, state: &AppState) -> Result<()> {
    sqlx::query!(
        r#"
        UPDATE refresh_tokens
        SET is_active = false,
            revoked_at = NOW(),
            revoked_reason = $2
        WHERE id = $1
        "#,
        token_id,
        reason
    )
    .execute(&state.db_pool)
    .await?;

    tracing::info!(
        refresh_token_id = %token_id,
        reason = %reason,
        "Refresh token revoked"
    );

    Ok(())
}

/// Revoke all refresh tokens in a family (for rotation violation detection)
pub async fn revoke_refresh_token_family(
    family: Uuid,
    reason: &str,
    state: &AppState,
) -> Result<u64> {
    let result = sqlx::query!(
        r#"
        UPDATE refresh_tokens
        SET is_active = false,
            revoked_at = NOW(),
            revoked_reason = $2
        WHERE family = $1
          AND is_active = true
          AND revoked_at IS NULL
        "#,
        family,
        reason
    )
    .execute(&state.db_pool)
    .await?;

    tracing::warn!(
        family = %family,
        reason = %reason,
        affected = result.rows_affected(),
        "Refresh token family revoked"
    );

    Ok(result.rows_affected())
}

/// Detect refresh token reuse (potential theft)
/// If a previously used refresh token is presented again, revoke the entire family
pub async fn detect_refresh_token_reuse(
    token_info: &RefreshTokenInfo,
    state: &AppState,
) -> Result<bool> {
    // Check if this token has been used before (has children in the chain)
    let has_descendants = sqlx::query_scalar!(
        r#"
        SELECT EXISTS(
            SELECT 1 FROM refresh_tokens
            WHERE parent_token_jti = $1
        )
        "#,
        token_info.id
    )
    .fetch_one(&state.db_pool)
    .await?;

    let is_reuse = has_descendants.unwrap_or(false);

    if is_reuse {
        // This is a reuse - revoke the entire family
        revoke_refresh_token_family(token_info.family, "refresh_token_reuse_detected", state)
            .await?;

        tracing::warn!(
            family = %token_info.family,
            token_id = %token_info.id,
            "Refresh token reuse detected - family revoked"
        );
    }

    Ok(is_reuse)
}

/// Rotate a refresh token (create new, mark old as inactive) inside a single
/// serialisable transaction.
///
/// The old token row is locked with `SELECT … FOR UPDATE` as the very first
/// statement so that concurrent rotation attempts for the same token queue up
/// behind the lock rather than racing past each other.
pub async fn rotate_refresh_token(
    old_token: &str,
    expires_in_days: i64,
    state: &AppState,
) -> Result<(String, RefreshTokenInfo)> {
    // Hash the incoming token before opening the transaction so we don't
    // hold the lock while doing CPU work.
    let token_bytes = base64::decode(old_token)
        .map_err(|_| Error::Authentication("Invalid refresh token format".to_string()))?;
    let mut hasher = Sha256::new();
    hasher.update(&token_bytes);
    let token_hash = hex::encode(hasher.finalize());

    // Open a transaction and immediately acquire a row-level lock on the
    // old token.  Any concurrent rotation for the same token will block
    // here until we commit or roll back.
    let mut tx = state.db_pool.begin().await?;

    let row = sqlx::query!(
        r#"
        SELECT
            id, user_id, team_id, family, parent_token_jti,
            scopes as "scopes: Vec<String>",
            roles as "roles: Vec<String>",
            expires_at, is_active, revoked_at
        FROM refresh_tokens
        WHERE token_hash = $1
        FOR UPDATE
        "#,
        token_hash
    )
    .fetch_optional(&mut *tx)
    .await?;

    let row = match row {
        Some(r) => r,
        None => return Err(Error::Authentication("Invalid refresh token".to_string())),
    };

    // --- inline validation (was validate_refresh_token) ---
    if row.is_active == Some(false) {
        return Err(Error::Authentication(
            "Refresh token has been revoked".to_string(),
        ));
    }
    if row.revoked_at.is_some() {
        return Err(Error::Authentication(
            "Refresh token has been revoked".to_string(),
        ));
    }
    if row.expires_at < Utc::now() {
        return Err(Error::Authentication(
            "Refresh token has expired".to_string(),
        ));
    }

    let old_id = row.id;
    let family = row.family;
    let user_id = row.user_id;
    let team_id = row.team_id;
    let scopes = row.scopes;
    let roles = row.roles;

    // --- inline reuse detection (was detect_refresh_token_reuse) ---
    // If children already exist this token was already consumed; revoke the
    // whole family inside the same transaction so the revocation is atomic
    // with the lock release.
    let has_descendants = sqlx::query_scalar!(
        r#"
        SELECT EXISTS(
            SELECT 1 FROM refresh_tokens
            WHERE parent_token_jti = $1
        )
        "#,
        old_id
    )
    .fetch_one(&mut *tx)
    .await?;

    if has_descendants.unwrap_or(false) {
        sqlx::query!(
            r#"
            UPDATE refresh_tokens
            SET is_active = false,
                revoked_at = NOW(),
                revoked_reason = $2
            WHERE family = $1
              AND is_active = true
              AND revoked_at IS NULL
            "#,
            family,
            "refresh_token_reuse_detected"
        )
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        tracing::warn!(
            family = %family,
            token_id = %old_id,
            "Refresh token reuse detected - family revoked"
        );

        return Err(Error::Authentication(
            "Refresh token reuse detected".to_string(),
        ));
    }

    // --- inline new-token creation (was create_refresh_token) ---
    let new_token_bytes: Vec<u8> = (0..32).map(|_| rand::random::<u8>()).collect();
    let new_token = base64::encode(&new_token_bytes);
    let mut new_hasher = Sha256::new();
    new_hasher.update(&new_token_bytes);
    let new_token_hash = hex::encode(new_hasher.finalize());

    let expires_at = Utc::now() + Duration::days(expires_in_days);
    let new_id = Uuid::new_v4();

    sqlx::query!(
        r#"
        INSERT INTO refresh_tokens (
            id, user_id, team_id, token_hash, family, parent_token_jti,
            scopes, roles, expires_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        "#,
        new_id,
        user_id,
        team_id,
        new_token_hash,
        family,
        old_id,
        scopes.as_deref(),
        roles.as_deref(),
        expires_at
    )
    .execute(&mut *tx)
    .await?;

    // Deactivate the old token within the same transaction.
    sqlx::query!(
        r#"
        UPDATE refresh_tokens
        SET is_active = false
        WHERE id = $1
        "#,
        old_id
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    let new_info = RefreshTokenInfo {
        id: new_id,
        user_id,
        team_id,
        family,
        parent_token_jti: Some(old_id),
        scopes,
        roles,
        expires_at,
        is_active: true,
        revoked_at: None,
    };

    tracing::debug!(
        old_token_id = %old_id,
        new_token_id = %new_id,
        family = %family,
        "Refresh token rotated"
    );

    Ok((new_token, new_info))
}

/// List active refresh tokens for a user
pub async fn list_user_refresh_tokens(
    user_id: Uuid,
    state: &AppState,
) -> Result<Vec<RefreshTokenInfo>> {
    let rows = sqlx::query!(
        r#"
        SELECT
            id, user_id, team_id, family, parent_token_jti,
            scopes as "scopes: Vec<String>",
            roles as "roles: Vec<String>",
            expires_at, is_active, revoked_at
        FROM refresh_tokens
        WHERE user_id = $1
        ORDER BY created_at DESC
        "#,
        user_id
    )
    .fetch_all(&state.db_pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| RefreshTokenInfo {
            id: row.id,
            user_id: row.user_id,
            team_id: row.team_id,
            family: row.family,
            parent_token_jti: row.parent_token_jti,
            scopes: row.scopes,
            roles: row.roles,
            expires_at: row.expires_at,
            is_active: row.is_active.unwrap_or(false),
            revoked_at: row.revoked_at,
        })
        .collect())
}

/// Clean up expired refresh tokens (for maintenance)
pub async fn cleanup_expired_refresh_tokens(state: &AppState) -> Result<u64> {
    let result = sqlx::query!(
        r#"
        DELETE FROM refresh_tokens
        WHERE expires_at < NOW() - INTERVAL '7 days'
        "#
    )
    .execute(&state.db_pool)
    .await?;

    if result.rows_affected() > 0 {
        tracing::info!(
            count = result.rows_affected(),
            "Cleaned up expired refresh tokens"
        );
    }

    Ok(result.rows_affected())
}

// Helper module for base64 encoding (since base64 crate may not be available)
mod base64 {
    use base64::{Engine, engine::general_purpose::STANDARD};

    pub fn encode<T: AsRef<[u8]>>(input: T) -> String {
        STANDARD.encode(input)
    }

    pub fn decode<T: AsRef<[u8]>>(input: T) -> Result<Vec<u8>, base64::DecodeError> {
        STANDARD.decode(input)
    }
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
    use super::*;

    #[test]
    fn test_hex_encoding() {
        let input = vec![0x00, 0x0f, 0xff];
        assert_eq!(hex::encode(input), "000fff");
    }
}
