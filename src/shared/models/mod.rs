//! Unified LLM request and response models
//!
//! These types provide a provider-agnostic representation for LLM
//! conversations that supports lossless conversion to/from OpenAI,
//! Anthropic, Ollama, and other provider formats.
//!
//! Uses an item-based model (inspired by the Responses API) as the
//! canonical internal representation. Chat Completions and Anthropic
//! Messages map as degenerate cases of this richer model.

pub mod media;
pub mod message;
pub mod metadata;
pub mod request;
pub mod response;
pub mod stream;
pub mod tools;

// Re-export primary types for convenience
pub use media::{ImageDetail, MediaSource};
pub use message::{
    AnnotatedContentPart, BlockMetadata, CacheControl, Content, ContentPart, Message, Role,
    ToolCallPart,
};
pub use metadata::RequestMetadata;
pub use request::{
    ChatRequest, CompletionRequest, EmbeddingInput, EmbeddingRequest, GenerationParams, InputItem,
    InstructionContent, LlmRequest, ReasoningConfig, ReasoningEffort, ReasoningSummary,
    ResponseFormat, SessionConfig, TruncationStrategy,
};
pub use response::{ChatResponse, FinishReason, ItemStatus, OutputItem, ResponseStatus, Usage};
pub use stream::{ContentDeltaPayload, OutputItemHeader, StreamEvent, ToolCallDelta};
pub use tools::{FunctionDefinition, ServerTool, ToolChoice, ToolDefinition};
