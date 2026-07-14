use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// Runtime information about a loaded plugin
#[derive(Debug, Clone)]
pub struct PluginInfo {
    pub name: String,
    pub plugin_type: crate::shared::config::types::PluginType,
    pub wasm_path: String,
    pub loaded_at: chrono::DateTime<chrono::Utc>,
    pub call_count: u64,
    pub error_count: u64,
    pub active_instances: usize,
    pub max_instances: usize,
}

/// Manager for loaded plugins
pub struct PluginManager {
    plugins: dashmap::DashMap<String, crate::features::plugin::infra::PluginEntry>,
}

impl std::fmt::Debug for PluginManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginManager")
            .field(
                "plugins",
                &self
                    .plugins
                    .iter()
                    .map(|e| e.key().clone())
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl PluginManager {
    /// Create a new empty plugin manager
    #[must_use]
    pub fn new() -> Self {
        Self {
            plugins: dashmap::DashMap::new(),
        }
    }

    /// Load plugins from a configuration
    pub fn load_from_config(
        config: &crate::shared::config::types::PluginConfig,
    ) -> crate::Result<Self> {
        let manager = Self::new();
        if !config.enabled {
            return Ok(manager);
        }

        if config.plugins.is_empty() {
            // Scan directory for .wasm files
            let entries = std::fs::read_dir(&config.directory).map_err(|e| {
                crate::Error::Internal(format!(
                    "Failed to read plugin directory '{}': {}",
                    config.directory, e
                ))
            })?;

            for entry in entries {
                let entry = entry.map_err(|e| {
                    crate::Error::Internal(format!("Failed to read directory entry: {e}"))
                })?;
                let path = entry.path();

                if path.extension().and_then(|s| s.to_str()) == Some("wasm") {
                    let name = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("unknown")
                        .to_string();
                    let wasm_path = path.to_string_lossy().to_string();

                    let manifest = crate::shared::config::types::PluginManifest {
                        name,
                        plugin_type: crate::shared::config::types::PluginType::Routing,
                        wasm_path,
                        config: HashMap::new(),
                        wasi: false,
                        max_instances: config.max_instances,
                        timeout_ms: config.timeout_ms,
                    };

                    manager.load_plugin(manifest)?;
                }
            }
        } else {
            for manifest in &config.plugins {
                manager.load_plugin(manifest.clone())?;
            }
        }

        Ok(manager)
    }

    /// Load a single plugin from its manifest
    pub fn load_plugin(
        &self,
        manifest: crate::shared::config::types::PluginManifest,
    ) -> crate::Result<()> {
        let pool = crate::features::plugin::infra::PluginRuntime::build_pool(&manifest)?;
        Self::validate_plugin_exports(&pool, &manifest)?;

        let entry = crate::features::plugin::infra::PluginEntry {
            pool,
            manifest: manifest.clone(),
            loaded_at: chrono::Utc::now(),
            call_count: std::sync::atomic::AtomicU64::new(0),
            error_count: std::sync::atomic::AtomicU64::new(0),
        };

        self.plugins.insert(manifest.name.clone(), entry);
        Ok(())
    }

    fn validate_plugin_exports(
        pool: &extism::Pool,
        manifest: &crate::shared::config::types::PluginManifest,
    ) -> crate::Result<()> {
        let timeout = Duration::from_secs(1);

        match manifest.plugin_type {
            crate::shared::config::types::PluginType::Routing => {
                Self::require_export(pool, manifest, "select", timeout)?;
            }
            crate::shared::config::types::PluginType::Adapter => {
                Self::require_export(pool, manifest, "build_request", timeout)?;
                Self::require_export(pool, manifest, "parse_response", timeout)?;
            }
            crate::shared::config::types::PluginType::Guardrail => {
                let has_request =
                    pool.function_exists("inspect_request", timeout)
                        .map_err(|e| {
                            crate::Error::Internal(format!(
                                "Plugin '{}' failed validation for 'inspect_request': {}",
                                manifest.name, e
                            ))
                        })?;
                let has_response =
                    pool.function_exists("inspect_response", timeout)
                        .map_err(|e| {
                            crate::Error::Internal(format!(
                                "Plugin '{}' failed validation for 'inspect_response': {}",
                                manifest.name, e
                            ))
                        })?;
                if !has_request && !has_response {
                    return Err(crate::Error::InvalidInput(format!(
                        "Guardrail plugin '{}' must export 'inspect_request' and/or 'inspect_response'",
                        manifest.name
                    )));
                }
            }
            crate::shared::config::types::PluginType::Health => {}
            crate::shared::config::types::PluginType::Observability => {
                Self::require_export(pool, manifest, "on_route", timeout)?;
            }
        }

        Ok(())
    }

    fn require_export(
        pool: &extism::Pool,
        manifest: &crate::shared::config::types::PluginManifest,
        export: &str,
        timeout: Duration,
    ) -> crate::Result<()> {
        let exists = pool.function_exists(export, timeout).map_err(|e| {
            crate::Error::Internal(format!(
                "Plugin '{}' failed validation for '{}': {}",
                manifest.name, export, e
            ))
        })?;
        if !exists {
            return Err(crate::Error::InvalidInput(format!(
                "{:?} plugin '{}' must export a '{}' function",
                manifest.plugin_type, manifest.name, export
            )));
        }
        Ok(())
    }

    /// Unload a plugin by name
    pub fn unload_plugin(&self, name: &str) -> crate::Result<()> {
        if self.plugins.remove(name).is_none() {
            return Err(crate::Error::NotFound(format!("Plugin '{name}' not found")));
        }
        Ok(())
    }

    /// Reload a plugin by name, atomically swapping the pool
    pub fn reload_plugin(&self, name: &str) -> crate::Result<()> {
        match self.plugins.entry(name.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(mut entry) => {
                let manifest = entry.get().manifest.clone();
                let new_pool =
                    crate::features::plugin::infra::PluginRuntime::build_pool(&manifest)?;

                Self::validate_plugin_exports(&new_pool, &manifest)?;

                let now = chrono::Utc::now();
                entry.get_mut().pool = new_pool;
                entry.get_mut().loaded_at = now;
                entry.get_mut().call_count = std::sync::atomic::AtomicU64::new(0);
                entry.get_mut().error_count = std::sync::atomic::AtomicU64::new(0);
                Ok(())
            }
            dashmap::mapref::entry::Entry::Vacant(_) => {
                Err(crate::Error::NotFound(format!("Plugin '{name}' not found")))
            }
        }
    }

    /// Get the pool for a plugin by name
    #[must_use]
    pub fn get_pool(&self, name: &str) -> Option<Arc<extism::Pool>> {
        self.plugins.get(name).map(|entry| Arc::clone(&entry.pool))
    }

    /// List all loaded plugins
    #[must_use]
    pub fn list_plugins(&self) -> Vec<PluginInfo> {
        self.plugins
            .iter()
            .map(|entry| PluginInfo {
                name: entry.manifest.name.clone(),
                plugin_type: entry.manifest.plugin_type,
                wasm_path: entry.manifest.wasm_path.clone(),
                loaded_at: entry.loaded_at,
                call_count: entry.call_count.load(std::sync::atomic::Ordering::Relaxed),
                error_count: entry.error_count.load(std::sync::atomic::Ordering::Relaxed),
                active_instances: entry.pool.count(),
                max_instances: entry.manifest.max_instances,
            })
            .collect()
    }

    /// Get info for a single plugin
    #[must_use]
    pub fn plugin_info(&self, name: &str) -> Option<PluginInfo> {
        self.plugins.get(name).map(|entry| PluginInfo {
            name: entry.manifest.name.clone(),
            plugin_type: entry.manifest.plugin_type,
            wasm_path: entry.manifest.wasm_path.clone(),
            loaded_at: entry.loaded_at,
            call_count: entry.call_count.load(std::sync::atomic::Ordering::Relaxed),
            error_count: entry.error_count.load(std::sync::atomic::Ordering::Relaxed),
            active_instances: entry.pool.count(),
            max_instances: entry.manifest.max_instances,
        })
    }

    /// Check if a plugin exists
    #[must_use]
    pub fn plugin_exists(&self, name: &str) -> bool {
        self.plugins.contains_key(name)
    }

    /// Check if a plugin exports a specific function
    #[must_use]
    pub fn function_exists(&self, name: &str, function: &str) -> bool {
        self.plugins
            .get(name)
            .and_then(|entry| {
                entry
                    .pool
                    .function_exists(function, Duration::from_secs(1))
                    .ok()
            })
            .unwrap_or(false)
    }
}

impl Default for PluginManager {
    fn default() -> Self {
        Self::new()
    }
}
