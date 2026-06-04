use serde::{Deserialize, Serialize};

pub use crate::types::{Endpoint, EndpointId, LlmRequest};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingContext {
    pub request: LlmRequest,
    pub candidates: Vec<Endpoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingResult {
    pub endpoint_id: Option<EndpointId>,
}

pub trait RoutingPlugin: Send + Sync {
    fn select(&self, ctx: &RoutingContext) -> Option<EndpointId>;
}

#[macro_export]
macro_rules! register_routing_plugin {
    ($plugin:expr) => {
        #[$crate::extism_pdk::plugin_fn]
        pub fn select(
            $crate::extism_pdk::Json(ctx): $crate::extism_pdk::Json<
                $crate::routing::RoutingContext,
            >,
        ) -> $crate::extism_pdk::FnResult<$crate::extism_pdk::Json<$crate::routing::RoutingResult>>
        {
            let plugin = $plugin;
            let selected = plugin.select(&ctx);
            Ok($crate::extism_pdk::Json($crate::routing::RoutingResult {
                endpoint_id: selected,
            }))
        }
    };
}
