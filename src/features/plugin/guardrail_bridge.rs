use std::sync::Arc;
use std::time::Duration;

use extism::Pool;

use crate::Error;
use crate::Result;
use crate::shared::config::types::PluginType;
use crate::shared::models::{ChatResponse, LlmRequest};

/// Default timeout for guardrail plugin calls in milliseconds.
pub const DEFAULT_GUARDRAIL_TIMEOUT_MS: u64 = 200;

/// A collection of guardrail plugins that can inspect requests and responses.
#[derive(Clone)]
pub struct GuardrailService {
    plugins: Vec<GuardrailPluginHandle>,
    timeout: Duration,
}

impl std::fmt::Debug for GuardrailService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuardrailService")
            .field("plugins", &self.plugins.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
struct GuardrailPluginHandle {
    pool: Arc<Pool>,
    name: String,
    plugin_type: GuardrailPluginType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuardrailPluginType {
    Request,
    Response,
}

impl GuardrailService {
    #[must_use]
    pub fn empty() -> Self {
        Self {
            plugins: Vec::new(),
            timeout: Duration::from_millis(DEFAULT_GUARDRAIL_TIMEOUT_MS),
        }
    }

    #[must_use]
    pub fn from_plugin_manager(
        plugin_manager: Option<&crate::features::plugin::PluginManager>,
        timeout_ms: u64,
    ) -> Self {
        let mut plugins = Vec::new();
        if let Some(pm) = plugin_manager {
            for info in pm.list_plugins() {
                if info.plugin_type == PluginType::Guardrail {
                    let plugin_type = if pm.function_exists(&info.name, "inspect_request") {
                        GuardrailPluginType::Request
                    } else if pm.function_exists(&info.name, "inspect_response") {
                        GuardrailPluginType::Response
                    } else {
                        continue;
                    };

                    if let Some(pool) = pm.get_pool(&info.name) {
                        plugins.push(GuardrailPluginHandle {
                            pool,
                            name: info.name.clone(),
                            plugin_type,
                        });
                    }
                }
            }
        }

        Self {
            plugins,
            timeout: Duration::from_millis(timeout_ms),
        }
    }

    /// Inspect an incoming request. Returns `Ok(())` if all request guardrails
    /// allow the request, or `Err(Error::Guardrail(...))` if any block it.
    pub fn inspect_request(&self, request: &LlmRequest) -> Result<()> {
        for handle in &self.plugins {
            if handle.plugin_type != GuardrailPluginType::Request {
                continue;
            }

            let plugin_request = thalamus_plugin::types::LlmRequest {
                model: request.model().to_string(),
            };
            let input_json = serde_json::to_string(&plugin_request).map_err(|e| {
                Error::Internal(format!(
                    "Guardrail '{}' serialization failed: {}",
                    handle.name, e
                ))
            })?;

            let mut plugin = self.get_plugin(handle)?;
            let output: String = plugin
                .call::<&str, String>("inspect_request", &input_json)
                .map_err(|e| {
                    Error::Backend(format!(
                        "Guardrail '{}' inspect_request failed: {}",
                        handle.name, e
                    ))
                })?;
            let result: thalamus_plugin::guardrail::GuardrailResult = serde_json::from_str(&output)
                .map_err(|e| {
                    Error::Backend(format!(
                        "Guardrail '{}' returned invalid result: {}",
                        handle.name, e
                    ))
                })?;

            if !result.allow {
                return Err(Error::Guardrail(format!(
                    "Guardrail '{}' blocked request{}",
                    handle.name,
                    result.reason.map(|r| format!(": {r}")).unwrap_or_default()
                )));
            }
        }

        Ok(())
    }

    /// Inspect a backend response. Returns `Ok(())` if all response guardrails
    /// allow the response, or `Err(Error::Guardrail(...))` if any block it.
    pub fn inspect_response(&self, response: &ChatResponse) -> Result<()> {
        for handle in &self.plugins {
            if handle.plugin_type != GuardrailPluginType::Response {
                continue;
            }

            let plugin_response = thalamus_plugin::types::ChatResponse {
                model: response.model.clone(),
                status: response.status.as_ref().map(|s| match s {
                    crate::shared::models::ResponseStatus::Completed => "completed".to_string(),
                    crate::shared::models::ResponseStatus::InProgress => "in_progress".to_string(),
                    crate::shared::models::ResponseStatus::Incomplete => "incomplete".to_string(),
                    crate::shared::models::ResponseStatus::Failed => "failed".to_string(),
                }),
            };
            let input_json = serde_json::to_string(&plugin_response).map_err(|e| {
                Error::Internal(format!(
                    "Guardrail '{}' serialization failed: {}",
                    handle.name, e
                ))
            })?;

            let mut plugin = self.get_plugin(handle)?;
            let output: String = plugin
                .call::<&str, String>("inspect_response", &input_json)
                .map_err(|e| {
                    Error::Backend(format!(
                        "Guardrail '{}' inspect_response failed: {}",
                        handle.name, e
                    ))
                })?;
            let result: thalamus_plugin::guardrail::GuardrailResult = serde_json::from_str(&output)
                .map_err(|e| {
                    Error::Backend(format!(
                        "Guardrail '{}' returned invalid result: {}",
                        handle.name, e
                    ))
                })?;

            if !result.allow {
                return Err(Error::Guardrail(format!(
                    "Guardrail '{}' blocked response{}",
                    handle.name,
                    result.reason.map(|r| format!(": {r}")).unwrap_or_default()
                )));
            }
        }

        Ok(())
    }

    fn get_plugin(&self, handle: &GuardrailPluginHandle) -> Result<extism::PoolPlugin> {
        handle
            .pool
            .get(self.timeout)
            .map_err(|e| Error::Backend(format!("Guardrail '{}' unavailable: {}", handle.name, e)))?
            .ok_or_else(|| Error::Backend(format!("Guardrail '{}' timed out", handle.name)))
    }
}
