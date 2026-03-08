use axum_tasks::spawn_task_workers;
use clap::Parser;
use std::sync::Arc;
use thalamus::{bootstrap, shared::config::ConfigWatcher, shared::observability};
use tokio_util::sync::CancellationToken;

/// Command-line arguments for Thalamus
#[derive(Parser, Debug)]
#[command(name = "thalamus")]
#[command(about = "Backend-centric LLM router and load balancer")]
struct Args {
    /// Path to the configuration file
    #[arg(short, long, default_value = "config.k")]
    config: String,

    /// Configuration profile to use
    #[arg(short, long, default_value = "default")]
    profile: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Parse command-line arguments
    let args = Args::parse();

    // Initialize tracing
    observability::init_tracing();

    tracing::info!("Starting Thalamus LLM Router");

    // Create the configuration watcher with the specified profile
    let watcher = Arc::new(ConfigWatcher::new(&args.config, &args.profile)?);

    // Get the config to use for initialization
    let config = watcher.config();

    // Log the loaded profile
    tracing::info!(
        config_path = %args.config,
        profile = %args.profile,
        "Configuration loaded"
    );

    // Create cancellation token for graceful shutdown
    let shutdown_token = CancellationToken::new();

    // Initialize application state with the config
    let state = bootstrap::init_app_state(config, shutdown_token.clone()).await?;

    // Spawn background task workers
    // Pass None to use default worker count: max(4, num_cpus / 2)
    tracing::info!("Starting background task workers");

    spawn_task_workers(
        state.tasks.clone(),
        shutdown_token.clone(),
        None, // Use default worker count
    );

    // Build router with state
    let app = bootstrap::build_router(state.clone());

    // Get the host and port from config
    let host = state.config.server.host.clone();
    let port = state.config.server.port;

    let addr = format!("{host}:{port}");

    tracing::info!("Listening on {}", addr);

    // Start configuration hot-reload watching
    let watcher_for_shutdown = Arc::clone(&watcher);
    watcher.start_watching().await?;

    // Start server with graceful shutdown
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::signal::ctrl_c()
                .await
                .expect("failed to listen for shutdown signal");
            tracing::info!("Shutdown signal received, stopping task workers");
            // Trigger config reload on shutdown
            if let Err(e) = watcher_for_shutdown.reload().await {
                tracing::warn!(error = %e, "Failed to reload config on shutdown");
            }
            shutdown_token.cancel();
        })
        .await?;

    Ok(())
}
