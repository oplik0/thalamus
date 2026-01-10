use std::sync::Arc;

use crate::Error;
use crate::Result;
use crate::features::backends::domain::{BackendRegistry, EndpointSnapshot};
use crate::features::routing::domain::{RoutingContext, RoutingStrategy};
use crate::features::routing::strategies::{
    ModelAwareStrategy, RandomStrategy, RoundRobinStrategy, WeightedStrategy,
};
use crate::shared::config::types::RoutingConfig;
use crate::shared::models::LlmRequest;

pub struct RouterService {
    registry: Arc<dyn BackendRegistry>,
    strategy: Box<dyn RoutingStrategy>,
    fallback_strategy: Option<Box<dyn RoutingStrategy>>,
}

impl std::fmt::Debug for RouterService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouterService")
            .field("strategy", &self.strategy.name())
            .finish_non_exhaustive()
    }
}

impl RouterService {
    #[must_use]
    pub fn from_config(registry: Arc<dyn BackendRegistry>, routing_config: &RoutingConfig) -> Self {
        let primary = strategy_from_name(
            &routing_config.strategy.name,
            routing_config.strategy.prefer_loaded_models,
        );

        let fallback_name = routing_config.strategy.fallback_strategy.trim();
        let fallback = if fallback_name.is_empty() {
            None
        } else {
            Some(strategy_from_name(
                fallback_name,
                routing_config.strategy.prefer_loaded_models,
            ))
        };

        Self {
            registry,
            strategy: primary,
            fallback_strategy: fallback,
        }
    }

    pub fn route(&self, request: &LlmRequest) -> Result<EndpointSnapshot> {
        let candidates = self.registry.endpoints_for_model(request.model());
        if candidates.is_empty() {
            return Err(Error::InvalidInput(format!(
                "No healthy backend endpoint supports model '{}'",
                request.model()
            )));
        }

        let ctx = RoutingContext {
            request,
            candidates: &candidates,
        };

        if let Some(endpoint) = self.strategy.select(&ctx) {
            return Ok(endpoint);
        }

        if let Some(fallback) = &self.fallback_strategy {
            if let Some(endpoint) = fallback.select(&ctx) {
                return Ok(endpoint);
            }
        }

        Err(Error::Backend(
            "Unable to select a backend endpoint with configured routing strategies".to_string(),
        ))
    }
}

fn strategy_from_name(name: &str, prefer_loaded_models: bool) -> Box<dyn RoutingStrategy> {
    let base: Box<dyn RoutingStrategy> = match name {
        "random" => Box::<RandomStrategy>::default(),
        "weighted" => Box::<WeightedStrategy>::default(),
        _ => Box::<RoundRobinStrategy>::default(),
    };

    if prefer_loaded_models {
        Box::new(ModelAwareStrategy::new(base))
    } else {
        base
    }
}
