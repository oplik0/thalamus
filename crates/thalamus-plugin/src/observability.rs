use serde::{Deserialize, Serialize};

pub use crate::types::{EndpointId, LlmRequest};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingEvent {
    pub request_model: String,
    pub selected_endpoint: Option<EndpointId>,
    pub duration_ms: u64,
}

pub trait ObservabilityPlugin: Send + Sync {
    fn on_route(&self, event: &RoutingEvent);
}

#[macro_export]
macro_rules! register_observability_plugin {
    ($plugin:expr) => {
        #[$crate::extism_pdk::plugin_fn]
        pub fn on_route(
            $crate::extism_pdk::Json(event): $crate::extism_pdk::Json<
                $crate::observability::RoutingEvent,
            >,
        ) -> $crate::extism_pdk::FnResult<()> {
            let plugin = $plugin;
            plugin.on_route(&event);
            Ok(())
        }
    };
}
