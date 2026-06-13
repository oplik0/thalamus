//! Core message types for LLM conversations
//!
//! Provides a unified message representation that maps naturally to
//! `OpenAI` Chat Completions, Anthropic Messages, Ollama Chat, and
//! `OpenAI` Responses API formats.

use serde::{Deserialize, Serialize};

use super::media::{ImageDetail, MediaSource};

/// Conversation role
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    Developer,
    User,
    Assistant,
    Tool,
}

/// Cache control directive for content blocks (Anthropic-style)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CacheControl {
    Ephemeral,
}

/// Per-block metadata (cache control, provider extensions)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BlockMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<serde_json::Value>,
}

/// A content part with optional per-block metadata
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnnotatedContentPart {
    #[serde(flatten)]
    pub part: ContentPart,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<BlockMetadata>,
}

impl AnnotatedContentPart {
    /// Create an annotated part with no metadata
    #[must_use]
    pub fn plain(part: ContentPart) -> Self {
        Self {
            part,
            metadata: None,
        }
    }

    /// Create an annotated part with cache control
    #[must_use]
    pub fn with_cache_control(part: ContentPart, cache_control: CacheControl) -> Self {
        Self {
            part,
            metadata: Some(BlockMetadata {
                cache_control: Some(cache_control),
                extensions: None,
            }),
        }
    }
}

/// Message content — accepts both `"content": "text"` and `"content": [...]`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Parts(Vec<AnnotatedContentPart>),
}

impl Content {
    /// Extract the concatenated text from this content
    #[must_use]
    pub fn text(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::Parts(parts) => parts
                .iter()
                .filter_map(|ap| match &ap.part {
                    ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
        }
    }

    /// Check if this content contains any tool calls
    #[must_use]
    pub fn has_tool_calls(&self) -> bool {
        match self {
            Self::Text(_) => false,
            Self::Parts(parts) => parts
                .iter()
                .any(|ap| matches!(ap.part, ContentPart::ToolCall(_))),
        }
    }

    /// Extract all tool call parts from this content
    #[must_use]
    pub fn tool_calls(&self) -> Vec<&ToolCallPart> {
        match self {
            Self::Text(_) => vec![],
            Self::Parts(parts) => parts
                .iter()
                .filter_map(|ap| match &ap.part {
                    ContentPart::ToolCall(tc) => Some(tc),
                    _ => None,
                })
                .collect(),
        }
    }

    /// Check if this content is empty (no text and no parts)
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Text(s) => s.is_empty(),
            Self::Parts(parts) => parts.is_empty(),
        }
    }
}

/// A single content block within a multipart message
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    /// Plain text content
    Text { text: String },
    /// Image content
    Image {
        source: MediaSource,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<ImageDetail>,
    },
    /// Audio content
    Audio { source: MediaSource },
    /// Document content (PDFs, etc.)
    Document {
        source: MediaSource,
        #[serde(skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
    },
    /// Video content
    Video { source: MediaSource },
    /// Tool/function call from the assistant
    ToolCall(ToolCallPart),
    /// Model thinking/reasoning content
    Thinking {
        #[serde(skip_serializing_if = "Option::is_none")]
        thinking: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
    },
    /// Reasoning summary (`OpenAI`)
    ReasoningSummary { text: String },
    /// Model refusal
    Refusal { refusal: String },
    /// Citation reference
    Citation {
        #[serde(skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        start_index: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        end_index: Option<u32>,
    },
}

/// A tool/function call invocation
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCallPart {
    pub id: String,
    pub name: String,
    /// JSON-encoded arguments
    pub arguments: String,
}

/// A single message in an LLM conversation
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub role: Role,
    pub content: Content,
    /// Tool call ID for `Role::Tool` messages
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Participant name
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Provider-specific passthrough data
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<serde_json::Value>,
}

impl Message {
    /// Create a system message
    #[must_use]
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: Content::Text(text.into()),
            tool_call_id: None,
            name: None,
            extensions: None,
        }
    }

    /// Create a user message
    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: Content::Text(text.into()),
            tool_call_id: None,
            name: None,
            extensions: None,
        }
    }

    /// Create an assistant message
    #[must_use]
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: Content::Text(text.into()),
            tool_call_id: None,
            name: None,
            extensions: None,
        }
    }

    /// Create a tool result message
    #[must_use]
    pub fn tool_result(tool_call_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Content::Text(text.into()),
            tool_call_id: Some(tool_call_id.into()),
            name: None,
            extensions: None,
        }
    }

    /// Create a multimodal message with content parts (wraps each in `AnnotatedContentPart` with no metadata)
    #[must_use]
    pub fn multimodal(role: Role, parts: Vec<ContentPart>) -> Self {
        Self {
            role,
            content: Content::Parts(parts.into_iter().map(AnnotatedContentPart::plain).collect()),
            tool_call_id: None,
            name: None,
            extensions: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_serialization() {
        assert_eq!(serde_json::to_string(&Role::System).unwrap(), r#""system""#);
        assert_eq!(serde_json::to_string(&Role::User).unwrap(), r#""user""#);
        assert_eq!(
            serde_json::to_string(&Role::Assistant).unwrap(),
            r#""assistant""#
        );
        assert_eq!(serde_json::to_string(&Role::Tool).unwrap(), r#""tool""#);
        assert_eq!(
            serde_json::to_string(&Role::Developer).unwrap(),
            r#""developer""#
        );
    }

    #[test]
    fn cache_control_serde() {
        let cc = CacheControl::Ephemeral;
        let json = serde_json::to_string(&cc).unwrap();
        assert_eq!(json, r#"{"type":"ephemeral"}"#);
        let round_tripped: CacheControl = serde_json::from_str(&json).unwrap();
        assert_eq!(cc, round_tripped);
    }

    #[test]
    fn block_metadata_with_cache_control() {
        let meta = BlockMetadata {
            cache_control: Some(CacheControl::Ephemeral),
            extensions: None,
        };
        let json = serde_json::to_string(&meta).unwrap();
        assert!(json.contains(r#""cache_control"#));
        assert!(!json.contains("extensions"));
        let round_tripped: BlockMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(meta, round_tripped);
    }

    #[test]
    fn annotated_content_part_plain() {
        let ap = AnnotatedContentPart::plain(ContentPart::Text {
            text: "hello".to_string(),
        });
        assert!(ap.metadata.is_none());
        let json = serde_json::to_string(&ap).unwrap();
        assert!(json.contains(r#""type":"text""#));
        assert!(!json.contains("metadata"));
    }

    #[test]
    fn annotated_content_part_with_cache_control() {
        let ap = AnnotatedContentPart::with_cache_control(
            ContentPart::Text {
                text: "cached".to_string(),
            },
            CacheControl::Ephemeral,
        );
        assert!(ap.metadata.is_some());
        let json = serde_json::to_string(&ap).unwrap();
        assert!(json.contains(r#""cache_control"#));
        let round_tripped: AnnotatedContentPart = serde_json::from_str(&json).unwrap();
        assert_eq!(ap, round_tripped);
    }

    #[test]
    fn content_text_string_form() {
        let json = r#""Hello, world!""#;
        let content: Content = serde_json::from_str(json).unwrap();
        assert_eq!(content, Content::Text("Hello, world!".to_string()));
        assert_eq!(content.text(), "Hello, world!");
        assert!(!content.is_empty());
        assert!(!content.has_tool_calls());
    }

    #[test]
    fn content_parts_array_form() {
        let json = r#"[{"type":"text","text":"Hello"},{"type":"text","text":" world"}]"#;
        let content: Content = serde_json::from_str(json).unwrap();
        assert_eq!(content.text(), "Hello world");
        assert!(!content.has_tool_calls());
    }

    #[test]
    fn content_empty() {
        assert!(Content::Text(String::new()).is_empty());
        assert!(Content::Parts(vec![]).is_empty());
        assert!(!Content::Text("hi".to_string()).is_empty());
    }

    #[test]
    fn content_with_tool_calls() {
        let content = Content::Parts(vec![
            AnnotatedContentPart::plain(ContentPart::Text {
                text: "Let me call a function".to_string(),
            }),
            AnnotatedContentPart::plain(ContentPart::ToolCall(ToolCallPart {
                id: "call_1".to_string(),
                name: "get_weather".to_string(),
                arguments: r#"{"city":"London"}"#.to_string(),
            })),
        ]);
        assert!(content.has_tool_calls());
        assert_eq!(content.tool_calls().len(), 1);
        assert_eq!(content.tool_calls()[0].name, "get_weather");
    }

    #[test]
    fn content_part_tagged_serde() {
        let part = ContentPart::Text {
            text: "hello".to_string(),
        };
        let json = serde_json::to_string(&part).unwrap();
        assert!(json.contains(r#""type":"text""#));

        let round_tripped: ContentPart = serde_json::from_str(&json).unwrap();
        assert_eq!(part, round_tripped);
    }

    #[test]
    fn content_part_image() {
        let part = ContentPart::Image {
            source: MediaSource::Url {
                url: "https://example.com/img.png".to_string(),
            },
            detail: Some(ImageDetail::High),
        };
        let json = serde_json::to_string(&part).unwrap();
        assert!(json.contains(r#""type":"image""#));

        let round_tripped: ContentPart = serde_json::from_str(&json).unwrap();
        assert_eq!(part, round_tripped);
    }

    #[test]
    fn content_part_thinking() {
        let part = ContentPart::Thinking {
            thinking: Some("Let me reason about this...".to_string()),
            signature: Some("sig123".to_string()),
            encrypted_content: None,
        };
        let json = serde_json::to_string(&part).unwrap();
        assert!(json.contains(r#""type":"thinking""#));
        assert!(!json.contains("encrypted_content"));

        let round_tripped: ContentPart = serde_json::from_str(&json).unwrap();
        assert_eq!(part, round_tripped);
    }

    #[test]
    fn content_part_refusal() {
        let part = ContentPart::Refusal {
            refusal: "I cannot help with that".to_string(),
        };
        let json = serde_json::to_string(&part).unwrap();
        assert!(json.contains(r#""type":"refusal""#));
    }

    #[test]
    fn message_system_constructor() {
        let msg = Message::system("You are a helpful assistant");
        assert_eq!(msg.role, Role::System);
        assert_eq!(msg.content.text(), "You are a helpful assistant");
        assert!(msg.tool_call_id.is_none());
    }

    #[test]
    fn message_user_constructor() {
        let msg = Message::user("Hello");
        assert_eq!(msg.role, Role::User);
        assert_eq!(msg.content.text(), "Hello");
    }

    #[test]
    fn message_assistant_constructor() {
        let msg = Message::assistant("Hi there!");
        assert_eq!(msg.role, Role::Assistant);
        assert_eq!(msg.content.text(), "Hi there!");
    }

    #[test]
    fn message_tool_result_constructor() {
        let msg = Message::tool_result("call_1", r#"{"temp": 72}"#);
        assert_eq!(msg.role, Role::Tool);
        assert_eq!(msg.tool_call_id.as_deref(), Some("call_1"));
    }

    #[test]
    fn message_multimodal_constructor() {
        let msg = Message::multimodal(
            Role::User,
            vec![
                ContentPart::Text {
                    text: "What's in this image?".to_string(),
                },
                ContentPart::Image {
                    source: MediaSource::Url {
                        url: "https://example.com/cat.jpg".to_string(),
                    },
                    detail: None,
                },
            ],
        );
        assert_eq!(msg.role, Role::User);
        assert!(!msg.content.is_empty());
        // Verify parts are wrapped in AnnotatedContentPart
        if let Content::Parts(parts) = &msg.content {
            assert_eq!(parts.len(), 2);
            assert!(parts[0].metadata.is_none());
            assert!(parts[1].metadata.is_none());
        } else {
            panic!("expected Content::Parts");
        }
    }

    #[test]
    fn message_round_trip() {
        let msg = Message {
            role: Role::Assistant,
            content: Content::Text("Hello".to_string()),
            tool_call_id: None,
            name: Some("assistant_1".to_string()),
            extensions: Some(serde_json::json!({"custom": true})),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, deserialized);
    }

    #[test]
    fn message_optional_fields_skipped() {
        let msg = Message::user("hi");
        let json = serde_json::to_string(&msg).unwrap();
        assert!(!json.contains("tool_call_id"));
        assert!(!json.contains("name"));
        assert!(!json.contains("extensions"));
    }

    #[test]
    fn message_extensions_passthrough() {
        let json = r#"{"role":"user","content":"hi","extensions":{"provider_field":"value"}}"#;
        let msg: Message = serde_json::from_str(json).unwrap();
        assert!(msg.extensions.is_some());
        assert_eq!(msg.extensions.unwrap()["provider_field"], "value");
    }
}
