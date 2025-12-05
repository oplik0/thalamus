//! Authentication feature
//!
//! Provides multiple authentication mechanisms:
//! - API keys (database-stored random tokens)
//! - PASETO tokens (stateful sessions)
//! - HTTP Signatures (RFC 9421) - planned

pub mod api;
pub mod domain;
pub mod infra;

pub use api::router;
