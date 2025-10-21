//! Shared infrastructure modules
//!
//! These modules provide cross-cutting concerns used by multiple features:
//! - config: Configuration management
//! - database: Database connection pooling
//! - observability: Tracing and metrics

pub mod config;
pub mod database;
pub mod observability;
