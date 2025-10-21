//! Observability setup
//!
//! Configures tracing and metrics collection.

use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

/// Initialize tracing
///
/// Sets up structured logging with environment-based filtering.
/// Use RUST_LOG environment variable to control log levels.
pub fn init_tracing() {
    tracing_subscriber::registry()
        .with(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,thalmus=debug")),
        )
        .with(tracing_subscriber::fmt::layer().with_target(true))
        .init();
}
