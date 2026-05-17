use std::sync::Arc;

use crate::Error;
use crate::Result;
use crate::features::backends::domain::{BackendRegistry, EndpointSnapshot};
use crate::features::plugin::PluginManager;
use crate::features::plugin::routing_bridge::{ExtismRoutingStrategy, DEFAULT_PLUGIN_TIMEOUT_MS};
use crate::features::routing::domain::{RoutingContext, RoutingStrategy};
use crate::features::routing::strategies::{
    HealthWeightedStrategy, LeastBusyStrategy, LeastConnectionsStrategy, ModelAwareStrategy,
    RandomStrategy, RoundRobinStrategy, WeightedStrategy,
};
use crate::shared::config::types::{RoutingConfig, StrategyConfig};
use crate::shared::models::LlmRequest;

pub struct RouterService {
    registry: Arc<dyn BackendRegistry>,
    strategy: Box<dyn RoutingStrategy>,
    fallback_strategy: Option<Box<dyn RoutingStrategy>>,
    admission_control: bool,
    plugin_manager: Option<Arc<PluginManager>>,
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
    pub fn from_config(
        registry: Arc<dyn BackendRegistry>,
        routing_config: &RoutingConfig,
        plugin_manager: Option<Arc<PluginManager>>,
    ) -> Self {
        let primary = strategy_from_config(&routing_config.strategy, &plugin_manager);

        let fallback_name = routing_config.strategy.fallback_strategy.trim();
        let fallback = if fallback_name.is_empty() {
            None
        } else {
            let fallback_config = StrategyConfig {
                name: fallback_name.to_string(),
                prefer_loaded_models: routing_config.strategy.prefer_loaded_models,
                consider_queue_depth: routing_config.strategy.consider_queue_depth,
                fallback_strategy: String::new(),
                hysteresis_threshold: routing_config.strategy.hysteresis_threshold,
                health_weighted: routing_config.strategy.health_weighted,
                admission_control: routing_config.strategy.admission_control,
            };
            Some(strategy_from_config(&fallback_config, &plugin_manager))
        };

        Self {
            registry,
            strategy: primary,
            fallback_strategy: fallback,
            admission_control: routing_config.strategy.admission_control,
            plugin_manager,
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

        // Step 4: Capacity-aware admission control
        if self.admission_control && candidates.iter().all(|e| e.active_requests >= e.capacity) {
            return Err(Error::ServiceUnavailable(format!(
                "All backend endpoints for model '{}' are at capacity",
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

fn strategy_from_config(
    config: &StrategyConfig,
    plugin_manager: &Option<Arc<PluginManager>>,
) -> Box<dyn RoutingStrategy> {
    // Check for plugin strategies first, before falling through to built-ins
    if let Some(pm) = plugin_manager {
        if pm.plugin_exists(&config.name) {
            if let Some(pool) = pm.get_pool(&config.name) {
                return Box::new(ExtismRoutingStrategy::new(
                    pool,
                    config.name.clone(),
                    DEFAULT_PLUGIN_TIMEOUT_MS,
                ));
            }
        }
    }

    let base: Box<dyn RoutingStrategy> = match config.name.as_str() {
        "random" => Box::<RandomStrategy>::default(),
        "weighted" => Box::<WeightedStrategy>::default(),
        "least_busy" | "model_aware_least_busy" => {
            Box::new(LeastBusyStrategy::new(config.hysteresis_threshold))
        }
        "least_connections" => Box::<LeastConnectionsStrategy>::default(),
        _ => Box::<RoundRobinStrategy>::default(),
    };

    // Composition order: base → HealthWeighted → ModelAware
    let with_health: Box<dyn RoutingStrategy> = if config.health_weighted {
        Box::new(HealthWeightedStrategy::new(base))
    } else {
        base
    };

    if config.prefer_loaded_models {
        Box::new(ModelAwareStrategy::new(with_health))
    } else {
        with_health
    }
}
                "least_connections" => Box::<LeastConnectionsStrategy>::default(),
                _ => Box::<RoundRobinStrategy>::default(),
            }
        }
    } else {
        match config.name.as_str() {
            "random" => Box::<RandomStrategy>::default(),
            "weighted" => Box::<WeightedStrategy>::default(),
            "least_busy" | "model_aware_least_busy" => {
                Box::new(LeastBusyStrategy::new(config.hysteresis_threshold))
            }
            "least_connections" => Box::<LeastConnectionsStrategy>::default(),
            _ => Box::<RoundRobinStrategy>::default(),
=======
    plugin_manager: &Option<Arc<PluginManager>>,
) -> Box<dyn RoutingStrategy> {
    // Check for plugin strategies first, before falling through to built-ins
    if let Some(pm) = plugin_manager {
        if pm.plugin_exists(&config.name) {
            if let Some(pool) = pm.get_pool(&config.name) {
                return Box::new(ExtismRoutingStrategy::new(
                    pool,
                    config.name.clone(),
                    DEFAULT_PLUGIN_TIMEOUT_MS,
                ));
            }
        }
    }

    let base: Box<dyn RoutingStrategy> = match config.name.as_str() {
        "random" => Box::<RandomStrategy>::default(),
        "weighted" => Box::<WeightedStrategy>::default(),
        "least_busy" | "model_aware_least_busy" => {
            Box::new(LeastBusyStrategy::new(config.hysteresis_threshold))
>>>>>>> ensemble/preserved/plugin-system/plugin-routing
        }
    };

    // Composition order: base → HealthWeighted → ModelAware
    let with_health: Box<dyn RoutingStrategy> = if config.health_weighted {
        Box::new(HealthWeightedStrategy::new(base))
    } else {
        base
    };

    if config.prefer_loaded_models {
        Box::new(ModelAwareStrategy::new(with_health))
    } else {
        with_health
    }
}
