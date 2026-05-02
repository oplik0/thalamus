//! Teams API handlers

use axum::{Router, routing::get};

async fn teams_placeholder() -> &'static str {
    "teams"
}

/// Create teams router
pub fn router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new().route("/teams", get(teams_placeholder))
}
