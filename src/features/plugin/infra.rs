use std::sync::Arc;
use std::time::Duration;

/// Internal entry for a loaded plugin
pub struct PluginEntry {
    pub pool: Arc<extism::Pool>,
    pub manifest: crate::features::plugin::domain::PluginManifest,
    pub loaded_at: chrono::DateTime<chrono::Utc>,
    pub call_count: std::sync::atomic::AtomicU64,
    pub error_count: std::sync::atomic::AtomicU64,
}

impl std::fmt::Debug for PluginEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginEntry")
            .field("manifest", &self.manifest)
            .field("loaded_at", &self.loaded_at)
            .field(
                "call_count",
                &self.call_count.load(std::sync::atomic::Ordering::Relaxed),
            )
            .field(
                "error_count",
                &self.error_count.load(std::sync::atomic::Ordering::Relaxed),
            )
            .finish_non_exhaustive()
    }
}

/// Runtime for building Extism plugin pools
#[derive(Debug)]
pub struct PluginRuntime;

impl PluginRuntime {
    /// Build an Extism pool from a plugin manifest
    pub fn build_pool(
        manifest: &crate::features::plugin::domain::PluginManifest,
    ) -> crate::Result<Arc<extism::Pool>> {
        let extism_manifest = extism::Manifest::new([extism::Wasm::file(&manifest.wasm_path)])
            .with_timeout(Duration::from_millis(manifest.timeout_ms))
            .with_config(manifest.config.iter().map(|(k, v)| (k.clone(), v.clone())));

        let wasi = manifest.wasi;

        let pool = extism::PoolBuilder::default()
            .with_max_instances(manifest.max_instances)
            .build(move || {
                extism::PluginBuilder::new(extism_manifest.clone())
                    .with_wasi(wasi)
                    .build()
            });

        Ok(Arc::new(pool))
    }
}
