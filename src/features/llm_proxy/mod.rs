//! LLM proxy feature
//!
//! OpenAI-compatible API endpoints for chat completions.

pub mod anthropic;
pub mod domain;
pub mod openai;
pub mod responses;

use axum::Router;

use crate::bootstrap::AppState;

pub use domain::ProxyService;

pub fn router() -> Router<AppState> {
    Router::new()
        .merge(openai::api::router())
        .merge(anthropic::api::router())
        .merge(responses::api::router())
}
