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
    pub project_id: Option<Uuid>,
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
    pub project_id: Option<Uuid>,
    pub name: String,
    pub description: Option<String>,
    pub scopes: Option<Vec<String>>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// Response when creating an API key (includes the plain-text key)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateApiKeyResponse {
    pub id: Uuid,
    pub key_id: String, // The key identifier (e.g., "abc123") - used for revocation
    pub key: String,    // Full key (only returned once)
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
    pub project_id: Option<Uuid>,
    pub scopes: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_api_key_creation() {
        let id = Uuid::new_v4();
        let user_id = Uuid::new_v4();
        let team_id = Uuid::new_v4();
        let now = Utc::now();

        let api_key = ApiKey {
            id,
            key_id: "test_key_id".to_string(),
            key_hash: "hashed_secret".to_string(),
            key_prefix: "thl_abc123".to_string(),
            user_id,
            team_id,
            project_id: None,
            name: "Test Key".to_string(),
            description: Some("A test key".to_string()),
            scopes: Some(vec!["read".to_string(), "write".to_string()]),
            is_active: true,
            last_used_at: None,
            expires_at: Some(now + chrono::Duration::days(30)),
            created_at: now,
            revoked_at: None,
        };

        assert_eq!(api_key.id, id);
        assert_eq!(api_key.user_id, user_id);
        assert_eq!(api_key.team_id, team_id);
        assert_eq!(api_key.name, "Test Key");
        assert!(api_key.is_active);
        assert!(api_key.revoked_at.is_none());
    }

    #[test]
    fn test_create_api_key_request() {
        let user_id = Uuid::new_v4();
        let team_id = Uuid::new_v4();
        let expires_at = Some(Utc::now() + chrono::Duration::days(30));

        let request = CreateApiKeyRequest {
            user_id,
            team_id,
            project_id: None,
            name: "My API Key".to_string(),
            description: Some("For testing".to_string()),
            scopes: Some(vec!["read".to_string()]),
            expires_at,
        };

        assert_eq!(request.user_id, user_id);
        assert_eq!(request.team_id, team_id);
        assert_eq!(request.name, "My API Key");
        assert_eq!(request.description, Some("For testing".to_string()));
        assert_eq!(request.scopes, Some(vec!["read".to_string()]));
    }

    #[test]
    fn test_create_api_key_response_serialization() {
        let id = Uuid::new_v4();
        let created_at = Utc::now();
        let expires_at = Some(created_at + chrono::Duration::days(30));

        let response = CreateApiKeyResponse {
            id,
            key_id: "test_key".to_string(),
            key: "thl_test_key_secret".to_string(),
            key_prefix: "thl_test".to_string(),
            name: "Test Key".to_string(),
            scopes: Some(vec!["read".to_string()]),
            created_at,
            expires_at,
        };

        let json = serde_json::to_string(&response).expect("Should serialize");
        assert!(json.contains("thl_test_key_secret"));
        assert!(json.contains("Test Key"));

        let deserialized: CreateApiKeyResponse =
            serde_json::from_str(&json).expect("Should deserialize");
        assert_eq!(deserialized.id, response.id);
        assert_eq!(deserialized.key, response.key);
        assert_eq!(deserialized.key_prefix, response.key_prefix);
    }

    #[test]
    fn test_validated_api_key() {
        let id = Uuid::new_v4();
        let user_id = Uuid::new_v4();
        let team_id = Uuid::new_v4();

        let validated = ValidatedApiKey {
            id,
            key_id: "key_123".to_string(),
            user_id,
            team_id,
            project_id: None,
            scopes: Some(vec!["admin".to_string()]),
        };

        assert_eq!(validated.id, id);
        assert_eq!(validated.key_id, "key_123");
        assert_eq!(validated.user_id, user_id);
        assert_eq!(validated.team_id, team_id);
        assert_eq!(validated.scopes, Some(vec!["admin".to_string()]));
    }

    #[test]
    fn test_api_key_serialization_roundtrip() {
        let id = Uuid::new_v4();
        let user_id = Uuid::new_v4();
        let team_id = Uuid::new_v4();
        let now = Utc::now();

        let api_key = ApiKey {
            id,
            key_id: "test_key_id".to_string(),
            key_hash: "hashed_secret".to_string(),
            key_prefix: "thl_abc123".to_string(),
            user_id,
            team_id,
            project_id: None,
            name: "Test Key".to_string(),
            description: None,
            scopes: None,
            is_active: true,
            last_used_at: None,
            expires_at: None,
            created_at: now,
            revoked_at: None,
        };

        let json = serde_json::to_string(&api_key).expect("Should serialize");
        let deserialized: ApiKey = serde_json::from_str(&json).expect("Should deserialize");

        assert_eq!(deserialized.id, api_key.id);
        assert_eq!(deserialized.key_id, api_key.key_id);
        assert_eq!(deserialized.user_id, api_key.user_id);
        assert_eq!(deserialized.team_id, api_key.team_id);
        assert_eq!(deserialized.name, api_key.name);
        assert_eq!(deserialized.is_active, api_key.is_active);
    }
}
