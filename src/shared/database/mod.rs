//! Database infrastructure
//!
//! SQLx connection pool and migration management.

pub mod pool;

pub use pool::{PoolConfig, create_pool, run_migrations};
