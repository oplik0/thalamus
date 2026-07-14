use axum_tasks::{Task, TaskHandler, TaskOutput};
use chrono::{DateTime, Utc};
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

/// Global database pool for background tasks
///
/// This is initialized at application startup and used by background tasks
/// to access the database without passing the entire `AppState` around.
static DB_POOL: OnceCell<PgPool> = OnceCell::new();

/// Initialize the global database pool for background tasks
///
/// This should be called once during application initialization.
pub fn init_task_db_pool(pool: PgPool) {
    DB_POOL.set(pool).ok();
}

/// Get the global database pool for background tasks
fn get_db_pool() -> Option<&'static PgPool> {
    DB_POOL.get()
}

/// Background task to update the `last_used_at` timestamp for an API key
///
/// This task is queued after successful key validation and runs asynchronously,
/// allowing the authentication request to complete without waiting for the database update.
#[derive(Task, Debug, Clone, Serialize, Deserialize)]
#[task(description = "Update API key last used timestamp", retry = true)]
pub struct UpdateKeyUsageTask {
    pub key_id: Uuid,
    pub timestamp: DateTime<Utc>,
}

impl UpdateKeyUsageTask {
    /// Create a new task to update key usage
    #[must_use]
    pub fn new(key_id: Uuid) -> Self {
        Self {
            key_id,
            timestamp: Utc::now(),
        }
    }

    /// Execute the task - update the `last_used_at` timestamp in the database
    pub async fn execute(&self) -> TaskOutput {
        let pool = if let Some(pool) = get_db_pool() {
            pool
        } else {
            tracing::error!("Database pool not initialized for background tasks");
            return TaskOutput::PermanentError("Database pool not available".to_string());
        };

        match sqlx::query!(
            r#"
            UPDATE api_keys
            SET last_used_at = $1
            WHERE id = $2
            "#,
            self.timestamp,
            self.key_id
        )
        .execute(pool)
        .await
        {
            Ok(_) => {
                tracing::debug!(
                    key_id = %self.key_id,
                    timestamp = %self.timestamp,
                    "Successfully updated API key last_used_at"
                );
                TaskOutput::Success(serde_json::json!({
                    "key_id": self.key_id,
                    "updated_at": self.timestamp
                }))
            }
            Err(e) => {
                tracing::warn!(
                    key_id = %self.key_id,
                    error = %e,
                    "Failed to update API key last_used_at"
                );
                // Return retryable error for database issues
                TaskOutput::RetryableError(format!("Database error: {e}"))
            }
        }
    }
}
