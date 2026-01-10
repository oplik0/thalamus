use std::sync::atomic::{AtomicUsize, Ordering};

use rand::distr::weighted::WeightedIndex;
use rand::prelude::Distribution;
use rand::seq::IndexedRandom;

use crate::features::backends::domain::EndpointSnapshot;
use crate::features::routing::domain::{RoutingContext, RoutingStrategy};

#[derive(Debug, Default)]
pub struct RoundRobinStrategy {
    cursor: AtomicUsize,
}

impl RoutingStrategy for RoundRobinStrategy {
    fn select(&self, ctx: &RoutingContext<'_>) -> Option<EndpointSnapshot> {
        if ctx.candidates.is_empty() {
            return None;
        }
        let index = self.cursor.fetch_add(1, Ordering::Relaxed) % ctx.candidates.len();
        ctx.candidates.get(index).cloned()
    }

    fn name(&self) -> &str {
        "round_robin"
    }
}

#[derive(Debug, Default)]
pub struct RandomStrategy;

impl RoutingStrategy for RandomStrategy {
    fn select(&self, ctx: &RoutingContext<'_>) -> Option<EndpointSnapshot> {
        let mut rng = rand::rng();
        ctx.candidates.choose(&mut rng).cloned()
    }

    fn name(&self) -> &str {
        "random"
    }
}

#[derive(Debug, Default)]
pub struct WeightedStrategy;

impl RoutingStrategy for WeightedStrategy {
    fn select(&self, ctx: &RoutingContext<'_>) -> Option<EndpointSnapshot> {
        if ctx.candidates.is_empty() {
            return None;
        }

        let mut rng = rand::rng();
        let weights: Vec<u32> = ctx
            .candidates
            .iter()
            .map(|endpoint| endpoint.weight.max(1))
            .collect();
        let index = WeightedIndex::new(weights).ok()?.sample(&mut rng);

        ctx.candidates.get(index).cloned()
    }

    fn name(&self) -> &str {
        "weighted"
    }
}

pub struct ModelAwareStrategy {
    delegate: Box<dyn RoutingStrategy>,
}

impl std::fmt::Debug for ModelAwareStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelAwareStrategy")
            .field("delegate", &self.delegate.name())
            .finish()
    }
}

impl ModelAwareStrategy {
    #[must_use]
    pub fn new(delegate: Box<dyn RoutingStrategy>) -> Self {
        Self { delegate }
    }
}

impl RoutingStrategy for ModelAwareStrategy {
    fn select(&self, ctx: &RoutingContext<'_>) -> Option<EndpointSnapshot> {
        let model = ctx.request.model();

        let loaded = ctx
            .candidates
            .iter()
            .filter(|endpoint| endpoint.has_loaded_model(model))
            .cloned()
            .collect::<Vec<_>>();

        if !loaded.is_empty() {
            let preferred_ctx = RoutingContext {
                request: ctx.request,
                candidates: &loaded,
            };
            return self.delegate.select(&preferred_ctx);
        }

        self.delegate.select(ctx)
    }

    fn name(&self) -> &str {
        "model_aware"
    }
}
