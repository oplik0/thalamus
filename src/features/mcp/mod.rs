//! MCP gateway feature
//!
//! Provides MCP server registration, REST tool proxy, and toolset management.

pub mod api;
pub mod domain;
pub mod dto;
pub mod infra;

pub use api::router;
pub use infra::{
    McpHealthMonitor, McpService, PooledMcpSessionManager, RmcpClientFactory,
    SqlxMcpServerRepository, SqlxMcpToolsetRepository,
};
