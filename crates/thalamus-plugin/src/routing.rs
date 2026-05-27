use serde::{Deserialize, Serialize};

pub use crate::types::{Endpoint, EndpointId, LlmRequest};

#[derive(Debug, Clone, Deserialize)]
pub struct RoutingContext {
    pub request: LlmRequest,
    pub candidates: Vec<Endpoint>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RoutingResult {
    pub endpoint_id: Option<EndpointId>,
}

pub trait RoutingPlugin: Send + Sync {
    fn select(&self, ctx: &RoutingContext) -> Option<EndpointId>;
}

#[macro_export]
macro_rules! register_routing_plugin {
    ($plugin:expr) => {
        #[extism_pdk::plugin_fn]
        pub fn select(
            extism_pdk::Json(ctx): extism_pdk::Json<$crate::routing::RoutingContext>,
        ) -> extism_pdk::FnResult<extism_pdk::Json<$crate::routing::RoutingResult>> {
            let plugin = $plugin;
            let selected = plugin.select(&ctx);
            Ok(extism_pdk::Json($crate::routing::RoutingResult {
                endpoint_id: selected,
            }))
        }
    };
}
