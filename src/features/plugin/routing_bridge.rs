use std::sync::Arc;
use std::time::Duration;

use extism::Pool;

use crate::features::backends::domain::EndpointSnapshot;
use crate::features::routing::domain::{RoutingContext, RoutingStrategy};
use crate::shared::models::LlmRequest;
use thalamus_plugin::routing::{
    RoutingContext as PluginRoutingContext, RoutingResult as PluginRoutingResult,
};
use thalamus_plugin::types::{
    Endpoint as PluginEndpoint, EndpointId as PluginEndpointId, LlmRequest as PluginLlmRequest,
};

/// Default timeout for plugin calls in milliseconds.
pub const DEFAULT_PLUGIN_TIMEOUT_MS: u64 = 500;

/// A `RoutingStrategy` implementation that delegates to an Extism plugin.
pub struct ExtismRoutingStrategy {
    pool: Arc<Pool>,
    name: String,
    timeout: Duration,
}

impl ExtismRoutingStrategy {
    #[must_use]
    pub fn new(pool: Arc<Pool>, name: String, timeout_ms: u64) -> Self {
        Self {
            pool,
            name,
            timeout: Duration::from_millis(timeout_ms),
        }
    }
}

impl std::fmt::Debug for ExtismRoutingStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtismRoutingStrategy")
            .field("name", &self.name)
            .finish()
    }
}

fn to_plugin_endpoint(snapshot: &EndpointSnapshot) -> PluginEndpoint {
    PluginEndpoint {
        id: PluginEndpointId {
            backend: snapshot.id.backend.clone(),
            index: snapshot.id.index,
        },
        url: snapshot.url.clone(),
        models: snapshot.models.clone(),
        currently_loaded_models: snapshot.currently_loaded_models.clone(),
        model_loading_aware: snapshot.model_loading_aware,
        tags: snapshot.tags.clone(),
        weight: snapshot.weight,
        capacity: snapshot.capacity,
        healthy: snapshot.healthy,
        active_requests: snapshot.active_requests,
        consecutive_failures: snapshot.consecutive_failures,
        consecutive_successes: snapshot.consecutive_successes,
    }
}

fn to_plugin_request(request: &LlmRequest) -> PluginLlmRequest {
    PluginLlmRequest {
        model: request.model().to_string(),
    }
}

impl RoutingStrategy for ExtismRoutingStrategy {
    fn select(&self, ctx: &RoutingContext<'_>) -> Option<EndpointSnapshot> {
        let plugin_ctx = PluginRoutingContext {
            request: to_plugin_request(ctx.request),
            candidates: ctx.candidates.iter().map(to_plugin_endpoint).collect(),
        };
        let input_json = serde_json::to_string(&plugin_ctx).ok()?;

        // Use pool to get exclusive mutable access to a plugin instance
        let mut plugin = self.pool.get(self.timeout).ok()??;
        let output: String = plugin.call::<&str, String>("select", &input_json).ok()?;
        let result: PluginRoutingResult = serde_json::from_str(&output).ok()?;

        let endpoint_id = result.endpoint_id?;
        ctx.candidates
            .iter()
            .find(|c| c.id.backend == endpoint_id.backend && c.id.index == endpoint_id.index)
            .cloned()
    }

    fn name(&self) -> &str {
        &self.name
    }
}
