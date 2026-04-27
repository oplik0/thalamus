use std::pin::Pin;
use std::time::Duration;

use bytes::Bytes;
use futures::{Stream, StreamExt};
use reqwest::{RequestBuilder, StatusCode};
use serde::{Deserialize, Serialize};

use crate::Error;
use crate::Result;
use crate::features::backends::domain::BackendAdapter;
use crate::shared::config::types::AuthConfig;
use crate::shared::models::{
    AnnotatedContentPart, ChatRequest, ChatResponse, ContentDeltaPayload, ContentPart,
    EmbeddingRequest, FinishReason, InputItem, ItemStatus, LlmRequest, OutputItem,
    OutputItemHeader, ResponseStatus, Role, StreamEvent, Usage,
};

#[derive(Debug)]
pub struct OllamaAdapter;

impl BackendAdapter for OllamaAdapter {
    fn name(&self) -> &str {
        "ollama"
    }

    fn build_request(
        &self,
        client: &reqwest::Client,
        endpoint_url: &str,
        request: &LlmRequest,
        auth: &AuthConfig,
        timeout: Duration,
    ) -> Result<reqwest::Request> {
        let chat = match request {
            LlmRequest::Chat(chat) => chat,
            _ => {
                return Err(Error::InvalidInput(
                    "Ollama chat endpoint requires a chat request".to_string(),
                ));
            }
        };

        let payload = to_ollama_request(chat);
        let url = format!("{}/api/chat", endpoint_url.trim_end_matches('/'));

        let builder = client.post(url).timeout(timeout).json(&payload);
        apply_backend_auth(builder, auth)
            .build()
            .map_err(Error::from)
    }

    fn parse_response(&self, status: StatusCode, body: &[u8]) -> Result<ChatResponse> {
        if !status.is_success() {
            let detail = String::from_utf8_lossy(body).to_string();
            return Err(Error::Backend(format!(
                "Backend returned status {}: {}",
                status.as_u16(),
                detail
            )));
        }

        let parsed: OllamaChatResponse = serde_json::from_slice(body)?;
        Ok(from_ollama_response(parsed))
    }

    fn parse_stream(
        &self,
        body_stream: Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>> {
        let stream = body_stream
            .scan(OllamaStreamState::default(), |state, item| {
                let mut events = Vec::new();
                match item {
                    Ok(chunk) => {
                        state.buffer.push_str(&String::from_utf8_lossy(&chunk));
                        while let Some(pos) = state.buffer.find('\n') {
                            let line = state.buffer[..pos].to_string();
                            state.buffer = state.buffer[pos + 1..].to_string();
                            if let Err(error) = parse_stream_frame(&line, state, &mut events) {
                                events.push(Err(error));
                            }
                        }
                    }
                    Err(error) => events.push(Err(error)),
                }
                std::future::ready(Some(events))
            })
            .flat_map(futures::stream::iter);

        Box::pin(stream)
    }

    fn build_embedding_request(
        &self,
        _client: &reqwest::Client,
        _endpoint_url: &str,
        _request: &EmbeddingRequest,
        _auth: &AuthConfig,
        _timeout: Duration,
    ) -> Result<reqwest::Request> {
        Err(Error::Backend(
            "Ollama does not support embeddings".to_string(),
        ))
    }

    fn parse_embedding_response(
        &self,
        _status: StatusCode,
        _body: &[u8],
    ) -> Result<serde_json::Value> {
        Err(Error::Backend(
            "Ollama does not support embeddings".to_string(),
        ))
    }
}

#[derive(Default)]
struct OllamaStreamState {
    buffer: String,
    sent_created: bool,
    started_message: bool,
}

fn parse_stream_frame(
    frame: &str,
    state: &mut OllamaStreamState,
    events: &mut Vec<Result<StreamEvent>>,
) -> Result<()> {
    let line = frame.trim();

    if line.is_empty() {
        return Ok(());
    }

    if line == "[DONE]" {
        events.push(Ok(StreamEvent::OutputItemDone {
            item_index: 0,
            item: None,
        }));

        events.push(Ok(StreamEvent::ResponseDone {
            status: Some(ResponseStatus::Completed),
            usage: None,
        }));
        return Ok(());
    }

    let chunk: OllamaStreamChunk = serde_json::from_str(line)?;

    if !state.sent_created {
        state.sent_created = true;
        events.push(Ok(StreamEvent::ResponseCreated {
            id: None,
            model: chunk.model.clone(),
        }));
    }

    if !state.started_message {
        state.started_message = true;
        events.push(Ok(StreamEvent::OutputItemStart {
            item_index: 0,
            item: OutputItemHeader::Message {
                id: None,
                role: Role::Assistant,
            },
        }));
    }

    let content = &chunk.message.content;
    if !content.is_empty() {
        events.push(Ok(StreamEvent::ContentDelta {
            item_index: 0,
            content_index: 0,
            delta: ContentDeltaPayload::Text {
                text: content.clone(),
            },
        }));
    }

    if chunk.done {
        events.push(Ok(StreamEvent::OutputItemDone {
            item_index: 0,
            item: None,
        }));

        events.push(Ok(StreamEvent::ResponseDone {
            status: Some(ResponseStatus::Completed),
            usage: Some(Usage {
                prompt_tokens: Some(chunk.prompt_eval_count.unwrap_or(0)),
                completion_tokens: Some(chunk.eval_count.unwrap_or(0)),
                total_tokens: None,
                reasoning_tokens: None,
                cache_read_tokens: None,
                cache_creation_tokens: None,
            }),
        }));
    }

    Ok(())
}

#[derive(Debug, Serialize)]
struct OllamaChatRequest {
    model: String,
    messages: Vec<OllamaMessage>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<OllamaOptions>,
}

#[derive(Debug, Serialize)]
struct OllamaMessage {
    role: String,
    content: String,
}

#[derive(Debug, Serialize)]
struct OllamaOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    num_predict: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    seed: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct OllamaChatResponse {
    model: String,
    message: OllamaResponseMessage,
    done: bool,
    total_duration: Option<u64>,
    prompt_eval_count: Option<u32>,
    eval_count: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct OllamaResponseMessage {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct OllamaStreamChunk {
    model: Option<String>,
    message: OllamaResponseMessage,
    done: bool,
    total_duration: Option<u64>,
    prompt_eval_count: Option<u32>,
    eval_count: Option<u32>,
}

fn to_ollama_request(chat: &ChatRequest) -> OllamaChatRequest {
    let mut messages = Vec::new();

    if let Some(instructions) = &chat.instructions {
        messages.push(OllamaMessage {
            role: "system".to_string(),
            content: instructions.text().to_string(),
        });
    }

    for item in &chat.input {
        match item {
            InputItem::Message { role, content, .. } => {
                messages.push(OllamaMessage {
                    role: to_ollama_role(role).to_string(),
                    content: content.text().to_string(),
                });
            }
            InputItem::FunctionCallOutput { output, .. } => {
                messages.push(OllamaMessage {
                    role: "tool".to_string(),
                    content: output.clone(),
                });
            }
            InputItem::ItemReference { .. } => {}
        }
    }

    let options = OllamaOptions {
        temperature: chat.params.temperature,
        top_p: chat.params.top_p,
        num_predict: chat.params.max_tokens,
        stop: chat.params.stop.clone(),
        seed: chat.params.seed,
    };

    OllamaChatRequest {
        model: chat.model.clone(),
        messages,
        stream: chat.stream.unwrap_or(false),
        options: Some(options),
    }
}

fn to_ollama_role(role: &Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::Developer => "developer",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn from_ollama_response(source: OllamaChatResponse) -> ChatResponse {
    let output = vec![OutputItem::Message {
        id: None,
        role: parse_ollama_role(&source.message.role),
        content: vec![AnnotatedContentPart::plain(ContentPart::Text {
            text: source.message.content,
        })],
        status: Some(ItemStatus::Completed),
        finish_reason: Some(FinishReason::Stop),
    }];

    ChatResponse {
        id: None,
        model: source.model,
        output,
        status: Some(ResponseStatus::Completed),
        usage: Some(Usage {
            prompt_tokens: source.prompt_eval_count,
            completion_tokens: source.eval_count,
            total_tokens: None,
            reasoning_tokens: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
        }),
        service_tier: None,
        extensions: None,
    }
}

fn parse_ollama_role(role: &str) -> Role {
    match role {
        "system" => Role::System,
        "developer" => Role::Developer,
        "user" => Role::User,
        "tool" => Role::Tool,
        _ => Role::Assistant,
    }
}

pub fn apply_backend_auth(builder: RequestBuilder, auth: &AuthConfig) -> RequestBuilder {
    match auth {
        AuthConfig::None => builder,
        AuthConfig::BearerToken { token } => builder.bearer_auth(token),
        AuthConfig::ApiKey { header, token } => builder.header(header, token),
        AuthConfig::Basic { username, password } => {
            builder.basic_auth(username.clone(), Some(password.clone()))
        }
        AuthConfig::Custom { token } => builder.header("Authorization", token),
    }
}
