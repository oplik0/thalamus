//! Authorization feature
//!
//! Casbin-based RBAC with domain (team) support.

pub mod api;
pub mod domain;
pub mod infra;

pub use api::router;
pub use domain::{AuthRequest, Authorizer, PolicyManager};
pub use infra::CasbinAuthorizer;
