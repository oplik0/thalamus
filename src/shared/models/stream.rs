//! Streaming event types for LLM responses
//!
//! Provides a unified streaming protocol with item-aware lifecycle events
//! that map to `OpenAI` SSE, Anthropic SSE, and Ollama NDJSON streaming formats.
//!
//! Lifecycle: `ResponseCreated → OutputItemStart → N × ContentDelta →
//!            ContentDone → OutputItemDone → ResponseDone`

use serde::{Deserialize, Serialize};

use super::message::Role;
use super::response::{OutputItem, ResponseStatus, Usage};

/// A single streaming event (item-aware lifecycle)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum StreamEvent {
    /// Response has been created (stream starting)
    ResponseCreated {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    /// A new output item has started
    OutputItemStart {
        item_index: u32,
        item: OutputItemHeader,
    },
    /// Incremental content update within an output item
    ContentDelta {
        item_index: u32,
        content_index: u32,
        delta: ContentDeltaPayload,
    },
    /// A content block within an output item has finished
    ContentDone { item_index: u32, content_index: u32 },
    /// An output item has finished
    OutputItemDone {
        item_index: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        item: Option<OutputItem>,
    },
    /// Response has ended (stream complete)
    ResponseDone {
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<ResponseStatus>,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
    /// An error occurred during streaming
    Error { code: String, message: String },
}

/// Header for an output item at stream start (before content arrives)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputItemHeader {
    Message {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        role: Role,
    },
    FunctionCall {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        call_id: String,
    },
    Reasoning {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
}

/// Payload for a content delta event
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentDeltaPayload {
    /// Text content delta
    Text { text: String },
    /// Tool call delta (within an `OutputItem::Message` that contains tool calls)
    ToolCall(ToolCallDelta),
    /// Thinking/reasoning delta
    Thinking {
        #[serde(skip_serializing_if = "Option::is_none")]
        thinking: Option<String>,
    },
    /// Reasoning summary delta
    ReasoningSummary { text: String },
    /// Model refusal delta
    Refusal { refusal: String },
    /// Function call arguments delta (for `OutputItem::FunctionCall` streaming)
    FunctionCallArguments {
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        arguments: String,
    },
}

/// Incremental tool call data
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCallDelta {
    pub tool_call_index: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::models::message::{AnnotatedContentPart, ContentPart};
    use crate::shared::models::response::{FinishReason, ItemStatus, OutputItem};

    #[test]
    fn response_created_round_trip() {
        let event = StreamEvent::ResponseCreated {
            id: Some("resp-123".to_string()),
            model: Some("gpt-oss:120b".to_string()),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""event":"response_created""#));
        let round_tripped: StreamEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, round_tripped);
    }

    #[test]
    fn output_item_start_message() {
        let event = StreamEvent::OutputItemStart {
            item_index: 0,
            item: OutputItemHeader::Message {
                id: Some("msg_1".to_string()),
                role: Role::Assistant,
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""event":"output_item_start""#));
        assert!(json.contains(r#""type":"message""#));
        let round_tripped: StreamEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, round_tripped);
    }

    #[test]
    fn output_item_start_function_call() {
        let event = StreamEvent::OutputItemStart {
            item_index: 1,
            item: OutputItemHeader::FunctionCall {
                id: Some("fc_1".to_string()),
                call_id: "call_123".to_string(),
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"function_call""#));
        let round_tripped: StreamEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, round_tripped);
    }

    #[test]
    fn output_item_start_reasoning() {
        let event = StreamEvent::OutputItemStart {
            item_index: 0,
            item: OutputItemHeader::Reasoning {
                id: Some("rs_1".to_string()),
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"reasoning""#));
        let round_tripped: StreamEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, round_tripped);
    }

    #[test]
    fn content_delta_text() {
        let event = StreamEvent::ContentDelta {
            item_index: 0,
            content_index: 0,
            delta: ContentDeltaPayload::Text {
                text: "Hello".to_string(),
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""event":"content_delta""#));
        assert!(json.contains(r#""type":"text""#));
        let round_tripped: StreamEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, round_tripped);
    }

    #[test]
    fn content_delta_tool_call() {
        let event = StreamEvent::ContentDelta {
            item_index: 0,
            content_index: 1,
            delta: ContentDeltaPayload::ToolCall(ToolCallDelta {
                tool_call_index: 0,
                id: Some("call_1".to_string()),
                name: Some("get_weather".to_string()),
                arguments: Some(r#"{"ci"#.to_string()),
            }),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"tool_call""#));
        let round_tripped: StreamEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, round_tripped);
    }

    #[test]
    fn content_delta_function_call_arguments() {
        let event = StreamEvent::ContentDelta {
            item_index: 1,
            content_index: 0,
            delta: ContentDeltaPayload::FunctionCallArguments {
                name: Some("get_weather".to_string()),
                arguments: r#"{"city"#.to_string(),
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"function_call_arguments""#));
        let round_tripped: StreamEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, round_tripped);
    }

    #[test]
    fn content_done() {
        let event = StreamEvent::ContentDone {
            item_index: 0,
            content_index: 0,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""event":"content_done""#));
        let round_tripped: StreamEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, round_tripped);
    }

    #[test]
    fn output_item_done_without_item() {
        let event = StreamEvent::OutputItemDone {
            item_index: 0,
            item: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""event":"output_item_done""#));
        let round_tripped: StreamEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, round_tripped);
    }

    #[test]
    fn output_item_done_with_item() {
        let event = StreamEvent::OutputItemDone {
            item_index: 0,
            item: Some(OutputItem::Message {
                id: Some("msg_1".to_string()),
                role: Role::Assistant,
                content: vec![AnnotatedContentPart::plain(ContentPart::Text {
                    text: "Hello world".to_string(),
                })],
                status: Some(ItemStatus::Completed),
                finish_reason: Some(FinishReason::Stop),
            }),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""event":"output_item_done""#));
        let round_tripped: StreamEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, round_tripped);
    }

    #[test]
    fn response_done_with_usage() {
        let event = StreamEvent::ResponseDone {
            status: Some(ResponseStatus::Completed),
            usage: Some(Usage {
                prompt_tokens: Some(10),
                completion_tokens: Some(20),
                total_tokens: Some(30),
                ..Default::default()
            }),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""event":"response_done""#));
        let round_tripped: StreamEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, round_tripped);
    }

    #[test]
    fn error_event() {
        let event = StreamEvent::Error {
            code: "rate_limit".to_string(),
            message: "Too many requests".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""event":"error""#));
        let round_tripped: StreamEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, round_tripped);
    }

    #[test]
    fn full_item_aware_lifecycle() {
        let events = vec![
            StreamEvent::ResponseCreated {
                id: Some("resp-1".to_string()),
                model: Some("gpt-oss:120b".to_string()),
            },
            StreamEvent::OutputItemStart {
                item_index: 0,
                item: OutputItemHeader::Message {
                    id: Some("msg_1".to_string()),
                    role: Role::Assistant,
                },
            },
            StreamEvent::ContentDelta {
                item_index: 0,
                content_index: 0,
                delta: ContentDeltaPayload::Text {
                    text: "Hello".to_string(),
                },
            },
            StreamEvent::ContentDelta {
                item_index: 0,
                content_index: 0,
                delta: ContentDeltaPayload::Text {
                    text: " world".to_string(),
                },
            },
            StreamEvent::ContentDone {
                item_index: 0,
                content_index: 0,
            },
            StreamEvent::OutputItemDone {
                item_index: 0,
                item: Some(OutputItem::Message {
                    id: Some("msg_1".to_string()),
                    role: Role::Assistant,
                    content: vec![AnnotatedContentPart::plain(ContentPart::Text {
                        text: "Hello world".to_string(),
                    })],
                    status: Some(ItemStatus::Completed),
                    finish_reason: Some(FinishReason::Stop),
                }),
            },
            StreamEvent::ResponseDone {
                status: Some(ResponseStatus::Completed),
                usage: Some(Usage {
                    prompt_tokens: Some(5),
                    completion_tokens: Some(2),
                    total_tokens: Some(7),
                    ..Default::default()
                }),
            },
        ];

        for event in &events {
            let json = serde_json::to_string(event).unwrap();
            let round_tripped: StreamEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(*event, round_tripped);
        }
    }

    #[test]
    fn tool_call_delta_accumulation() {
        let deltas = vec![
            ToolCallDelta {
                tool_call_index: 0,
                id: Some("call_1".to_string()),
                name: Some("search".to_string()),
                arguments: None,
            },
            ToolCallDelta {
                tool_call_index: 0,
                id: None,
                name: None,
                arguments: Some(r#"{"q"#.to_string()),
            },
            ToolCallDelta {
                tool_call_index: 0,
                id: None,
                name: None,
                arguments: Some(r#"uery":"test"}"#.to_string()),
            },
        ];

        let mut id = String::new();
        let mut name = String::new();
        let mut args = String::new();
        for d in &deltas {
            if let Some(ref v) = d.id {
                id = v.clone();
            }
            if let Some(ref v) = d.name {
                name = v.clone();
            }
            if let Some(ref v) = d.arguments {
                args.push_str(v);
            }
        }
        assert_eq!(id, "call_1");
        assert_eq!(name, "search");
        assert_eq!(args, r#"{"query":"test"}"#);
    }

    #[test]
    fn content_delta_thinking() {
        let event = StreamEvent::ContentDelta {
            item_index: 0,
            content_index: 0,
            delta: ContentDeltaPayload::Thinking {
                thinking: Some("Let me think...".to_string()),
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"thinking""#));
        let round_tripped: StreamEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, round_tripped);
    }

    #[test]
    fn content_delta_refusal() {
        let event = StreamEvent::ContentDelta {
            item_index: 0,
            content_index: 0,
            delta: ContentDeltaPayload::Refusal {
                refusal: "I cannot".to_string(),
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"refusal""#));
        let round_tripped: StreamEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, round_tripped);
    }
}
