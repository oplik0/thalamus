use serde::{Deserialize, Serialize};

use crate::shared::models::{
    ChatRequest, ChatResponse, EmbeddingInput, EmbeddingRequest, FinishReason, GenerationParams,
    InputItem, LlmRequest, Message, OutputItem, StreamEvent,
};

#[derive(Debug, Deserialize)]
pub struct ChatCompletionsRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct ChatCompletionsResponse {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<ChatChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<crate::shared::models::Usage>,
}

#[derive(Debug, Serialize)]
pub struct ChatChoice {
    pub index: u32,
    pub message: ChatChoiceMessage,
    pub finish_reason: String,
}

#[derive(Debug, Serialize)]
pub struct ChatChoiceMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ResponseToolCall>>,
}

#[derive(Debug, Serialize)]
pub struct ResponseToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: ResponseToolFunction,
}

#[derive(Debug, Serialize)]
pub struct ResponseToolFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Serialize)]
pub struct ChatCompletionsChunk {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<ChunkChoice>,
}

#[derive(Debug, Serialize)]
pub struct ChunkChoice {
    pub index: u32,
    pub delta: ChunkDelta,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ChunkDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ChunkToolCall>>,
}

#[derive(Debug, Serialize)]
pub struct ChunkToolCall {
    pub index: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "type")]
    pub call_type: Option<String>,
    pub function: ChunkFunction,
}

#[derive(Debug, Serialize)]
pub struct ChunkFunction {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

impl From<ChatCompletionsRequest> for LlmRequest {
    fn from(value: ChatCompletionsRequest) -> Self {
        LlmRequest::Chat(ChatRequest {
            model: value.model,
            input: value.messages.into_iter().map(InputItem::from).collect(),
            instructions: None,
            params: GenerationParams {
                temperature: value.temperature,
                top_p: value.top_p,
                max_tokens: value.max_tokens,
                ..GenerationParams::default()
            },
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            response_format: None,
            stream: Some(value.stream),
            thinking: None,
            metadata: None,
            session: None,
            truncation: None,
            background: None,
            service_tier: None,
            extensions: None,
        })
    }
}

impl From<ChatResponse> for ChatCompletionsResponse {
    fn from(value: ChatResponse) -> Self {
        let content = value
            .output
            .iter()
            .find_map(crate::shared::models::response::OutputItem::text);

        let tool_calls: Vec<ResponseToolCall> = value
            .output
            .iter()
            .filter_map(|item| match item {
                OutputItem::FunctionCall {
                    call_id,
                    name,
                    arguments,
                    ..
                } => Some(ResponseToolCall {
                    id: call_id.clone(),
                    call_type: "function".to_string(),
                    function: ResponseToolFunction {
                        name: name.clone(),
                        arguments: arguments.clone(),
                    },
                }),
                _ => None,
            })
            .collect();

        let finish_reason = value
            .output
            .iter()
            .find_map(|item| match item {
                OutputItem::Message { finish_reason, .. } => {
                    finish_reason.as_ref().map(finish_reason_to_string)
                }
                _ => None,
            })
            .unwrap_or_else(|| {
                if tool_calls.is_empty() {
                    "stop".to_string()
                } else {
                    "tool_calls".to_string()
                }
            });

        Self {
            id: value
                .id
                .unwrap_or_else(|| format!("chatcmpl-{}", uuid::Uuid::new_v4())),
            object: "chat.completion".to_string(),
            created: chrono::Utc::now().timestamp(),
            model: value.model,
            choices: vec![ChatChoice {
                index: 0,
                message: ChatChoiceMessage {
                    role: "assistant".to_string(),
                    content,
                    tool_calls: if tool_calls.is_empty() {
                        None
                    } else {
                        Some(tool_calls)
                    },
                },
                finish_reason,
            }],
            usage: value.usage,
        }
    }
}

/// Stateful converter that carries the stream ID and model across chunks.
#[derive(Debug, Default)]
pub struct StreamChunkConverter {
    id: String,
    model: String,
    created: i64,
    /// Whether any tool-call deltas have been emitted in the current item,
    /// used to select `finish_reason` on `OutputItemDone`.
    has_tool_calls: bool,
}

impl StreamChunkConverter {
    fn chunk(&self, choices: Vec<ChunkChoice>) -> ChatCompletionsChunk {
        ChatCompletionsChunk {
            id: self.id.clone(),
            object: "chat.completion.chunk".to_string(),
            created: self.created,
            model: self.model.clone(),
            choices,
        }
    }

    /// Convert a `StreamEvent` to a `ChatCompletionsChunk`, updating internal state.
    /// Returns `None` for events that don't map to chunks.
    pub fn convert(&mut self, event: StreamEvent) -> Option<ChatCompletionsChunk> {
        match event {
            StreamEvent::ResponseCreated { id, model } => {
                self.id = id.unwrap_or_else(|| format!("chatcmpl-{}", uuid::Uuid::new_v4()));
                self.model = model.unwrap_or_else(|| "unknown".to_string());
                self.created = chrono::Utc::now().timestamp();
                self.has_tool_calls = false;
                Some(self.chunk(vec![ChunkChoice {
                    index: 0,
                    delta: ChunkDelta {
                        role: Some("assistant".to_string()),
                        content: None,
                        refusal: None,
                        tool_calls: None,
                    },
                    finish_reason: None,
                }]))
            }
            StreamEvent::ContentDelta { delta, .. } => {
                let chunk_delta = match delta {
                    crate::shared::models::ContentDeltaPayload::Text { text } => ChunkDelta {
                        role: None,
                        content: Some(text),
                        refusal: None,
                        tool_calls: None,
                    },
                    crate::shared::models::ContentDeltaPayload::ToolCall(tc) => {
                        self.has_tool_calls = true;
                        ChunkDelta {
                            role: None,
                            content: None,
                            refusal: None,
                            tool_calls: Some(vec![ChunkToolCall {
                                index: tc.tool_call_index,
                                id: tc.id,
                                call_type: Some("function".to_string()),
                                function: ChunkFunction {
                                    name: tc.name,
                                    arguments: tc.arguments,
                                },
                            }]),
                        }
                    }
                    crate::shared::models::ContentDeltaPayload::FunctionCallArguments {
                        name,
                        arguments,
                    } => {
                        self.has_tool_calls = true;
                        ChunkDelta {
                            role: None,
                            content: None,
                            refusal: None,
                            tool_calls: Some(vec![ChunkToolCall {
                                index: 0,
                                id: None,
                                call_type: Some("function".to_string()),
                                function: ChunkFunction {
                                    name,
                                    arguments: Some(arguments),
                                },
                            }]),
                        }
                    }
                    crate::shared::models::ContentDeltaPayload::Thinking { thinking } => {
                        thinking.map(|text| ChunkDelta {
                            role: None,
                            content: Some(text),
                            refusal: None,
                            tool_calls: None,
                        })?
                    }
                    crate::shared::models::ContentDeltaPayload::ReasoningSummary { text } => {
                        ChunkDelta {
                            role: None,
                            content: Some(text),
                            refusal: None,
                            tool_calls: None,
                        }
                    }
                    crate::shared::models::ContentDeltaPayload::Refusal { refusal } => ChunkDelta {
                        role: None,
                        content: None,
                        refusal: Some(refusal),
                        tool_calls: None,
                    },
                };

                Some(self.chunk(vec![ChunkChoice {
                    index: 0,
                    delta: chunk_delta,
                    finish_reason: None,
                }]))
            }
            StreamEvent::OutputItemDone { .. } => {
                let finish_reason = if self.has_tool_calls {
                    self.has_tool_calls = false;
                    "tool_calls".to_string()
                } else {
                    "stop".to_string()
                };
                Some(self.chunk(vec![ChunkChoice {
                    index: 0,
                    delta: ChunkDelta {
                        role: None,
                        content: None,
                        refusal: None,
                        tool_calls: None,
                    },
                    finish_reason: Some(finish_reason),
                }]))
            }
            _ => None,
        }
    }
}

fn finish_reason_to_string(reason: &FinishReason) -> String {
    match reason {
        FinishReason::Length => "length".to_string(),
        FinishReason::ToolCalls => "tool_calls".to_string(),
        FinishReason::ContentFilter => "content_filter".to_string(),
        FinishReason::Stop => "stop".to_string(),
    }
}

#[derive(Debug, Deserialize)]
pub struct OpenAiEmbeddingsRequest {
    pub model: String,
    pub input: serde_json::Value,
    #[serde(default)]
    pub encoding_format: Option<String>,
    #[serde(default)]
    pub dimensions: Option<u32>,
}

impl From<OpenAiEmbeddingsRequest> for EmbeddingRequest {
    fn from(value: OpenAiEmbeddingsRequest) -> Self {
        let input = if let Some(text) = value.input.as_str() {
            EmbeddingInput::Single(text.to_string())
        } else if let Some(array) = value.input.as_array() {
            EmbeddingInput::Multiple(
                array
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(ToString::to_string)
                    .collect(),
            )
        } else {
            EmbeddingInput::Single(value.input.to_string())
        };

        EmbeddingRequest {
            model: value.model,
            input,
            encoding_format: value.encoding_format,
            dimensions: value.dimensions,
            extensions: None,
        }
    }
}
