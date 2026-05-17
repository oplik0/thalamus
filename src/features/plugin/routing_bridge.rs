use std::sync::Arc;
use std::time::Duration;

use extism::Pool;
use serde::{Deserialize, Serialize};

use crate::features::backends::domain::EndpointSnapshot;
use crate::features::routing::domain::{RoutingContext, RoutingStrategy};

/// Default timeout for plugin calls in milliseconds.
pub const DEFAULT_PLUGIN_TIMEOUT_MS: u64 = 500;

/// A `RoutingStrategy` implementation that delegates to an Extism plugin.
pub struct ExtismRoutingStrategy {
    pool: Arc<Pool>,
    name: String,
    timeout: Duration,
}

impl ExtismRoutingStrategy {
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

// JSON schemas for the plugin boundary
#[derive(Serialize)]
struct PluginRoutingContext<'a> {
    request: &'a crate::shared::models::LlmRequest,
    candidates: &'a [EndpointSnapshot],
}

#[derive(Deserialize)]
struct PluginRoutingResult {
    endpoint_id: Option<PluginEndpointId>,
}

#[derive(Deserialize)]
struct PluginEndpointId {
    backend: String,
    index: usize,
}

impl RoutingStrategy for ExtismRoutingStrategy {
    fn select(&self, ctx: &RoutingContext<'_>) -> Option<EndpointSnapshot> {
        let input = PluginRoutingContext {
            request: ctx.request,
            candidates: ctx.candidates,
        };
        let input_json = serde_json::to_string(&input).ok()?;

        // Use pool to get exclusive mutable access to a plugin instance
        let mut plugin = self.pool.get(self.timeout).ok()??;
        let output: &str = plugin.call::<&str, &str>("select", &input_json).ok()?;
        let result: PluginRoutingResult = serde_json::from_str(output).ok()?;

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
