//! Thalmus - Backend-centric LLM Router
//!
//! This library provides a clean-architecture implementation of an LLM router
//! with team-based access control, advanced routing strategies, and comprehensive observability.

#![forbid(unsafe_code)]
#![warn(
    missing_debug_implementations,
    rust_2018_idioms,
    clippy::all,
    clippy::pedantic
)]

pub mod bootstrap;
pub mod error;
pub mod features;
pub mod middleware;
pub mod shared;

// Re-export commonly used types
pub use error::{Error, Result};
