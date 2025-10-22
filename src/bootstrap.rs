//! Application bootstrap and dependency injection
//!
//! This module wires together all the application components,
//! creates the AppState, and builds the Axum router.

use axum::Router;

/// Application state shared across all handlers
#[derive(Clone, Debug)]
pub struct AppState {
    // Database pool will go here
    // Config will go here
    // Other shared state
}

/// Build the application router with all routes and middleware
pub fn build_router() -> Router {
    Router::new()
        // Health check (no state needed)
        .merge(crate::features::health::router())
    // Other routes will be added here
}

/// Initialize the application state
///
/// This function:
/// - Connects to the database
/// - Loads configuration
/// - Initializes shared services
#[expect(clippy::unused_async)] // Will be async when we add real initialization
pub async fn init_app_state() -> crate::Result<AppState> {
    Ok(AppState {
        // Initialize components here
    })
}
