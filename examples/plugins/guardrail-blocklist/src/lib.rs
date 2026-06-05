use thalamus_plugin::guardrail::{GuardrailResult, RequestGuardrailPlugin};
use thalamus_plugin::register_request_guardrail_plugin;
use thalamus_plugin::types::LlmRequest;

struct BlocklistGuardrail;

const BLOCKED_MODELS: &[&str] = &["forbidden", "restricted"];

impl RequestGuardrailPlugin for BlocklistGuardrail {
    fn inspect_request(&self, request: &LlmRequest) -> GuardrailResult {
        let model_lower = request.model.to_lowercase();
        if BLOCKED_MODELS.iter().any(|b| model_lower.contains(b)) {
            GuardrailResult {
                allow: false,
                reason: Some(format!("Model '{}' is on the blocklist", request.model)),
            }
        } else {
            GuardrailResult {
                allow: true,
                reason: None,
            }
        }
    }
}

register_request_guardrail_plugin!(BlocklistGuardrail);
