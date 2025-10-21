//! Thalmus server entry point

use thalmus::{bootstrap, shared::observability};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    observability::init_tracing();

    tracing::info!("Starting Thalmus LLM Router");

    // Initialize application state
    let _state = bootstrap::init_app_state().await?;

    // Build router
    let app = bootstrap::build_router();

    // Determine bind address from environment or use default
    let host = std::env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
    let port = std::env::var("PORT").unwrap_or_else(|_| "3000".to_string());
    let addr = format!("{host}:{port}");

    tracing::info!("Listening on {}", addr);

    // Start server
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
