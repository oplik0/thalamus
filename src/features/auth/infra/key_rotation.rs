//! API Key rotation with grace period support
//!
//! This module provides secure key rotation:
//! - Create new key while keeping old key valid during grace period
//! - Automatic expiration of old keys after grace period
//! - Audit trail of key rotations

use crate::bootstrap::AppState;
use crate::error::{Error, Result};
use crate::features::auth::domain::api_key::{CreateApiKeyRequest, CreateApiKeyResponse};
use crate::features::auth::domain::keys::{Prefix, generate_key};
use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

/// Rotation result containing both old and new key information
#[derive(Debug, Clone)]
pub struct KeyRotationResult {
    pub old_key_id: String,
    pub old_key_expires_at: DateTime<Utc>,
    pub new_key: CreateApiKeyResponse,
}

/// Rotate an API key with a grace period
///
/// The old key remains valid until the grace period expires.
/// This allows clients to update their configurations without downtime.
pub async fn rotate_key_with_grace_period(
    old_key_id: &str,
    grace_period_minutes: i64,
    reason: Option<&str>,
    state: &AppState,
) -> Result<KeyRotationResult> {
    // Get the old key details
    let old_key = sqlx::query!(
        r#"
        SELECT
            id, user_id, team_id, name, description, scopes as "scopes: Vec<String>"
        FROM api_keys
        WHERE key_id = $1 AND is_active = true AND revoked_at IS NULL
        "#,
        old_key_id
    )
    .fetch_optional(&state.db_pool)
    .await?;

    let old_key = old_key.ok_or_else(|| Error::NotFound("API key not found".to_string()))?;

    let grace_period_ends = Utc::now() + Duration::minutes(grace_period_minutes);

    // Generate new key with same permissions
    let request = CreateApiKeyRequest {
        user_id: old_key.user_id,
        team_id: old_key.team_id,
        name: format!("{} (rotated)", old_key.name),
        description: old_key.description.clone(),
        scopes: old_key.scopes.clone(),
        expires_at: None, // New key doesn't expire by default
    };

    let new_key = generate_key(Prefix::Standard, request, state).await?;

    // Mark old key as rotated
    sqlx::query!(
        r#"
        UPDATE api_keys
        SET rotated_from = id,
            rotated_at = NOW(),
            grace_period_ends_at = $2,
            rotation_reason = $3,
            name = name || ' (rotated - expires soon)'
        WHERE key_id = $1
        "#,
        old_key_id,
        grace_period_ends,
        reason.unwrap_or("user_initiated")
    )
    .execute(&state.db_pool)
    .await?;

    tracing::info!(
        old_key_id = %old_key_id,
        new_key_id = %new_key.id,
        grace_period_minutes = %grace_period_minutes,
        "API key rotated with grace period"
    );

    Ok(KeyRotationResult {
        old_key_id: old_key_id.to_string(),
        old_key_expires_at: grace_period_ends,
        new_key,
    })
}

/// Rotate an API key immediately (no grace period)
pub async fn rotate_key_immediate(
    old_key_id: &str,
    reason: Option<&str>,
    state: &AppState,
) -> Result<CreateApiKeyResponse> {
    // Get the old key details
    let old_key = sqlx::query!(
        r#"
        SELECT
            id, user_id, team_id, name, description, scopes as "scopes: Vec<String>"
        FROM api_keys
        WHERE key_id = $1 AND is_active = true AND revoked_at IS NULL
        "#,
        old_key_id
    )
    .fetch_optional(&state.db_pool)
    .await?;

    let old_key = old_key.ok_or_else(|| Error::NotFound("API key not found".to_string()))?;

    // Generate new key with same permissions
    let request = CreateApiKeyRequest {
        user_id: old_key.user_id,
        team_id: old_key.team_id,
        name: format!("{} (rotated)", old_key.name),
        description: old_key.description.clone(),
        scopes: old_key.scopes.clone(),
        expires_at: None,
    };

    let new_key = generate_key(Prefix::Standard, request, state).await?;

    // Immediately revoke old key
    sqlx::query!(
        r#"
        UPDATE api_keys
        SET is_active = false,
            revoked_at = NOW(),
            rotation_reason = $2
        WHERE key_id = $1
        "#,
        old_key_id,
        reason.unwrap_or("immediate_rotation")
    )
    .execute(&state.db_pool)
    .await?;

    tracing::info!(
        old_key_id = %old_key_id,
        new_key_id = %new_key.id,
        "API key rotated immediately"
    );

    Ok(new_key)
}

/// Check if a key is within its grace period and still valid
pub async fn check_grace_period_status(
    key_id: &str,
    state: &AppState,
) -> Result<GracePeriodStatus> {
    let row = sqlx::query!(
        r#"
        SELECT
            grace_period_ends_at,
            is_active,
            revoked_at
        FROM api_keys
        WHERE key_id = $1
        "#,
        key_id
    )
    .fetch_optional(&state.db_pool)
    .await?;

    let row = match row {
        Some(r) => r,
        None => return Err(Error::NotFound("API key not found".to_string())),
    };

    if row.revoked_at.is_some() {
        return Ok(GracePeriodStatus::Revoked);
    }

    if row.is_active == Some(false) {
        return Ok(GracePeriodStatus::Inactive);
    }

    match row.grace_period_ends_at {
        Some(ends_at) => {
            let now = Utc::now();
            if now > ends_at {
                Ok(GracePeriodStatus::Expired)
            } else {
                let remaining = ends_at - now;
                Ok(GracePeriodStatus::Active {
                    expires_at: ends_at,
                    remaining_minutes: remaining.num_minutes(),
                })
            }
        }
        None => Ok(GracePeriodStatus::NoGracePeriod),
    }
}

/// Grace period status for a key
#[derive(Debug, Clone)]
pub enum GracePeriodStatus {
    /// Key is in grace period and still valid
    Active {
        expires_at: DateTime<Utc>,
        remaining_minutes: i64,
    },
    /// Key's grace period has expired
    Expired,
    /// Key was revoked
    Revoked,
    /// Key is inactive
    Inactive,
    /// Key has no grace period set
    NoGracePeriod,
}

/// Revoke all keys in grace period that have expired
/// Returns the number of keys revoked
pub async fn revoke_expired_grace_period_keys(state: &AppState) -> Result<u64> {
    let result = sqlx::query!(
        r#"
        UPDATE api_keys
        SET is_active = false,
            revoked_at = NOW()
        WHERE grace_period_ends_at IS NOT NULL
          AND grace_period_ends_at < NOW()
          AND is_active = true
          AND revoked_at IS NULL
        "#
    )
    .execute(&state.db_pool)
    .await?;

    if result.rows_affected() > 0 {
        tracing::info!(
            count = result.rows_affected(),
            "Revoked API keys with expired grace period"
        );
    }

    Ok(result.rows_affected())
}

/// Get rotation history for a key (all keys that were rotated from this one)
pub async fn get_rotation_history(
    key_id: &str,
    state: &AppState,
) -> Result<Vec<RotationHistoryEntry>> {
    let rows = sqlx::query!(
        r#"
        WITH RECURSIVE rotation_chain AS (
            -- Base case: the starting key
            SELECT
                id, key_id, name, rotated_from, rotated_at,
                grace_period_ends_at, rotation_reason,
                created_at, 0 as depth
            FROM api_keys
            WHERE key_id = $1

            UNION ALL

            -- Recursive case: keys rotated from this one
            SELECT
                k.id, k.key_id, k.name, k.rotated_from, k.rotated_at,
                k.grace_period_ends_at, k.rotation_reason,
                k.created_at, rc.depth + 1
            FROM api_keys k
            INNER JOIN rotation_chain rc ON k.rotated_from = rc.id
            WHERE rc.depth < 10 -- Prevent infinite loops
        )
        SELECT
            key_id,
            name,
            rotated_at,
            grace_period_ends_at,
            rotation_reason,
            created_at
        FROM rotation_chain
        WHERE depth > 0 -- Exclude the starting key
        ORDER BY created_at DESC
        "#,
        key_id
    )
    .fetch_all(&state.db_pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|row| {
            // Filter out rows with missing required fields
            let key_id = row.key_id?;
            let name = row.name?;
            let created_at = row.created_at?;

            Some(RotationHistoryEntry {
                key_id,
                name,
                rotated_at: row.rotated_at,
                grace_period_ends_at: row.grace_period_ends_at,
                rotation_reason: row.rotation_reason,
                created_at,
            })
        })
        .collect())
}

/// Single entry in rotation history
#[derive(Debug, Clone)]
pub struct RotationHistoryEntry {
    pub key_id: String,
    pub name: String,
    pub rotated_at: Option<DateTime<Utc>>,
    pub grace_period_ends_at: Option<DateTime<Utc>>,
    pub rotation_reason: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// List all keys currently in grace period for a user
pub async fn list_keys_in_grace_period(
    user_id: Uuid,
    state: &AppState,
) -> Result<Vec<KeyInGracePeriod>> {
    let rows = sqlx::query!(
        r#"
        SELECT
            key_id,
            name,
            grace_period_ends_at,
            rotation_reason,
            created_at
        FROM api_keys
        WHERE user_id = $1
          AND grace_period_ends_at IS NOT NULL
          AND grace_period_ends_at > NOW()
          AND is_active = true
          AND revoked_at IS NULL
        ORDER BY grace_period_ends_at ASC
        "#,
        user_id
    )
    .fetch_all(&state.db_pool)
    .await?;

    let now = Utc::now();
    Ok(rows
        .into_iter()
        .map(|row| {
            let remaining = row.grace_period_ends_at.map(|ends| ends - now);
            KeyInGracePeriod {
                key_id: row.key_id,
                name: row.name,
                grace_period_ends_at: row.grace_period_ends_at.unwrap(),
                remaining_minutes: remaining.map(|d| d.num_minutes()).unwrap_or(0),
                rotation_reason: row.rotation_reason,
                created_at: row.created_at,
            }
        })
        .collect())
}

/// Key currently in grace period
#[derive(Debug, Clone)]
pub struct KeyInGracePeriod {
    pub key_id: String,
    pub name: String,
    pub grace_period_ends_at: DateTime<Utc>,
    pub remaining_minutes: i64,
    pub rotation_reason: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: These tests require a database connection
    // They should be run as integration tests

    #[test]
    fn test_grace_period_status_display() {
        let status = GracePeriodStatus::Active {
            expires_at: Utc::now(),
            remaining_minutes: 30,
        };
        // Just verify it compiles and debug works
        let _ = format!("{:?}", status);
    }
}
