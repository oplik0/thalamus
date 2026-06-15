use std::convert::Infallible;

use axum::{
    Json, Router,
    extract::State,
    http::HeaderMap,
    response::sse::{Event, KeepAlive, Sse},
    response::{IntoResponse, Response},
    routing::post,
};
use futures::StreamExt;

use crate::Result;
use crate::bootstrap::AppState;
use crate::features::routing::priority::resolve_priority;
use crate::middleware::ApiKeyAuth;
use crate::shared::models::{ChatRequest, LlmRequest};

pub fn router() -> Router<AppState> {
    Router::new().route("/v1/responses", post(responses))
}

pub async fn responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    ApiKeyAuth(auth): ApiKeyAuth,
    Json(request): Json<ChatRequest>,
) -> Result<Response> {
    let priority = resolve_priority(&headers, Some(&auth), &state.config.routing);
    let is_stream = request.stream.unwrap_or(false);
    let unified = LlmRequest::Chat(request);

    if is_stream {
        let stream = state.proxy.handle_stream(unified, priority).await?;
        let sse_stream = stream.map(|event| {
            let payload = match event {
                Ok(evt) => serde_json::to_string(&evt).unwrap_or_else(|_| "{}".to_string()),
                Err(error) => {
                    serde_json::json!({"event":"error","message": error.to_string()}).to_string()
                }
            };
            Ok::<Event, Infallible>(Event::default().data(payload))
        });

        return Ok(Sse::new(sse_stream)
            .keep_alive(KeepAlive::default())
            .into_response());
    }

    let response = state.proxy.handle(unified, priority).await?;
    Ok(Json(response).into_response())
}
