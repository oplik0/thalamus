use std::sync::Arc;

use crate::shared::config::types::BackendConfig;

use crate::features::backends::domain::BackendAdapter;

pub mod openai;
pub mod ollama;

#[must_use]
pub fn adapter_for_backend(
    backend_name: &str,
    _config: &BackendConfig,
) -> Arc<dyn BackendAdapter> {
    if backend_name == "ollama" {
        Arc::new(ollama::OllamaAdapter)
    } else {
        Arc::new(openai::OpenAiAdapter)
    }
}
