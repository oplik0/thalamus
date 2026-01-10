//! Internal routing metadata for LLM requests
//!
//! Carries Thalamus-specific routing hints that are not part of
//! any external API format.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Thalamus-internal metadata for routing and tracking
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RequestMetadata {
    /// Unique request identifier
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<Uuid>,
    /// Upstream user identifier
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Routing queue name / priority level
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    /// Preferred backend name
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_backend: Option<String>,
    /// Preferred backend tags for routing
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_tags: Option<Vec<String>>,
    /// Arbitrary extra metadata
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_metadata() {
        let meta = RequestMetadata::default();
        assert!(meta.request_id.is_none());
        assert!(meta.user.is_none());
        assert!(meta.priority.is_none());
        assert!(meta.preferred_backend.is_none());
        assert!(meta.preferred_tags.is_none());
        assert!(meta.extra.is_none());
    }

    #[test]
    fn metadata_round_trip() {
        let id = Uuid::new_v4();
        let meta = RequestMetadata {
            request_id: Some(id),
            user: Some("user-123".to_string()),
            priority: Some("realtime".to_string()),
            preferred_backend: Some("backend-a".to_string()),
            preferred_tags: Some(vec!["gpu".to_string(), "fast".to_string()]),
            extra: Some(serde_json::json!({"team": "engineering"})),
        };
        let json = serde_json::to_string(&meta).unwrap();
        let deserialized: RequestMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(meta, deserialized);
    }

    #[test]
    fn metadata_optional_fields_skipped() {
        let meta = RequestMetadata::default();
        let json = serde_json::to_string(&meta).unwrap();
        assert_eq!(json, "{}");
    }
}
