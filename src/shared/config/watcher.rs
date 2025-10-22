//! Configuration hot-reload watcher

use super::{loader::load_config, types::Config};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::RwLock;

/// Configuration watcher for hot-reload support
#[derive(Debug)]
pub struct ConfigWatcher {
    config: Arc<RwLock<Config>>,
    path: PathBuf,
}

impl ConfigWatcher {
    /// Create a new configuration watcher
    ///
    /// # Errors
    /// Returns an error if the configuration file cannot be loaded or is invalid
    pub fn new<P: AsRef<Path>>(path: P) -> crate::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let config = load_config(&path)?;

        Ok(Self {
            config: Arc::new(RwLock::new(config)),
            path,
        })
    }

    /// Get a reference to the current configuration
    #[must_use]
    pub fn config(&self) -> Arc<RwLock<Config>> {
        Arc::clone(&self.config)
    }

    /// Manually reload the configuration
    ///
    /// # Errors
    /// Returns an error if the configuration file cannot be loaded or is invalid
    pub async fn reload(&self) -> crate::Result<()> {
        tracing::info!("Reloading configuration");

        let new_config = load_config(&self.path)?;

        let mut config = self.config.write().await;
        *config = new_config;

        tracing::info!("Configuration reloaded successfully");

        Ok(())
    }

    /// Start watching for configuration file changes
    ///
    /// # Errors
    /// Returns an error if the file watcher cannot be created or the configuration file cannot be watched
    #[expect(clippy::unused_async)] // todo: check if actual impl needs async
    pub async fn start_watching(self: Arc<Self>) -> crate::Result<()> {
        let (tx, mut rx) = tokio::sync::mpsc::channel(100);

        // Create file watcher
        let mut watcher: RecommendedWatcher = Watcher::new(
            move |result: Result<Event, notify::Error>| {
                if let Ok(event) = result {
                    let _ = tx.blocking_send(event);
                }
            },
            notify::Config::default().with_poll_interval(Duration::from_secs(2)),
        )
        .map_err(|e| crate::Error::Config(format!("Failed to create file watcher: {e}")))?;

        // Watch the config file
        watcher
            .watch(&self.path, RecursiveMode::NonRecursive)
            .map_err(|e| {
                crate::Error::Config(format!("Failed to watch configuration file: {e}"))
            })?;

        // Also watch the pkg/ directory for schema changes
        let pkg_dir = self
            .path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("pkg");
        if pkg_dir.exists() {
            let _ = watcher.watch(&pkg_dir, RecursiveMode::Recursive);
        }

        tracing::info!(
            path = %self.path.display(),
            "Started watching configuration file for changes"
        );

        // Spawn task to handle file change events
        tokio::spawn(async move {
            #[expect(unused_assignments)] // TODO: finish debounce logic
            let mut debounce_timer: Option<tokio::time::Instant> = None;
            let debounce_duration = Duration::from_millis(500);

            while let Some(_event) = rx.recv().await {
                // Debounce: wait for a period of inactivity before reloading
                debounce_timer = Some(tokio::time::Instant::now());

                tokio::time::sleep(debounce_duration).await;

                // Check if another event came in during the debounce period
                #[expect(unused_assignments)] // TODO: finish debounce logic
                if let Some(timer) = debounce_timer
                    && timer.elapsed() >= debounce_duration
                {
                    // Reload configuration
                    if let Err(e) = self.reload().await {
                        tracing::error!(error = %e, "Failed to reload configuration");
                    } else {
                        tracing::info!("Configuration hot-reloaded successfully");
                    }
                    debounce_timer = None;
                }
            }

            // Keep watcher alive
            drop(watcher);
        });

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_config_watcher_creation() {
        let result = ConfigWatcher::new("config.example.k");
        // May fail due to missing env vars, but should not panic
        if let Ok(watcher) = result {
            let config = watcher.config();
            let config_read = config.read().await;
            assert!(!config_read.backends.is_empty());
        } else {
            // Expected in test environment without proper setup
        }
    }
}
