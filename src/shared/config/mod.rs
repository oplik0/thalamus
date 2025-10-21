//! Configuration management
//!
//! KCL-based configuration loading with hot-reload support.

pub mod loader;
pub mod types;
pub mod watcher;

pub use loader::load_config;
pub use types::Config;
pub use watcher::ConfigWatcher;
