//! Global middleware
//!
//! Middleware that applies to all routes:
//! - Request ID generation
//! - Timeout handling
//! - Request/response logging
//! - API key authentication
//! - Admin authorization
//! - Rate limiting
//! - Casbin authorization

pub mod admin_auth;
pub mod auth;
pub mod authz;
pub mod rate_limit;
pub mod security_headers;

// Module declarations for future middleware
// pub mod request_id;
// pub mod timeout;

pub use admin_auth::{admin_auth_middleware, require_admin, require_task_monitor};
pub use auth::{
    ApiKeyAuth, OptionalApiKeyAuth, require_all_scopes, require_any_scope, require_scope,
};
pub use authz::{AuthzExt, casbin_auth_middleware, require_permission};
pub use rate_limit::{
    RateLimitConfig, RateLimitHeaders, RateLimitLayer, RateLimiter, rate_limit_middleware,
    strict_rate_limit_middleware,
};
pub use security_headers::{
    SecurityHeadersConfig, cors_layer, request_id_middleware, sanitize_headers_middleware,
    security_headers_middleware,
};
