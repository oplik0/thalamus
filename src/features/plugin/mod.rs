//! Plugin feature
//!
//! Extism-based plugin system for custom routing strategies and other extensions.

pub mod adapter_bridge;
pub mod api;
pub mod domain;
pub mod guardrail_bridge;
pub mod infra;
pub mod routing_bridge;

pub use api::router;
pub use domain::PluginManager;
