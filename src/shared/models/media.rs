//! Media source types for multimodal content
//!
//! Provides a unified representation for images, audio, documents,
//! and video content from different LLM providers.

use serde::{Deserialize, Serialize};

/// Source for media content (images, audio, documents, video)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "source_type", rename_all = "snake_case")]
pub enum MediaSource {
    /// URL-based media reference
    Url { url: String },
    /// Base64-encoded inline media
    Base64 { data: String, media_type: String },
}

/// Level of detail for image processing
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ImageDetail {
    #[default]
    Auto,
    Low,
    High,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_source_round_trip() {
        let source = MediaSource::Url {
            url: "https://example.com/image.png".to_string(),
        };
        let json = serde_json::to_string(&source).unwrap();
        let deserialized: MediaSource = serde_json::from_str(&json).unwrap();
        assert_eq!(source, deserialized);
        assert!(json.contains(r#""source_type":"url""#));
    }

    #[test]
    fn base64_source_round_trip() {
        let source = MediaSource::Base64 {
            data: "aGVsbG8=".to_string(),
            media_type: "image/png".to_string(),
        };
        let json = serde_json::to_string(&source).unwrap();
        let deserialized: MediaSource = serde_json::from_str(&json).unwrap();
        assert_eq!(source, deserialized);
        assert!(json.contains(r#""source_type":"base64""#));
        assert!(json.contains(r#""media_type":"image/png""#));
    }

    #[test]
    fn image_detail_serialization() {
        assert_eq!(
            serde_json::to_string(&ImageDetail::Auto).unwrap(),
            r#""auto""#
        );
        assert_eq!(
            serde_json::to_string(&ImageDetail::Low).unwrap(),
            r#""low""#
        );
        assert_eq!(
            serde_json::to_string(&ImageDetail::High).unwrap(),
            r#""high""#
        );
    }

    #[test]
    fn image_detail_default() {
        assert_eq!(ImageDetail::default(), ImageDetail::Auto);
    }
}
