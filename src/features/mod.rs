//! Feature modules
//!
//! Each feature is organized into a separate module with the following structure:
//! - api: HTTP handlers and routing
//! - domain: Business logic and trait definitions
//! - infra: Infrastructure implementations (repositories, external services)
//! - dto: Data transfer objects for API layer

pub mod auth;
pub mod authorization;
pub mod backends;
pub mod batch;
pub mod health;
pub mod llm_proxy;
pub mod plugin;
pub mod routing;
pub mod teams;
pub mod users;
