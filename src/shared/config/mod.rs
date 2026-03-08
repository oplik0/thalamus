//! Configuration management
//!
//! KCL-based configuration loading with hot-reload support and named profiles.

pub mod loader;
pub mod types;
pub mod watcher;

pub use loader::{load_config, load_config_profiles};
pub use types::Config;
pub use watcher::ConfigWatcher;
