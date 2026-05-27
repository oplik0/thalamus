use serde::{Deserialize, Serialize};

pub use crate::types::{ChatResponse, LlmRequest};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuardrailResult {
    pub allow: bool,
    pub reason: Option<String>,
}

pub trait RequestGuardrailPlugin: Send + Sync {
    fn inspect_request(&self, request: &LlmRequest) -> GuardrailResult;
}

pub trait ResponseGuardrailPlugin: Send + Sync {
    fn inspect_response(&self, response: &ChatResponse) -> GuardrailResult;
}

#[macro_export]
macro_rules! register_request_guardrail_plugin {
    ($plugin:expr) => {
        #[extism_pdk::plugin_fn]
        pub fn inspect_request(
            extism_pdk::Json(request): extism_pdk::Json<$crate::types::LlmRequest>,
        ) -> extism_pdk::FnResult<extism_pdk::Json<$crate::guardrail::GuardrailResult>> {
            let plugin = $plugin;
            Ok(extism_pdk::Json(plugin.inspect_request(&request)))
        }
    };
}

#[macro_export]
macro_rules! register_response_guardrail_plugin {
    ($plugin:expr) => {
        #[extism_pdk::plugin_fn]
        pub fn inspect_response(
            extism_pdk::Json(response): extism_pdk::Json<$crate::types::ChatResponse>,
        ) -> extism_pdk::FnResult<extism_pdk::Json<$crate::guardrail::GuardrailResult>> {
            let plugin = $plugin;
            Ok(extism_pdk::Json(plugin.inspect_response(&response)))
        }
    };
}
