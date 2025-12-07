use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// API Key domain model
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    pub id: Uuid,
    pub key_id: String,
    pub key_hash: String,
    pub key_prefix: String,
    pub user_id: Uuid,
    pub team_id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub scopes: Option<Vec<String>>,
    pub is_active: bool,
    pub last_used_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

/// Request to create a new API key
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateApiKeyRequest {
    pub user_id: Uuid,
    pub team_id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub scopes: Option<Vec<String>>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// Response when creating an API key (includes the plain-text key)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateApiKeyResponse {
    pub id: Uuid,
    pub key: String, // Full key (only returned once)
    pub key_prefix: String,
    pub name: String,
    pub scopes: Option<Vec<String>>,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// API Key validation result
#[derive(Debug, Clone)]
pub struct ValidatedApiKey {
    pub id: Uuid,
    pub key_id: String,
    pub user_id: Uuid,
    pub team_id: Uuid,
    pub scopes: Option<Vec<String>>,
}
