//! Teams feature
//!
//! Team management with budgets, rate limits, and access control.

pub mod api;
pub mod domain;
pub mod dto;
pub mod infra;

pub use api::router;
