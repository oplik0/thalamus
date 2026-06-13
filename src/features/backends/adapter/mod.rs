use std::sync::Arc;

use crate::features::backends::domain::BackendAdapter;
use crate::features::plugin::PluginManager;
use crate::features::plugin::adapter_bridge::ExtismBackendAdapter;
use crate::shared::config::types::{BackendConfig, PluginType};

pub mod openai;

const DEFAULT_ADAPTER_TIMEOUT_MS: u64 = 5000;

#[must_use]
pub fn adapter_for_backend(
    backend_name: &str,
    _config: &BackendConfig,
    plugin_manager: Option<&PluginManager>,
) -> Arc<dyn BackendAdapter> {
    if let Some(pm) = plugin_manager
        && let Some(info) = pm.plugin_info(backend_name)
        && info.plugin_type == PluginType::Adapter
        && let Some(pool) = pm.get_pool(backend_name)
    {
        return Arc::new(ExtismBackendAdapter::new(
            pool,
            backend_name.to_string(),
            DEFAULT_ADAPTER_TIMEOUT_MS,
        ));
    }

    Arc::new(openai::OpenAiAdapter)
}
