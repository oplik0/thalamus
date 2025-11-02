//! Global middleware
//!
//! Middleware that applies to all routes:
//! - Request ID generation
//! - Timeout handling
//! - Request/response logging
//! - API key authentication
//! - Admin authorization

pub mod admin_auth;
pub mod api_key_auth;

// Module declarations for future middleware
// pub mod request_id;
// pub mod timeout;

pub use admin_auth::{require_admin, require_task_monitor};
pub use api_key_auth::{
    ApiKeyAuth, OptionalApiKeyAuth, require_all_scopes, require_any_scope, require_scope,
};
