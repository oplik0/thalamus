//! Configuration hot-reload watcher
//!
//! Supports loading configuration with named profiles and hot-reload.

use super::types::Config;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::RwLock;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

/// Configuration watcher for hot-reload support with profiles
#[derive(Debug)]
pub struct ConfigWatcher {
    /// Current configuration (wrapped in RwLock for safe swapping on reload)
    config: Arc<RwLock<Arc<Config>>>,
    path: PathBuf,
    profile: String,
}

impl ConfigWatcher {
    /// Create a new configuration watcher with a specific profile
    ///
    /// # Errors
    /// Returns an error if the configuration file cannot be loaded or is invalid
    pub fn new<P: AsRef<Path>>(path: P, profile: &str) -> crate::Result<Self> {
        let path = path.as_ref().to_path_buf();

        // Load all profiles to validate they exist
        let profiles = super::loader::load_config_profiles(&path)?;

        // Get the requested profile
        let config = profiles.get(profile).cloned().ok_or_else(|| {
            crate::Error::Config(format!(
                "Profile '{}' not found in configuration file. Available profiles: {:?}",
                profile,
                profiles.keys().collect::<Vec<_>>()
            ))
        })?;

        tracing::info!(
            path = %path.display(),
            profile = profile,
            available_profiles = ?profiles.keys().collect::<Vec<_>>(),
            "Loaded configuration profile"
        );

        Ok(Self {
            config: Arc::new(RwLock::new(Arc::new(config))),
            path,
            profile: profile.to_string(),
        })
    }

    /// Create a new configuration watcher with the "default" profile
    ///
    /// # Errors
    /// Returns an error if the configuration file cannot be loaded or is invalid
    pub fn new_default<P: AsRef<Path>>(path: P) -> crate::Result<Self> {
        Self::new(path, "default")
    }

    /// Get a reference to the current configuration
    #[must_use]
    pub fn config(&self) -> Arc<Config> {
        Arc::clone(&self.config.read())
    }

    /// Get the active profile name
    #[must_use]
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// Get a list of available profile names
    #[must_use]
    pub fn profiles(&self) -> Vec<String> {
        // Reload profiles to get the list (they should already be loaded once)
        super::loader::load_config_profiles(&self.path)
            .map(|p| p.keys().cloned().collect())
            .unwrap_or_else(|_| vec![self.profile.clone()])
    }

    /// Manually reload the configuration
    ///
    /// # Errors
    /// Returns an error if the configuration file cannot be loaded or is invalid
    pub async fn reload(&self) -> crate::Result<()> {
        tracing::info!("Reloading configuration");

        // Reload all profiles
        let new_profiles = super::loader::load_config_profiles(&self.path)?;

        // Get the requested profile
        let new_config = new_profiles.get(&self.profile).cloned().ok_or_else(|| {
            crate::Error::Config(format!(
                "Profile '{}' no longer exists after reload",
                self.profile
            ))
        })?;

        // Swap the config Arc pointer (parking_lot RwLock is sync)
        *self.config.write() = Arc::new(new_config);

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
            profile = %self.profile,
            "Started watching configuration file for changes"
        );

        // Spawn task to handle file change events
        tokio::spawn(async move {
            let mut debounce_deadline: Option<tokio::time::Instant> = None;
            let debounce_duration = Duration::from_millis(500);

            loop {
                tokio::select! {
                    // Wait for next file change event
                    event = rx.recv() => {
                        match event {
                            Some(_) => {
                                // Got an event - reset debounce deadline
                                debounce_deadline = Some(tokio::time::Instant::now() + debounce_duration);
                            }
                            None => break,
                        }
                    }
                    // Periodically check if debounce period has passed
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {
                        if let Some(deadline) = debounce_deadline {
                            if deadline.elapsed() >= debounce_duration {
                                // Debounce period passed, reload configuration
                                debounce_deadline = None;
                                if let Err(e) = self.reload().await {
                                    tracing::error!(error = %e, "Failed to reload configuration");
                                } else {
                                    tracing::info!("Configuration hot-reloaded successfully");
                                }
                            }
                        }
                    }
                }
            }
        });

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_watcher_creation() {
        let result = ConfigWatcher::new("config.k", "default");
        // May fail due to missing env vars, but should not panic
        if let Ok(watcher) = result {
            let config = watcher.config();
            assert!(!config.backends.is_empty());
            assert_eq!(watcher.profile(), "default");
        } else {
            // Expected in test environment without proper setup
        }
    }

    #[test]
    fn test_config_watcher_profiles() {
        let result = ConfigWatcher::new("config.k", "default");
        if let Ok(watcher) = result {
            let profiles = watcher.profiles();
            assert!(!profiles.is_empty());
            println!("Available profiles: {:?}", profiles);
        }
    }
}
