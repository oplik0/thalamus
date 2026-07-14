use serde::{Deserialize, Serialize};

use crate::shared::models::{
    ChatRequest, ChatResponse, Content, FinishReason, FunctionDefinition, GenerationParams,
    InputItem, InstructionContent, LlmRequest, OutputItem, Role, StreamEvent, ToolChoice,
    ToolDefinition,
};

#[derive(Debug, Deserialize)]
pub struct AnthropicMessagesRequest {
    pub model: String,
    pub messages: Vec<AnthropicInputMessage>,
    #[serde(default)]
    pub system: Option<String>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub top_k: Option<u32>,
    #[serde(default)]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(default)]
    pub tools: Option<Vec<AnthropicToolDef>>,
    #[serde(default)]
    pub tool_choice: Option<AnthropicToolChoice>,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct AnthropicInputMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct AnthropicToolDef {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub input_schema: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct AnthropicToolChoice {
    #[serde(rename = "type")]
    pub choice_type: String,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AnthropicMessagesResponse {
    pub id: String,
    #[serde(rename = "type")]
    pub response_type: String,
    pub role: String,
    pub content: Vec<AnthropicContentBlock>,
    pub model: String,
    pub stop_reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<crate::shared::models::Usage>,
}

#[derive(Debug, Serialize)]
pub struct AnthropicContentBlock {
    #[serde(rename = "type")]
    pub block_type: String,
    pub text: String,
}

/// An Anthropic SSE streaming event.
///
/// Each variant maps to one of Anthropic's SSE event types. The variant name
/// determines both the `event:` SSE header and the `"type"` JSON field via
/// serde's `tag = "type"` + `rename_all`.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicStreamEvent {
    MessageStart {
        message: MessageStartPayload,
    },
    ContentBlockStart {
        index: u32,
        content_block: ContentBlock,
    },
    ContentBlockDelta {
        index: u32,
        delta: ContentDelta,
    },
    ContentBlockStop {
        index: u32,
    },
    MessageDelta {
        delta: MessageDeltaBody,
        usage: MessageDeltaUsage,
    },
    MessageStop {},
    Error {
        error: AnthropicError,
    },
}

impl AnthropicStreamEvent {
    /// Returns the SSE `event:` header value for this event.
    #[must_use]
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::MessageStart { .. } => "message_start",
            Self::ContentBlockStart { .. } => "content_block_start",
            Self::ContentBlockDelta { .. } => "content_block_delta",
            Self::ContentBlockStop { .. } => "content_block_stop",
            Self::MessageDelta { .. } => "message_delta",
            Self::MessageStop { .. } => "message_stop",
            Self::Error { .. } => "error",
        }
    }
}

// ── Payload structs ──────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct MessageStartPayload {
    pub id: String,
    #[serde(rename = "type")]
    pub message_type: &'static str,
    pub role: &'static str,
    pub content: Vec<()>,
    pub model: String,
    pub stop_reason: Option<String>,
    pub stop_sequence: Option<String>,
}

/// Content block header at `content_block_start`.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    Thinking {
        thinking: String,
    },
}

/// Incremental delta inside `content_block_delta`.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentDelta {
    TextDelta { text: String },
    InputJsonDelta { partial_json: String },
    ThinkingDelta { thinking: String },
}

#[derive(Debug, Serialize)]
pub struct MessageDeltaBody {
    pub stop_reason: String,
    pub stop_sequence: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct MessageDeltaUsage {
    pub output_tokens: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct AnthropicError {
    #[serde(rename = "type")]
    pub error_type: String,
    pub message: String,
}

impl From<AnthropicMessagesRequest> for LlmRequest {
    fn from(value: AnthropicMessagesRequest) -> Self {
        let input = value
            .messages
            .into_iter()
            .map(|message| InputItem::Message {
                role: match message.role.as_str() {
                    "assistant" => Role::Assistant,
                    _ => Role::User,
                },
                content: Content::Text(message.content),
                name: None,
                extensions: None,
            })
            .collect();

        let tools = value.tools.map(|tools| {
            tools
                .into_iter()
                .map(|t| ToolDefinition::Function {
                    function: FunctionDefinition {
                        name: t.name,
                        description: t.description,
                        parameters: t.input_schema,
                        strict: None,
                    },
                })
                .collect()
        });

        let tool_choice = value.tool_choice.map(|tc| match tc.choice_type.as_str() {
            "auto" => ToolChoice::Auto,
            "any" => ToolChoice::Required,
            "tool" => {
                if let Some(name) = tc.name {
                    ToolChoice::Function { name }
                } else {
                    tracing::warn!(
                        "Anthropic tool_choice type='tool' received with no tool name; \
                     falling back to ToolChoice::Auto"
                    );
                    ToolChoice::Auto
                }
            }
            _ => ToolChoice::Auto,
        });

        LlmRequest::Chat(ChatRequest {
            model: value.model,
            input,
            instructions: value.system.map(InstructionContent::Text),
            params: GenerationParams {
                temperature: value.temperature,
                top_p: value.top_p,
                top_k: value.top_k,
                max_tokens: value.max_tokens,
                stop: value.stop_sequences,
                ..GenerationParams::default()
            },
            tools,
            tool_choice,
            parallel_tool_calls: None,
            response_format: None,
            stream: Some(value.stream),
            thinking: None,
            metadata: None,
            session: None,
            truncation: None,
            background: None,
            service_tier: None,
            extensions: value.metadata,
        })
    }
}

impl From<ChatResponse> for AnthropicMessagesResponse {
    fn from(value: ChatResponse) -> Self {
        let text = value
            .output
            .iter()
            .find_map(OutputItem::text)
            .unwrap_or_default();

        let stop_reason = value
            .output
            .iter()
            .find_map(|item| match item {
                OutputItem::Message { finish_reason, .. } => finish_reason.as_ref(),
                _ => None,
            })
            .map_or_else(|| "end_turn".to_string(), finish_reason_to_anthropic);

        Self {
            id: value
                .id
                .unwrap_or_else(|| format!("msg_{}", uuid::Uuid::new_v4().simple())),
            response_type: "message".to_string(),
            role: "assistant".to_string(),
            content: vec![AnthropicContentBlock {
                block_type: "text".to_string(),
                text,
            }],
            model: value.model,
            stop_reason,
            usage: value.usage,
        }
    }
}

fn finish_reason_to_anthropic(reason: &FinishReason) -> String {
    match reason {
        FinishReason::Stop => "end_turn".to_string(),
        FinishReason::Length => "max_tokens".to_string(),
        FinishReason::ToolCalls => "tool_use".to_string(),
        FinishReason::ContentFilter => "content_filter".to_string(),
    }
}

/// Convert a unified `StreamEvent` to Anthropic SSE events.
/// Returns a Vec because some events map to multiple SSE events
/// (e.g., `ResponseDone` -> `MessageDelta` + `MessageStop`).
#[must_use]
pub fn stream_event_to_anthropic(value: StreamEvent) -> Vec<AnthropicStreamEvent> {
    match value {
        StreamEvent::ResponseCreated { id, model } => {
            vec![AnthropicStreamEvent::MessageStart {
                message: MessageStartPayload {
                    id: id.unwrap_or_else(|| format!("msg_{}", uuid::Uuid::new_v4().simple())),
                    message_type: "message",
                    role: "assistant",
                    content: vec![],
                    model: model.unwrap_or_else(|| "unknown".to_string()),
                    stop_reason: None,
                    stop_sequence: None,
                },
            }]
        }
        StreamEvent::OutputItemStart { item_index, item } => {
            let content_block = match item {
                crate::shared::models::OutputItemHeader::Message { .. } => ContentBlock::Text {
                    text: String::new(),
                },
                crate::shared::models::OutputItemHeader::FunctionCall { call_id, .. } => {
                    ContentBlock::ToolUse {
                        id: call_id,
                        name: String::new(),
                        input: serde_json::json!({}),
                    }
                }
                crate::shared::models::OutputItemHeader::Reasoning { .. } => {
                    ContentBlock::Thinking {
                        thinking: String::new(),
                    }
                }
            };
            vec![AnthropicStreamEvent::ContentBlockStart {
                index: item_index,
                content_block,
            }]
        }
        StreamEvent::ContentDelta {
            content_index,
            delta,
            ..
        } => {
            let delta = match delta {
                crate::shared::models::ContentDeltaPayload::Text { text } => {
                    ContentDelta::TextDelta { text }
                }
                crate::shared::models::ContentDeltaPayload::ToolCall(tc) => {
                    ContentDelta::InputJsonDelta {
                        partial_json: tc.arguments.unwrap_or_default(),
                    }
                }
                crate::shared::models::ContentDeltaPayload::FunctionCallArguments {
                    arguments,
                    ..
                } => ContentDelta::InputJsonDelta {
                    partial_json: arguments,
                },
                crate::shared::models::ContentDeltaPayload::Thinking { thinking } => {
                    ContentDelta::ThinkingDelta {
                        thinking: thinking.unwrap_or_default(),
                    }
                }
                crate::shared::models::ContentDeltaPayload::ReasoningSummary { text } => {
                    ContentDelta::TextDelta { text }
                }
                crate::shared::models::ContentDeltaPayload::Refusal { refusal } => {
                    ContentDelta::TextDelta { text: refusal }
                }
            };
            vec![AnthropicStreamEvent::ContentBlockDelta {
                index: content_index,
                delta,
            }]
        }
        StreamEvent::ContentDone { content_index, .. } => {
            vec![AnthropicStreamEvent::ContentBlockStop {
                index: content_index,
            }]
        }
        StreamEvent::ResponseDone { status, usage } => {
            let stop_reason = match status {
                Some(crate::shared::models::ResponseStatus::Incomplete) => "max_tokens",
                _ => "end_turn",
            }
            .to_string();
            vec![
                AnthropicStreamEvent::MessageDelta {
                    delta: MessageDeltaBody {
                        stop_reason,
                        stop_sequence: None,
                    },
                    usage: MessageDeltaUsage {
                        output_tokens: usage.and_then(|u| u.completion_tokens),
                    },
                },
                AnthropicStreamEvent::MessageStop {},
            ]
        }
        StreamEvent::Error { code, message } => vec![AnthropicStreamEvent::Error {
            error: AnthropicError {
                error_type: code,
                message,
            },
        }],
        // OutputItemDone has no direct Anthropic SSE equivalent
        _ => vec![],
    }
}
