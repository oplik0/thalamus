use std::convert::Infallible;

use axum::{
    Json, Router,
    extract::State,
    response::sse::{Event, KeepAlive, Sse},
    response::{IntoResponse, Response},
    routing::post,
};
use futures::StreamExt;

use crate::Result;
use crate::bootstrap::AppState;
use crate::features::llm_proxy::anthropic::dto::{
    AnthropicError, AnthropicMessagesRequest, AnthropicMessagesResponse, AnthropicStreamEvent,
    stream_event_to_anthropic,
};
use crate::shared::models::LlmRequest;

pub fn router() -> Router<AppState> {
    Router::new().route("/v1/messages", post(messages))
}

pub async fn messages(
    State(state): State<AppState>,
    Json(request): Json<AnthropicMessagesRequest>,
) -> Result<Response> {
    let is_stream = request.stream;
    let unified: LlmRequest = request.into();

    if is_stream {
        let stream = state.proxy.handle_stream(unified).await?;
        let sse_stream = stream.flat_map(|event| {
            let events: Vec<std::result::Result<Event, Infallible>> = match event {
                Ok(evt) => stream_event_to_anthropic(evt)
                    .into_iter()
                    .filter_map(|chunk| {
                        let event_type = chunk.event_type();
                        let json_data = serde_json::to_string(&chunk).ok()?;
                        Some(Ok(Event::default().event(event_type).data(json_data)))
                    })
                    .collect(),
                Err(error) => {
                    let err_event = AnthropicStreamEvent::Error {
                        error: AnthropicError {
                            error_type: "server_error".to_string(),
                            message: error.to_string(),
                        },
                    };
                    if let Ok(json_data) = serde_json::to_string(&err_event) {
                        vec![Ok(Event::default().event("error").data(json_data))]
                    } else {
                        vec![]
                    }
                }
            };
            futures::stream::iter(events)
        });

        return Ok(Sse::new(sse_stream)
            .keep_alive(KeepAlive::default())
            .into_response());
    }

    let response = state.proxy.handle(unified).await?;
    Ok(Json(AnthropicMessagesResponse::from(response)).into_response())
}
