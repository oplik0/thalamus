use std::convert::Infallible;

use axum::{
    Json, Router,
    extract::State,
    response::IntoResponse,
    response::sse::{Event, KeepAlive, Sse},
    routing::post,
};
use futures::StreamExt;

use crate::Result;
use crate::bootstrap::AppState;
use crate::features::llm_proxy::openai::dto::{
    ChatCompletionsRequest, ChatCompletionsResponse, OpenAiEmbeddingsRequest, StreamChunkConverter,
};
use crate::shared::models::{LlmRequest, StreamEvent};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/embeddings", post(embeddings))
}

pub async fn chat_completions(
    State(state): State<AppState>,
    Json(request): Json<ChatCompletionsRequest>,
) -> Result<axum::response::Response> {
    let is_stream = request.stream;
    let unified: LlmRequest = request.into();

    if is_stream {
        let stream = state.proxy.handle_stream(unified).await?;

        // Use scan to carry converter state and a done flag through the stream.
        // Yields Option<Result<Event>>: Some for events to emit, None to skip.
        // Returns None from scan (terminating the stream) once done.
        let sse_stream = stream
            .scan(
                (StreamChunkConverter::default(), false),
                |(converter, done), event| {
                    if *done {
                        return std::future::ready(None);
                    }
                    let sse_event: Option<std::result::Result<Event, Infallible>> = match event {
                        Ok(evt) => {
                            if matches!(evt, StreamEvent::ResponseDone { .. }) {
                                *done = true;
                                Some(Ok(Event::default().data("[DONE]")))
                            } else {
                                converter.convert(evt).and_then(|chunk| {
                                    let json_data = serde_json::to_string(&chunk).ok()?;
                                    Some(Ok(Event::default().data(json_data)))
                                })
                            }
                        }
                        Err(error) => {
                            let payload = serde_json::json!({
                                "error": {"message": error.to_string(), "type": "server_error"}
                            })
                            .to_string();
                            Some(Ok(Event::default().data(payload)))
                        }
                    };
                    std::future::ready(Some(sse_event))
                },
            )
            .filter_map(std::future::ready);

        return Ok(Sse::new(sse_stream)
            .keep_alive(KeepAlive::default())
            .into_response());
    }

    let response = state.proxy.handle(unified).await?;
    Ok(Json(ChatCompletionsResponse::from(response)).into_response())
}

pub async fn embeddings(
    State(state): State<AppState>,
    Json(request): Json<OpenAiEmbeddingsRequest>,
) -> Result<Json<serde_json::Value>> {
    let response = state.proxy.handle_embedding(request.into()).await?;
    Ok(Json(response))
}
