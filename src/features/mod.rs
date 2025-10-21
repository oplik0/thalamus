//! Feature modules
//!
//! Each feature is organized using clean architecture principles:
//! - api: HTTP handlers and routing
//! - domain: Business logic and trait definitions
//! - infra: Infrastructure implementations (repositories, external services)
//! - dto: Data transfer objects for API layer

pub mod auth;
pub mod authorization;
pub mod backends;
pub mod health;
pub mod llm_proxy;
pub mod routing;
pub mod teams;
