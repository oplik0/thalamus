use axum_tasks::spawn_task_workers;
use thalamus::{bootstrap, shared::observability};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    observability::init_tracing();

    tracing::info!("Starting Thalamus LLM Router");

    // Get config path from environment or use default
    let config_path = std::env::var("CONFIG_PATH").unwrap_or_else(|_| "config.k".to_string());

    // Create cancellation token for graceful shutdown
    let shutdown_token = CancellationToken::new();

    // Initialize application state
    let state = bootstrap::init_app_state(&config_path, shutdown_token.clone()).await?;

    // Spawn background task workers
    // Pass None to use default worker count: max(4, num_cpus / 2)
    tracing::info!("Starting background task workers");

    spawn_task_workers(
        state.tasks.clone(),
        shutdown_token.clone(),
        None, // Use default worker count
    );

    // Build router with state
    let app = bootstrap::build_router(state);

    // Determine bind address from environment or use default
    let host = std::env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
    let port = std::env::var("PORT").unwrap_or_else(|_| "3000".to_string());
    let addr = format!("{host}:{port}");

    tracing::info!("Listening on {}", addr);

    // Start server with graceful shutdown
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::signal::ctrl_c()
                .await
                .expect("failed to listen for shutdown signal");
            tracing::info!("Shutdown signal received, stopping task workers");
            shutdown_token.cancel();
        })
        .await?;

    Ok(())
}
