//! Durable debug-session storage and constrained Kubernetes/OpenCode integrations.
//!
//! Callers must authenticate principals and verify current team membership on every
//! operation. Kubernetes and model/tool-budget enforcement are separate trusted
//! prerequisites; neither stored limits nor a Job deadline enforce token or tool
//! budgets by themselves.

pub mod jobs;
pub mod proxy;
pub mod store;

pub const MIGRATION: &str = include_str!("../migrations/0001_debug.sql");
