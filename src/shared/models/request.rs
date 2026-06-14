//! LLM request types
//!
//! Unified request representations for chat completions, embeddings,
//! and text completions that map to all supported provider formats.
//! Uses an item-based input model (inspired by the Responses API) as the
//! provider-agnostic superset.

use serde::{Deserialize, Serialize};

use super::message::{AnnotatedContentPart, Content, ContentPart, Message, Role};
use super::metadata::RequestMetadata;
use super::tools::{ToolChoice, ToolDefinition};

/// Top-level LLM request envelope
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "request_type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum LlmRequest {
    Chat(ChatRequest),
    Embedding(EmbeddingRequest),
    Completion(CompletionRequest),
}

impl LlmRequest {
    /// Get the model name for this request
    #[must_use]
    pub fn model(&self) -> &str {
        match self {
            Self::Chat(r) => &r.model,
            Self::Embedding(r) => &r.model,
            Self::Completion(r) => &r.model,
        }
    }

    /// Check if this request is a streaming request
    #[must_use]
    pub fn is_stream(&self) -> bool {
        match self {
            Self::Chat(r) => r.stream.unwrap_or(false),
            Self::Embedding(_) => false,
            Self::Completion(r) => r.stream.unwrap_or(false),
        }
    }
}

/// A single input item in the conversation (item-based model)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputItem {
    /// A conversation message
    Message {
        role: Role,
        content: Content,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        extensions: Option<serde_json::Value>,
    },
    /// Result of a function call (Responses API style)
    FunctionCallOutput { call_id: String, output: String },
    /// Reference to a previous item by ID
    ItemReference { id: String },
}

impl From<Message> for InputItem {
    fn from(msg: Message) -> Self {
        // Tool result messages map to FunctionCallOutput when they have a tool_call_id
        if msg.role == Role::Tool
            && let Some(call_id) = msg.tool_call_id
        {
            return Self::FunctionCallOutput {
                call_id,
                output: msg.content.text(),
            };
        }
        Self::Message {
            role: msg.role,
            content: msg.content,
            name: msg.name,
            extensions: msg.extensions,
        }
    }
}

/// System prompt / instructions content — supports both plain text and annotated blocks
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum InstructionContent {
    Text(String),
    Blocks(Vec<AnnotatedContentPart>),
}

impl InstructionContent {
    /// Extract the text from instructions
    #[must_use]
    pub fn text(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::Blocks(parts) => parts
                .iter()
                .filter_map(|ap| match &ap.part {
                    ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
        }
    }
}

/// Session configuration for stateful conversations (Responses API)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionConfig {
    /// Reference to a previous response for multi-turn
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
    /// Whether to store the response for future reference
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
}

/// How to truncate conversation history when it exceeds context
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TruncationStrategy {
    Auto,
    LastNTurns { n: u32 },
    Disabled,
}

/// A chat completion request
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatRequest {
    pub model: String,
    pub input: Vec<InputItem>,
    /// System prompt (Anthropic `system` / Responses API `instructions`)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<InstructionContent>,

    #[serde(flatten)]
    pub params: GenerationParams,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDefinition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ReasoningConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<RequestMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<TruncationStrategy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    /// Provider-specific passthrough data
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<serde_json::Value>,
}

impl ChatRequest {
    /// Create a simple chat request with a single user message
    #[must_use]
    pub fn simple(model: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            input: vec![InputItem::Message {
                role: Role::User,
                content: Content::Text(message.into()),
                name: None,
                extensions: None,
            }],
            instructions: None,
            params: GenerationParams::default(),
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            response_format: None,
            stream: None,
            thinking: None,
            metadata: None,
            session: None,
            truncation: None,
            background: None,
            service_tier: None,
            extensions: None,
        }
    }

    /// Create a chat request from a Vec<Message> (convenience for Chat Completions style)
    #[must_use]
    pub fn from_messages(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            input: messages.into_iter().map(InputItem::from).collect(),
            instructions: None,
            params: GenerationParams::default(),
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            response_format: None,
            stream: None,
            thinking: None,
            metadata: None,
            session: None,
            truncation: None,
            background: None,
            service_tier: None,
            extensions: None,
        }
    }

    /// Get the system prompt from the `instructions` field or first system message input item
    #[must_use]
    pub fn system_prompt(&self) -> Option<String> {
        if let Some(ref instructions) = self.instructions {
            return Some(instructions.text());
        }
        self.input.iter().find_map(|item| match item {
            InputItem::Message {
                role: Role::System,
                content,
                ..
            } => Some(content.text()),
            _ => None,
        })
    }

    /// Get the number of input items
    #[must_use]
    pub fn input_count(&self) -> usize {
        self.input.len()
    }

    /// Check if this request includes tool definitions
    #[must_use]
    pub fn has_tools(&self) -> bool {
        self.tools.as_ref().is_some_and(|t| !t.is_empty())
    }

    /// Check if any input items contain multimodal content
    #[must_use]
    pub fn is_multimodal(&self) -> bool {
        self.input.iter().any(|item| match item {
            InputItem::Message { content, .. } => match content {
                Content::Parts(parts) => parts.iter().any(|ap| {
                    matches!(
                        ap.part,
                        ContentPart::Image { .. }
                            | ContentPart::Audio { .. }
                            | ContentPart::Document { .. }
                            | ContentPart::Video { .. }
                    )
                }),
                Content::Text(_) => false,
            },
            _ => false,
        })
    }
}

/// Common generation parameters shared across request types
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GenerationParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Top-k sampling (Anthropic, Ollama)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
}

/// Configuration for model reasoning/thinking
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReasoningConfig {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<ReasoningEffort>,
    /// Anthropic thinking budget in tokens
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
    /// `OpenAI` reasoning summary mode
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<ReasoningSummary>,
}

/// Reasoning effort level (`OpenAI`)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
}

/// Reasoning summary mode (`OpenAI`)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningSummary {
    Concise,
    Detailed,
    Auto,
}

/// Desired response format
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseFormat {
    Text,
    JsonObject,
    JsonSchema {
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        schema: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        strict: Option<bool>,
    },
}

/// Input for embedding requests
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum EmbeddingInput {
    Single(String),
    Multiple(Vec<String>),
}

/// An embedding request
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EmbeddingRequest {
    pub model: String,
    pub input: EmbeddingInput,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding_format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<serde_json::Value>,
}

/// A text completion request (legacy / Ollama generate)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompletionRequest {
    pub model: String,
    pub prompt: String,
    #[serde(flatten)]
    pub params: GenerationParams,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::models::media::MediaSource;
    use crate::shared::models::message::{
        AnnotatedContentPart, CacheControl, Content, ContentPart, Role,
    };
    use crate::shared::models::tools::{FunctionDefinition, ToolChoice};

    #[test]
    fn chat_request_simple() {
        let req = ChatRequest::simple("gpt-oss:120b", "Hello");
        assert_eq!(req.model, "gpt-oss:120b");
        assert_eq!(req.input_count(), 1);
        match &req.input[0] {
            InputItem::Message { role, content, .. } => {
                assert_eq!(*role, Role::User);
                assert_eq!(content.text(), "Hello");
            }
            _ => panic!("expected InputItem::Message"),
        }
        assert!(!req.has_tools());
        assert!(!req.is_multimodal());
    }

    #[test]
    fn chat_request_from_messages() {
        let req = ChatRequest::from_messages(
            "gpt-oss:120b",
            vec![Message::system("Be helpful"), Message::user("Hi")],
        );
        assert_eq!(req.input_count(), 2);
        assert_eq!(req.system_prompt(), Some("Be helpful".to_string()));
    }

    #[test]
    fn chat_request_from_messages_tool_result() {
        let req = ChatRequest::from_messages(
            "gpt-oss:120b",
            vec![Message::tool_result("call_1", r#"{"result": 42}"#)],
        );
        assert_eq!(req.input_count(), 1);
        match &req.input[0] {
            InputItem::FunctionCallOutput { call_id, output } => {
                assert_eq!(call_id, "call_1");
                assert_eq!(output, r#"{"result": 42}"#);
            }
            _ => panic!("expected InputItem::FunctionCallOutput"),
        }
    }

    #[test]
    fn chat_request_full_round_trip() {
        let req = ChatRequest {
            model: "claude-3-opus".to_string(),
            input: vec![
                InputItem::Message {
                    role: Role::System,
                    content: Content::Text("Be helpful".to_string()),
                    name: None,
                    extensions: None,
                },
                InputItem::Message {
                    role: Role::User,
                    content: Content::Text("Hi".to_string()),
                    name: None,
                    extensions: None,
                },
            ],
            instructions: Some(InstructionContent::Text("Be concise".to_string())),
            params: GenerationParams {
                temperature: Some(0.7),
                top_p: Some(0.9),
                top_k: Some(40),
                max_tokens: Some(1024),
                stop: Some(vec!["END".to_string()]),
                frequency_penalty: Some(0.5),
                presence_penalty: Some(0.5),
                seed: Some(42),
            },
            tools: Some(vec![ToolDefinition::Function {
                function: FunctionDefinition {
                    name: "search".to_string(),
                    description: Some("Search the web".to_string()),
                    parameters: Some(serde_json::json!({"type": "object"})),
                    strict: None,
                },
            }]),
            tool_choice: Some(ToolChoice::Auto),
            parallel_tool_calls: Some(true),
            response_format: Some(ResponseFormat::JsonObject),
            stream: Some(false),
            thinking: Some(ReasoningConfig {
                enabled: true,
                effort: Some(ReasoningEffort::High),
                budget_tokens: Some(4096),
                summary: Some(ReasoningSummary::Concise),
            }),
            metadata: None,
            session: Some(SessionConfig {
                previous_response_id: Some("resp_abc".to_string()),
                store: Some(true),
            }),
            truncation: Some(TruncationStrategy::Auto),
            background: None,
            service_tier: Some("default".to_string()),
            extensions: Some(serde_json::json!({"provider_specific": true})),
        };
        let json = serde_json::to_string(&req).unwrap();
        let deserialized: ChatRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, deserialized);
    }

    #[test]
    fn chat_request_minimal_serde() {
        let json =
            r#"{"model":"gpt-oss:120b","input":[{"type":"message","role":"user","content":"hi"}]}"#;
        let req: ChatRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.model, "gpt-oss:120b");
        assert_eq!(req.input_count(), 1);
        assert!(req.params.temperature.is_none());
        assert!(req.tools.is_none());
    }

    #[test]
    fn generation_params_flatten() {
        let req = ChatRequest::simple("model", "hi");
        let json = serde_json::to_string(&req).unwrap();
        // Flattened params should not appear as a nested object
        assert!(!json.contains(r#""params""#));
    }

    #[test]
    fn llm_request_model_across_variants() {
        let chat = LlmRequest::Chat(ChatRequest::simple("gpt-oss:120b", "hi"));
        assert_eq!(chat.model(), "gpt-oss:120b");

        let embed = LlmRequest::Embedding(EmbeddingRequest {
            model: "text-embedding-3".to_string(),
            input: EmbeddingInput::Single("test".to_string()),
            encoding_format: None,
            dimensions: None,
            extensions: None,
        });
        assert_eq!(embed.model(), "text-embedding-3");

        let completion = LlmRequest::Completion(CompletionRequest {
            model: "llama3".to_string(),
            prompt: "Once upon".to_string(),
            params: GenerationParams::default(),
            stream: None,
            extensions: None,
        });
        assert_eq!(completion.model(), "llama3");
    }

    #[test]
    fn llm_request_is_stream() {
        let req = LlmRequest::Chat(ChatRequest {
            stream: Some(true),
            ..ChatRequest::simple("m", "hi")
        });
        assert!(req.is_stream());

        let req = LlmRequest::Embedding(EmbeddingRequest {
            model: "m".to_string(),
            input: EmbeddingInput::Single("t".to_string()),
            encoding_format: None,
            dimensions: None,
            extensions: None,
        });
        assert!(!req.is_stream());
    }

    #[test]
    fn has_tools() {
        let mut req = ChatRequest::simple("m", "hi");
        assert!(!req.has_tools());

        req.tools = Some(vec![]);
        assert!(!req.has_tools());

        req.tools = Some(vec![ToolDefinition::Function {
            function: FunctionDefinition {
                name: "f".to_string(),
                description: None,
                parameters: None,
                strict: None,
            },
        }]);
        assert!(req.has_tools());
    }

    #[test]
    fn is_multimodal() {
        let text_req = ChatRequest::simple("m", "hi");
        assert!(!text_req.is_multimodal());

        let mm_req = ChatRequest {
            input: vec![InputItem::Message {
                role: Role::User,
                content: Content::Parts(vec![
                    AnnotatedContentPart::plain(ContentPart::Text {
                        text: "Describe".to_string(),
                    }),
                    AnnotatedContentPart::plain(ContentPart::Image {
                        source: MediaSource::Url {
                            url: "https://example.com/img.png".to_string(),
                        },
                        detail: None,
                    }),
                ]),
                name: None,
                extensions: None,
            }],
            ..ChatRequest::simple("m", "")
        };
        assert!(mm_req.is_multimodal());
    }

    #[test]
    fn response_format_variants() {
        let text = ResponseFormat::Text;
        let json_str = serde_json::to_string(&text).unwrap();
        assert!(json_str.contains(r#""type":"text""#));

        let json_obj = ResponseFormat::JsonObject;
        let json_str = serde_json::to_string(&json_obj).unwrap();
        assert!(json_str.contains(r#""type":"json_object""#));

        let json_schema = ResponseFormat::JsonSchema {
            name: Some("person".to_string()),
            schema: serde_json::json!({"type": "object"}),
            strict: Some(true),
        };
        let json_str = serde_json::to_string(&json_schema).unwrap();
        assert!(json_str.contains(r#""type":"json_schema""#));

        let round_tripped: ResponseFormat = serde_json::from_str(&json_str).unwrap();
        assert_eq!(json_schema, round_tripped);
    }

    #[test]
    fn thinking_config_round_trip() {
        let config = ReasoningConfig {
            enabled: true,
            effort: Some(ReasoningEffort::Medium),
            budget_tokens: Some(8192),
            summary: Some(ReasoningSummary::Detailed),
        };
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: ReasoningConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, deserialized);
    }

    #[test]
    fn embedding_input_untagged() {
        let single: EmbeddingInput = serde_json::from_str(r#""hello""#).unwrap();
        assert_eq!(single, EmbeddingInput::Single("hello".to_string()));

        let multi: EmbeddingInput = serde_json::from_str(r#"["a","b"]"#).unwrap();
        assert_eq!(
            multi,
            EmbeddingInput::Multiple(vec!["a".to_string(), "b".to_string()])
        );
    }

    #[test]
    fn input_item_message_serde() {
        let item = InputItem::Message {
            role: Role::User,
            content: Content::Text("hello".to_string()),
            name: None,
            extensions: None,
        };
        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains(r#""type":"message""#));
        let round_tripped: InputItem = serde_json::from_str(&json).unwrap();
        assert_eq!(item, round_tripped);
    }

    #[test]
    fn input_item_function_call_output_serde() {
        let item = InputItem::FunctionCallOutput {
            call_id: "call_123".to_string(),
            output: r#"{"temp": 72}"#.to_string(),
        };
        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains(r#""type":"function_call_output""#));
        let round_tripped: InputItem = serde_json::from_str(&json).unwrap();
        assert_eq!(item, round_tripped);
    }

    #[test]
    fn input_item_reference_serde() {
        let item = InputItem::ItemReference {
            id: "item_abc".to_string(),
        };
        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains(r#""type":"item_reference""#));
        let round_tripped: InputItem = serde_json::from_str(&json).unwrap();
        assert_eq!(item, round_tripped);
    }

    #[test]
    fn instruction_content_untagged_text() {
        let ic: InstructionContent = serde_json::from_str(r#""Be helpful""#).unwrap();
        assert_eq!(ic.text(), "Be helpful");
    }

    #[test]
    fn instruction_content_untagged_blocks() {
        let ic: InstructionContent =
            serde_json::from_str(r#"[{"type":"text","text":"Be helpful"}]"#).unwrap();
        assert_eq!(ic.text(), "Be helpful");
    }

    #[test]
    fn instruction_content_blocks_with_cache_control() {
        let ic = InstructionContent::Blocks(vec![AnnotatedContentPart::with_cache_control(
            ContentPart::Text {
                text: "Cached system prompt".to_string(),
            },
            CacheControl::Ephemeral,
        )]);
        let json = serde_json::to_string(&ic).unwrap();
        assert!(json.contains("cache_control"));
        assert_eq!(ic.text(), "Cached system prompt");
    }

    #[test]
    fn session_config_round_trip() {
        let config = SessionConfig {
            previous_response_id: Some("resp_abc".to_string()),
            store: Some(true),
        };
        let json = serde_json::to_string(&config).unwrap();
        let round_tripped: SessionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, round_tripped);
    }

    #[test]
    fn truncation_strategy_variants() {
        let auto = TruncationStrategy::Auto;
        let json = serde_json::to_string(&auto).unwrap();
        assert!(json.contains(r#""type":"auto""#));
        let round_tripped: TruncationStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(auto, round_tripped);

        let last_n = TruncationStrategy::LastNTurns { n: 10 };
        let json = serde_json::to_string(&last_n).unwrap();
        assert!(json.contains(r#""type":"last_n_turns""#));
        let round_tripped: TruncationStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(last_n, round_tripped);

        let disabled = TruncationStrategy::Disabled;
        let json = serde_json::to_string(&disabled).unwrap();
        assert!(json.contains(r#""type":"disabled""#));
        let round_tripped: TruncationStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(disabled, round_tripped);
    }

    #[test]
    fn input_item_from_message() {
        let msg = Message::user("hello");
        let item = InputItem::from(msg);
        match item {
            InputItem::Message { role, content, .. } => {
                assert_eq!(role, Role::User);
                assert_eq!(content.text(), "hello");
            }
            _ => panic!("expected Message variant"),
        }
    }

    #[test]
    fn input_item_from_tool_message() {
        let msg = Message::tool_result("call_1", "result");
        let item = InputItem::from(msg);
        match item {
            InputItem::FunctionCallOutput { call_id, output } => {
                assert_eq!(call_id, "call_1");
                assert_eq!(output, "result");
            }
            _ => panic!("expected FunctionCallOutput variant"),
        }
    }

    #[test]
    fn system_prompt_from_instructions() {
        let req = ChatRequest {
            instructions: Some(InstructionContent::Text("Be concise".to_string())),
            ..ChatRequest::simple("m", "hi")
        };
        assert_eq!(req.system_prompt(), Some("Be concise".to_string()));
    }

    #[test]
    fn system_prompt_from_input_item() {
        let req = ChatRequest {
            input: vec![
                InputItem::Message {
                    role: Role::System,
                    content: Content::Text("Be helpful".to_string()),
                    name: None,
                    extensions: None,
                },
                InputItem::Message {
                    role: Role::User,
                    content: Content::Text("Hi".to_string()),
                    name: None,
                    extensions: None,
                },
            ],
            ..ChatRequest::simple("m", "")
        };
        assert_eq!(req.system_prompt(), Some("Be helpful".to_string()));
    }
}
