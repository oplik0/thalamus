//! LLM response types
//!
//! Unified response representations using an item-based output model
//! that maps to all supported provider response formats. The item model
//! is a superset: Chat Completions = single `OutputItem::Message`,
//! Responses API = multiple item types.

use serde::{Deserialize, Serialize};

use super::message::{AnnotatedContentPart, ContentPart, Role};

/// A chat completion response
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub model: String,
    pub output: Vec<OutputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<ResponseStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    /// Provider-specific passthrough data
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<serde_json::Value>,
}

/// A single output item in the response (item-based model)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputItem {
    /// A conversation message
    Message {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        role: Role,
        content: Vec<AnnotatedContentPart>,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<ItemStatus>,
        #[serde(skip_serializing_if = "Option::is_none")]
        finish_reason: Option<FinishReason>,
    },
    /// A function call the model wants to make
    FunctionCall {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        call_id: String,
        name: String,
        arguments: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<ItemStatus>,
    },
    /// Reasoning/thinking output
    Reasoning {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        summary: Vec<AnnotatedContentPart>,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<ItemStatus>,
    },
}

impl OutputItem {
    /// Extract text from a Message variant's content
    #[must_use]
    pub fn text(&self) -> Option<String> {
        match self {
            Self::Message { content, .. } => {
                let text: String = content
                    .iter()
                    .filter_map(|ap| match &ap.part {
                        ContentPart::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                if text.is_empty() { None } else { Some(text) }
            }
            _ => None,
        }
    }
}

/// Status of an individual output item
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    InProgress,
    Completed,
    Incomplete,
}

/// Overall response status
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResponseStatus {
    InProgress,
    Completed,
    Incomplete,
    Failed,
}

/// Reason the model stopped generating
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
}

/// Token usage statistics
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u32>,
    /// `OpenAI` reasoning tokens
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u32>,
    /// Cache read tokens (Anthropic/OpenAI)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u32>,
    /// Cache creation tokens (Anthropic)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<u32>,
}

impl Usage {
    /// Get total tokens, using `total_tokens` if available or computing from components
    #[must_use]
    pub fn computed_total(&self) -> Option<u32> {
        self.total_tokens
            .or_else(|| match (self.prompt_tokens, self.completion_tokens) {
                (Some(p), Some(c)) => p.checked_add(c),
                _ => None,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::models::message::{AnnotatedContentPart, ContentPart, Role};

    #[test]
    fn chat_response_round_trip() {
        let response = ChatResponse {
            id: Some("chatcmpl-123".to_string()),
            model: "gpt-oss:120b".to_string(),
            output: vec![OutputItem::Message {
                id: None,
                role: Role::Assistant,
                content: vec![AnnotatedContentPart::plain(ContentPart::Text {
                    text: "Hello!".to_string(),
                })],
                status: Some(ItemStatus::Completed),
                finish_reason: Some(FinishReason::Stop),
            }],
            status: Some(ResponseStatus::Completed),
            usage: Some(Usage {
                prompt_tokens: Some(10),
                completion_tokens: Some(5),
                total_tokens: Some(15),
                reasoning_tokens: None,
                cache_read_tokens: None,
                cache_creation_tokens: None,
            }),
            service_tier: None,
            extensions: None,
        };
        let json = serde_json::to_string(&response).unwrap();
        let deserialized: ChatResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(response, deserialized);
    }

    #[test]
    fn output_item_message_serde() {
        let item = OutputItem::Message {
            id: Some("msg_1".to_string()),
            role: Role::Assistant,
            content: vec![AnnotatedContentPart::plain(ContentPart::Text {
                text: "Hello".to_string(),
            })],
            status: Some(ItemStatus::Completed),
            finish_reason: Some(FinishReason::Stop),
        };
        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains(r#""type":"message""#));
        let round_tripped: OutputItem = serde_json::from_str(&json).unwrap();
        assert_eq!(item, round_tripped);
    }

    #[test]
    fn output_item_function_call_serde() {
        let item = OutputItem::FunctionCall {
            id: Some("fc_1".to_string()),
            call_id: "call_123".to_string(),
            name: "get_weather".to_string(),
            arguments: r#"{"city":"London"}"#.to_string(),
            status: Some(ItemStatus::Completed),
        };
        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains(r#""type":"function_call""#));
        let round_tripped: OutputItem = serde_json::from_str(&json).unwrap();
        assert_eq!(item, round_tripped);
    }

    #[test]
    fn output_item_reasoning_serde() {
        let item = OutputItem::Reasoning {
            id: Some("rs_1".to_string()),
            summary: vec![AnnotatedContentPart::plain(ContentPart::Text {
                text: "I thought about this carefully".to_string(),
            })],
            status: Some(ItemStatus::Completed),
        };
        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains(r#""type":"reasoning""#));
        let round_tripped: OutputItem = serde_json::from_str(&json).unwrap();
        assert_eq!(item, round_tripped);
    }

    #[test]
    fn output_item_text_helper() {
        let msg = OutputItem::Message {
            id: None,
            role: Role::Assistant,
            content: vec![
                AnnotatedContentPart::plain(ContentPart::Text {
                    text: "Hello".to_string(),
                }),
                AnnotatedContentPart::plain(ContentPart::Text {
                    text: " world".to_string(),
                }),
            ],
            status: None,
            finish_reason: None,
        };
        assert_eq!(msg.text(), Some("Hello world".to_string()));

        let fc = OutputItem::FunctionCall {
            id: None,
            call_id: "c".to_string(),
            name: "f".to_string(),
            arguments: "{}".to_string(),
            status: None,
        };
        assert_eq!(fc.text(), None);
    }

    #[test]
    fn output_item_text_empty_content() {
        let msg = OutputItem::Message {
            id: None,
            role: Role::Assistant,
            content: vec![],
            status: None,
            finish_reason: None,
        };
        assert_eq!(msg.text(), None);
    }

    #[test]
    fn chat_response_mixed_output_items() {
        let response = ChatResponse {
            id: Some("resp_1".to_string()),
            model: "gpt-oss:120b".to_string(),
            output: vec![
                OutputItem::Reasoning {
                    id: Some("rs_1".to_string()),
                    summary: vec![AnnotatedContentPart::plain(ContentPart::Text {
                        text: "Thinking...".to_string(),
                    })],
                    status: Some(ItemStatus::Completed),
                },
                OutputItem::Message {
                    id: Some("msg_1".to_string()),
                    role: Role::Assistant,
                    content: vec![AnnotatedContentPart::plain(ContentPart::Text {
                        text: "The answer is 42".to_string(),
                    })],
                    status: Some(ItemStatus::Completed),
                    finish_reason: Some(FinishReason::Stop),
                },
                OutputItem::FunctionCall {
                    id: Some("fc_1".to_string()),
                    call_id: "call_1".to_string(),
                    name: "calculate".to_string(),
                    arguments: r#"{"expr":"6*7"}"#.to_string(),
                    status: Some(ItemStatus::Completed),
                },
            ],
            status: Some(ResponseStatus::Completed),
            usage: Some(Usage {
                prompt_tokens: Some(20),
                completion_tokens: Some(30),
                total_tokens: Some(50),
                ..Default::default()
            }),
            service_tier: Some("default".to_string()),
            extensions: None,
        };
        let json = serde_json::to_string(&response).unwrap();
        let deserialized: ChatResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(response, deserialized);
        assert_eq!(deserialized.output.len(), 3);
    }

    #[test]
    fn item_status_variants() {
        let variants = vec![
            (ItemStatus::InProgress, r#""in_progress""#),
            (ItemStatus::Completed, r#""completed""#),
            (ItemStatus::Incomplete, r#""incomplete""#),
        ];
        for (variant, expected) in variants {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected);
            let round_tripped: ItemStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(variant, round_tripped);
        }
    }

    #[test]
    fn response_status_variants() {
        let variants = vec![
            (ResponseStatus::InProgress, r#""in_progress""#),
            (ResponseStatus::Completed, r#""completed""#),
            (ResponseStatus::Incomplete, r#""incomplete""#),
            (ResponseStatus::Failed, r#""failed""#),
        ];
        for (variant, expected) in variants {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected);
            let round_tripped: ResponseStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(variant, round_tripped);
        }
    }

    #[test]
    fn finish_reason_variants() {
        let variants = vec![
            (FinishReason::Stop, r#""stop""#),
            (FinishReason::Length, r#""length""#),
            (FinishReason::ToolCalls, r#""tool_calls""#),
            (FinishReason::ContentFilter, r#""content_filter""#),
        ];
        for (variant, expected) in variants {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected);
            let round_tripped: FinishReason = serde_json::from_str(&json).unwrap();
            assert_eq!(variant, round_tripped);
        }
    }

    #[test]
    fn usage_computed_total_from_field() {
        let usage = Usage {
            total_tokens: Some(100),
            prompt_tokens: Some(60),
            completion_tokens: Some(40),
            ..Default::default()
        };
        assert_eq!(usage.computed_total(), Some(100));
    }

    #[test]
    fn usage_computed_total_from_components() {
        let usage = Usage {
            total_tokens: None,
            prompt_tokens: Some(60),
            completion_tokens: Some(40),
            ..Default::default()
        };
        assert_eq!(usage.computed_total(), Some(100));
    }

    #[test]
    fn usage_computed_total_none() {
        let usage = Usage {
            total_tokens: None,
            prompt_tokens: Some(60),
            completion_tokens: None,
            ..Default::default()
        };
        assert_eq!(usage.computed_total(), None);
    }

    #[test]
    fn usage_default_empty() {
        let usage = Usage::default();
        let json = serde_json::to_string(&usage).unwrap();
        assert_eq!(json, "{}");
    }
}
