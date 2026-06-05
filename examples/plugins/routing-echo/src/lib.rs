use thalamus_plugin::register_routing_plugin;
use thalamus_plugin::routing::{EndpointId, RoutingContext, RoutingPlugin};

struct EchoPlugin;

impl RoutingPlugin for EchoPlugin {
    fn select(&self, ctx: &RoutingContext) -> Option<EndpointId> {
        ctx.candidates.first().map(|c| c.id.clone())
    }
}

register_routing_plugin!(EchoPlugin);
