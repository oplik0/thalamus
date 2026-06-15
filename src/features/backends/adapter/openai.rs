use std::collections::VecDeque;
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
    AnnotatedContentPart, ChatRequest, ChatResponse, Content, ContentDeltaPayload, ContentPart,
    EmbeddingRequest, FinishReason, InputItem, ItemStatus, LlmRequest, OutputItem,
    OutputItemHeader, ResponseStatus, Role, StreamEvent, ToolCallDelta, ToolChoice, ToolDefinition,
    Usage,
};

#[derive(Debug)]
pub struct OpenAiAdapter;

impl BackendAdapter for OpenAiAdapter {
    fn name(&self) -> &'static str {
        "openai"
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
                    "OpenAI chat endpoint requires a chat request".to_string(),
                ));
            }
        };

        let payload = to_openai_request(chat);
        let url = format!("{}/v1/chat/completions", endpoint_url.trim_end_matches('/'));

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

        let parsed: OpenAiChatResponse = serde_json::from_slice(body).map_err(|err| {
            Error::Backend(format!("Backend returned invalid chat response: {err}"))
        })?;
        Ok(from_openai_response(parsed))
    }

    fn parse_stream(
        &self,
        body_stream: Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>> {
        let stream = body_stream
            .scan(StreamParseState::default(), |state, item| {
                let mut events = Vec::new();
                match item {
                    Ok(chunk) => {
                        state.buffer.push_str(&String::from_utf8_lossy(&chunk));
                        while let Some(pos) = state.buffer.find("\n\n") {
                            let frame = state.buffer[..pos].to_string();
                            state.buffer = state.buffer[pos + 2..].to_string();
                            if let Err(error) = parse_sse_frame(&frame, state, &mut events) {
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
        client: &reqwest::Client,
        endpoint_url: &str,
        request: &EmbeddingRequest,
        auth: &AuthConfig,
        timeout: Duration,
    ) -> Result<reqwest::Request> {
        let url = format!("{}/v1/embeddings", endpoint_url.trim_end_matches('/'));
        let builder = client.post(url).timeout(timeout).json(request);
        apply_backend_auth(builder, auth)
            .build()
            .map_err(Error::from)
    }

    fn parse_embedding_response(
        &self,
        status: StatusCode,
        body: &[u8],
    ) -> Result<serde_json::Value> {
        if !status.is_success() {
            let detail = String::from_utf8_lossy(body).to_string();
            return Err(Error::Backend(format!(
                "Backend returned status {}: {}",
                status.as_u16(),
                detail
            )));
        }

        serde_json::from_slice(body).map_err(|err| {
            Error::Backend(format!(
                "Backend returned invalid embedding response: {err}"
            ))
        })
    }
}

#[derive(Default)]
struct StreamParseState {
    buffer: String,
    sent_created: bool,
    started_message: bool,
}

fn parse_sse_frame(
    frame: &str,
    state: &mut StreamParseState,
    events: &mut Vec<Result<StreamEvent>>,
) -> Result<()> {
    let mut data_lines = VecDeque::new();
    for line in frame.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            data_lines.push_back(data.trim().to_string());
        }
    }

    if data_lines.is_empty() {
        return Ok(());
    }

    let payload = data_lines.into_iter().collect::<Vec<_>>().join("\n");

    if payload == "[DONE]" {
        events.push(Ok(StreamEvent::ResponseDone {
            status: Some(ResponseStatus::Completed),
            usage: None,
        }));
        return Ok(());
    }

    let chunk: OpenAiChatChunk = serde_json::from_str(&payload)?;

    if !state.sent_created {
        state.sent_created = true;
        events.push(Ok(StreamEvent::ResponseCreated {
            id: chunk.id.clone(),
            model: chunk.model.clone(),
        }));
    }

    for choice in chunk.choices {
        if !state.started_message {
            state.started_message = true;
            events.push(Ok(StreamEvent::OutputItemStart {
                item_index: choice.index,
                item: OutputItemHeader::Message {
                    id: None,
                    role: Role::Assistant,
                },
            }));
        }

        if let Some(content) = choice.delta.content {
            events.push(Ok(StreamEvent::ContentDelta {
                item_index: choice.index,
                content_index: 0,
                delta: ContentDeltaPayload::Text { text: content },
            }));
        }

        if let Some(tool_calls) = choice.delta.tool_calls {
            for call in tool_calls {
                events.push(Ok(StreamEvent::ContentDelta {
                    item_index: choice.index,
                    content_index: 0,
                    delta: ContentDeltaPayload::ToolCall(ToolCallDelta {
                        tool_call_index: call.index,
                        id: call.id,
                        name: call.function.as_ref().map(|f| f.name.clone()),
                        arguments: call.function.map(|f| f.arguments),
                    }),
                }));
            }
        }

        if choice.finish_reason.is_some() {
            events.push(Ok(StreamEvent::OutputItemDone {
                item_index: choice.index,
                item: None,
            }));
        }
    }

    if let Some(usage) = chunk.usage {
        events.push(Ok(StreamEvent::ResponseDone {
            status: Some(ResponseStatus::Completed),
            usage: Some(usage),
        }));
    }

    Ok(())
}

#[derive(Debug, Serialize)]
struct OpenAiChatRequest {
    model: String,
    messages: Vec<OpenAiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    frequency_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    presence_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    seed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OpenAiToolDef>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct OpenAiToolDef {
    #[serde(rename = "type")]
    tool_type: &'static str,
    function: OpenAiToolDefFunction,
}

#[derive(Debug, Serialize)]
struct OpenAiToolDefFunction {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parameters: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    strict: Option<bool>,
}

#[derive(Debug, Serialize)]
struct OpenAiMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OpenAiMessageToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

#[derive(Debug, Serialize)]
struct OpenAiMessageToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: &'static str,
    function: OpenAiMessageToolFunction,
}

#[derive(Debug, Serialize)]
struct OpenAiMessageToolFunction {
    name: String,
    arguments: String,
}

fn to_openai_request(chat: &ChatRequest) -> OpenAiChatRequest {
    let mut messages = Vec::new();

    if let Some(instructions) = &chat.instructions {
        messages.push(OpenAiMessage {
            role: "system".to_string(),
            content: Some(instructions.text()),
            tool_call_id: None,
            tool_calls: None,
            name: None,
        });
    }

    for item in &chat.input {
        match item {
            InputItem::Message {
                role,
                content,
                name,
                ..
            } => {
                // Check if this is an assistant message with tool calls in its content
                let tool_calls = extract_tool_calls(content);
                let text = content.text();
                let content_str = if text.is_empty() && tool_calls.is_some() {
                    None
                } else {
                    Some(text)
                };

                messages.push(OpenAiMessage {
                    role: to_openai_role(role).to_string(),
                    content: content_str,
                    tool_call_id: None,
                    tool_calls,
                    name: name.clone(),
                });
            }
            InputItem::FunctionCallOutput { call_id, output } => {
                messages.push(OpenAiMessage {
                    role: "tool".to_string(),
                    content: Some(output.clone()),
                    tool_call_id: Some(call_id.clone()),
                    tool_calls: None,
                    name: None,
                });
            }
            InputItem::ItemReference { .. } => {
                // Item references don't map to OpenAI messages
            }
        }
    }

    let tools = chat.tools.as_ref().and_then(|tools| {
        let oai_tools: Vec<_> = tools
            .iter()
            .filter_map(|t| match t {
                ToolDefinition::Function { function } => Some(OpenAiToolDef {
                    tool_type: "function",
                    function: OpenAiToolDefFunction {
                        name: function.name.clone(),
                        description: function.description.clone(),
                        parameters: function.parameters.clone(),
                        strict: function.strict,
                    },
                }),
                ToolDefinition::ServerTool(_) => None,
            })
            .collect();
        if oai_tools.is_empty() {
            None
        } else {
            Some(oai_tools)
        }
    });

    let tool_choice = chat.tool_choice.as_ref().map(|tc| match tc {
        ToolChoice::Auto => serde_json::json!("auto"),
        ToolChoice::None => serde_json::json!("none"),
        ToolChoice::Required => serde_json::json!("required"),
        ToolChoice::Function { name } => {
            serde_json::json!({"type": "function", "function": {"name": name}})
        }
    });

    let response_format = chat.response_format.as_ref().map(|rf| match rf {
        crate::shared::models::ResponseFormat::Text => serde_json::json!({"type": "text"}),
        crate::shared::models::ResponseFormat::JsonObject => {
            serde_json::json!({"type": "json_object"})
        }
        crate::shared::models::ResponseFormat::JsonSchema {
            name,
            schema,
            strict,
        } => {
            let mut obj =
                serde_json::json!({"type": "json_schema", "json_schema": {"schema": schema}});
            if let Some(n) = name {
                obj["json_schema"]["name"] = serde_json::json!(n);
            }
            if let Some(s) = strict {
                obj["json_schema"]["strict"] = serde_json::json!(s);
            }
            obj
        }
    });

    OpenAiChatRequest {
        model: chat.model.clone(),
        messages,
        temperature: chat.params.temperature,
        top_p: chat.params.top_p,
        max_tokens: chat.params.max_tokens,
        stream: chat.stream,
        stop: chat.params.stop.clone(),
        frequency_penalty: chat.params.frequency_penalty,
        presence_penalty: chat.params.presence_penalty,
        seed: chat.params.seed,
        tools,
        tool_choice,
        response_format,
    }
}

/// Extract tool calls from `Content::Parts` if any exist
fn extract_tool_calls(content: &Content) -> Option<Vec<OpenAiMessageToolCall>> {
    let calls: Vec<_> = content
        .tool_calls()
        .into_iter()
        .map(|tc| OpenAiMessageToolCall {
            id: tc.id.clone(),
            call_type: "function",
            function: OpenAiMessageToolFunction {
                name: tc.name.clone(),
                arguments: tc.arguments.clone(),
            },
        })
        .collect();
    if calls.is_empty() { None } else { Some(calls) }
}

fn to_openai_role(role: &Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::Developer => "developer",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

#[derive(Debug, Deserialize)]
struct OpenAiChatResponse {
    id: Option<String>,
    model: String,
    choices: Vec<OpenAiChoice>,
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChoice {
    message: OpenAiChoiceMessage,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChoiceMessage {
    role: String,
    content: Option<String>,
    tool_calls: Option<Vec<OpenAiToolCall>>,
}

#[derive(Debug, Deserialize)]
struct OpenAiToolCall {
    id: Option<String>,
    function: OpenAiToolFunction,
}

#[derive(Debug, Deserialize)]
struct OpenAiToolFunction {
    name: String,
    arguments: String,
}

fn from_openai_response(source: OpenAiChatResponse) -> ChatResponse {
    let mut output = Vec::new();

    for choice in source.choices {
        let finish = choice
            .finish_reason
            .as_deref()
            .map_or(FinishReason::Stop, parse_finish_reason);

        let has_tool_calls = choice
            .message
            .tool_calls
            .as_ref()
            .is_some_and(|tc| !tc.is_empty());

        if let Some(content) = choice.message.content
            && (!content.is_empty() || !has_tool_calls)
        {
            output.push(OutputItem::Message {
                id: None,
                role: parse_openai_role(&choice.message.role),
                content: vec![AnnotatedContentPart::plain(ContentPart::Text {
                    text: content,
                })],
                status: Some(ItemStatus::Completed),
                finish_reason: Some(finish.clone()),
            });
        }

        if let Some(tool_calls) = choice.message.tool_calls {
            for tool_call in tool_calls {
                output.push(OutputItem::FunctionCall {
                    id: tool_call.id.clone(),
                    call_id: tool_call
                        .id
                        .unwrap_or_else(|| tool_call.function.name.clone()),
                    name: tool_call.function.name,
                    arguments: tool_call.function.arguments,
                    status: Some(ItemStatus::Completed),
                });
            }
        }
    }

    ChatResponse {
        id: source.id,
        model: source.model,
        output,
        status: Some(ResponseStatus::Completed),
        usage: source.usage,
        service_tier: None,
        extensions: None,
    }
}

fn parse_openai_role(role: &str) -> Role {
    match role {
        "system" => Role::System,
        "developer" => Role::Developer,
        "user" => Role::User,
        "tool" => Role::Tool,
        _ => Role::Assistant,
    }
}

fn parse_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "length" => FinishReason::Length,
        "tool_calls" => FinishReason::ToolCalls,
        "content_filter" => FinishReason::ContentFilter,
        _ => FinishReason::Stop,
    }
}

#[derive(Debug, Deserialize)]
struct OpenAiChatChunk {
    id: Option<String>,
    model: Option<String>,
    choices: Vec<OpenAiChunkChoice>,
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChunkChoice {
    index: u32,
    delta: OpenAiChunkDelta,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChunkDelta {
    content: Option<String>,
    tool_calls: Option<Vec<OpenAiChunkToolCall>>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChunkToolCall {
    index: u32,
    id: Option<String>,
    function: Option<OpenAiChunkToolFunction>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChunkToolFunction {
    name: String,
    arguments: String,
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
