//! Batch API handlers

use axum::{
    Json, Router,
    extract::{Path, State},
    http::HeaderMap,
    routing::{get, post},
};
use serde_json::json;
use uuid::Uuid;

use crate::Result;
use crate::bootstrap::AppState;
use crate::features::batch::domain::{BatchJob, BatchRequestBody};
use crate::middleware::OptionalApiKeyAuth;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/batch/chat/completions", post(create_batch))
        .route("/v1/batch/chat/completions/{id}", get(get_batch))
}

/// Create a new batch job from an array of chat completion requests.
///
/// # Errors
/// Returns `InvalidInput` if the request body is empty or too large.
pub async fn create_batch(
    State(state): State<AppState>,
    _headers: HeaderMap,
    OptionalApiKeyAuth(auth): OptionalApiKeyAuth,
    Json(body): Json<BatchRequestBody>,
) -> Result<Json<serde_json::Value>> {
    if body.requests.is_empty() {
        return Err(crate::Error::InvalidInput(
            "Batch request must contain at least one request".to_string(),
        ));
    }

    // Limit batch size to avoid unbounded memory / DB pressure.
    if body.requests.len() > 1000 {
        return Err(crate::Error::InvalidInput(
            "Batch request exceeds maximum of 1000 requests".to_string(),
        ));
    }

    let (team_id, user_id) = auth
        .as_ref()
        .map(|a| (Some(a.team_id), Some(a.user_id)))
        .unwrap_or((None, None));

    let id = state.batch_service.create_job(body, team_id, user_id).await?;

    Ok(Json(json!({
        "id": id.to_string(),
        "status": "pending",
    })))
}

/// Retrieve a batch job by id, including its current status and results.
///
/// # Errors
/// Returns `NotFound` if the job does not exist.
pub async fn get_batch(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Option<BatchJob>>> {
    let job = state.batch_service.get_job(id).await?;
    Ok(Json(job))
}
