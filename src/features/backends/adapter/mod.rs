use std::sync::Arc;

use crate::shared::config::types::BackendConfig;

use crate::features::backends::domain::BackendAdapter;

pub mod openai;

#[must_use]
pub fn adapter_for_backend(
    _backend_name: &str,
    _config: &BackendConfig,
) -> Arc<dyn BackendAdapter> {
    Arc::new(openai::OpenAiAdapter)
}
