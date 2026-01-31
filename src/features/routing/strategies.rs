use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

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

// --- Step 1: LeastBusyStrategy ---

fn endpoint_hash(endpoint: &EndpointSnapshot) -> u64 {
    let mut hasher = DefaultHasher::new();
    endpoint.id.hash(&mut hasher);
    hasher.finish()
}

pub struct LeastBusyStrategy {
    hysteresis_threshold: f64,
    last_selected: AtomicU64,
}

impl std::fmt::Debug for LeastBusyStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeastBusyStrategy")
            .field("hysteresis_threshold", &self.hysteresis_threshold)
            .finish()
    }
}

impl LeastBusyStrategy {
    #[must_use]
    pub fn new(hysteresis_threshold: f64) -> Self {
        Self {
            hysteresis_threshold,
            last_selected: AtomicU64::new(0),
        }
    }

    fn load_ratio(endpoint: &EndpointSnapshot) -> f64 {
        let capacity = endpoint.capacity.max(1) as f64;
        endpoint.active_requests as f64 / capacity
    }
}

impl RoutingStrategy for LeastBusyStrategy {
    fn select(&self, ctx: &RoutingContext<'_>) -> Option<EndpointSnapshot> {
        if ctx.candidates.is_empty() {
            return None;
        }

        let last_hash = self.last_selected.load(Ordering::Relaxed);

        // Find the best candidate (lowest load ratio, tie-break by weight desc)
        let best = ctx.candidates.iter().min_by(|a, b| {
            let ratio_a = Self::load_ratio(a);
            let ratio_b = Self::load_ratio(b);
            ratio_a
                .partial_cmp(&ratio_b)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.weight.cmp(&a.weight))
        })?;

        // Check if the last-selected endpoint is still a candidate
        if let Some(current) = ctx
            .candidates
            .iter()
            .find(|e| endpoint_hash(e) == last_hash)
        {
            let current_ratio = Self::load_ratio(current);
            let best_ratio = Self::load_ratio(best);

            // Hysteresis: only switch if the improvement exceeds the threshold
            if current_ratio - best_ratio <= self.hysteresis_threshold {
                return Some(current.clone());
            }
        }

        self.last_selected
            .store(endpoint_hash(best), Ordering::Relaxed);
        Some(best.clone())
    }

    fn name(&self) -> &str {
        "least_busy"
    }
}

// --- Step 2: LeastConnectionsStrategy ---

#[derive(Debug, Default)]
pub struct LeastConnectionsStrategy;

impl RoutingStrategy for LeastConnectionsStrategy {
    fn select(&self, ctx: &RoutingContext<'_>) -> Option<EndpointSnapshot> {
        ctx.candidates
            .iter()
            .min_by_key(|e| e.active_requests)
            .cloned()
    }

    fn name(&self) -> &str {
        "least_connections"
    }
}

// --- Step 3: HealthWeightedStrategy (decorator) ---

pub struct HealthWeightedStrategy {
    delegate: Box<dyn RoutingStrategy>,
}

impl std::fmt::Debug for HealthWeightedStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HealthWeightedStrategy")
            .field("delegate", &self.delegate.name())
            .finish()
    }
}

impl HealthWeightedStrategy {
    #[must_use]
    pub fn new(delegate: Box<dyn RoutingStrategy>) -> Self {
        Self { delegate }
    }

    fn health_factor(consecutive_failures: u32) -> f64 {
        // Each failure multiplies by 0.7: factor = 0.7^failures
        0.7_f64.powi(consecutive_failures as i32)
    }
}

impl RoutingStrategy for HealthWeightedStrategy {
    fn select(&self, ctx: &RoutingContext<'_>) -> Option<EndpointSnapshot> {
        if ctx.candidates.is_empty() {
            return None;
        }

        let adjusted: Vec<EndpointSnapshot> = ctx
            .candidates
            .iter()
            .map(|endpoint| {
                let factor = Self::health_factor(endpoint.consecutive_failures);
                let original_weight = endpoint.weight.max(1) as f64;
                let min_weight = (0.1 * original_weight).max(1.0);
                let adjusted_weight = (original_weight * factor).max(min_weight) as u32;

                let mut adjusted_endpoint = endpoint.clone();
                adjusted_endpoint.weight = adjusted_weight.max(1);
                adjusted_endpoint
            })
            .collect();

        let adjusted_ctx = RoutingContext {
            request: ctx.request,
            candidates: &adjusted,
        };
        self.delegate.select(&adjusted_ctx)
    }

    fn name(&self) -> &str {
        "health_weighted"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::backends::domain::EndpointId;
    use crate::shared::models::{ChatRequest, LlmRequest};

    fn make_endpoint(
        backend: &str,
        index: usize,
        capacity: u32,
        active: u32,
        weight: u32,
    ) -> EndpointSnapshot {
        EndpointSnapshot {
            id: EndpointId {
                backend: backend.to_string(),
                index,
            },
            url: format!("http://{backend}/{index}"),
            models: vec!["model-a".to_string()],
            currently_loaded_models: vec!["model-a".to_string()],
            model_loading_aware: false,
            tags: vec![],
            weight,
            capacity,
            healthy: true,
            active_requests: active,
            consecutive_failures: 0,
            consecutive_successes: 0,
        }
    }

    fn make_request() -> LlmRequest {
        LlmRequest::Chat(ChatRequest::simple("model-a", "hello"))
    }

    // --- LeastBusyStrategy tests ---

    #[test]
    fn least_busy_selects_lowest_ratio() {
        let strategy = LeastBusyStrategy::new(0.10);
        let candidates = vec![
            make_endpoint("a", 0, 10, 5, 1), // ratio 0.5
            make_endpoint("b", 0, 10, 2, 1), // ratio 0.2
            make_endpoint("c", 0, 10, 8, 1), // ratio 0.8
        ];
        let request = make_request();
        let ctx = RoutingContext {
            request: &request,
            candidates: &candidates,
        };
        let selected = strategy.select(&ctx).unwrap();
        assert_eq!(selected.id.backend, "b");
    }

    #[test]
    fn least_busy_hysteresis_sticking() {
        let strategy = LeastBusyStrategy::new(0.10);
        let request = make_request();

        // First selection picks "b" (ratio 0.2)
        let candidates = vec![
            make_endpoint("a", 0, 10, 5, 1),
            make_endpoint("b", 0, 10, 2, 1),
        ];
        let ctx = RoutingContext {
            request: &request,
            candidates: &candidates,
        };
        let selected = strategy.select(&ctx).unwrap();
        assert_eq!(selected.id.backend, "b");

        // "b" ratio increases to 0.3, "a" drops to 0.25 — difference is only 0.05 < 0.10
        let candidates = vec![
            make_endpoint("a", 0, 100, 25, 1), // ratio 0.25
            make_endpoint("b", 0, 10, 3, 1),   // ratio 0.30
        ];
        let ctx = RoutingContext {
            request: &request,
            candidates: &candidates,
        };
        let selected = strategy.select(&ctx).unwrap();
        // Should stick with "b" due to hysteresis
        assert_eq!(selected.id.backend, "b");
    }

    #[test]
    fn least_busy_hysteresis_switching() {
        let strategy = LeastBusyStrategy::new(0.10);
        let request = make_request();

        // First selection picks "b" (ratio 0.2)
        let candidates = vec![
            make_endpoint("a", 0, 10, 5, 1),
            make_endpoint("b", 0, 10, 2, 1),
        ];
        let ctx = RoutingContext {
            request: &request,
            candidates: &candidates,
        };
        let selected = strategy.select(&ctx).unwrap();
        assert_eq!(selected.id.backend, "b");

        // Now "b" is heavily loaded (ratio 0.8) and "a" is light (0.1) — diff 0.7 > 0.10
        let candidates = vec![
            make_endpoint("a", 0, 10, 1, 1), // ratio 0.1
            make_endpoint("b", 0, 10, 8, 1), // ratio 0.8
        ];
        let ctx = RoutingContext {
            request: &request,
            candidates: &candidates,
        };
        let selected = strategy.select(&ctx).unwrap();
        assert_eq!(selected.id.backend, "a");
    }

    #[test]
    fn least_busy_empty_candidates() {
        let strategy = LeastBusyStrategy::new(0.10);
        let request = make_request();
        let ctx = RoutingContext {
            request: &request,
            candidates: &[],
        };
        assert!(strategy.select(&ctx).is_none());
    }

    #[test]
    fn least_busy_zero_capacity() {
        let strategy = LeastBusyStrategy::new(0.10);
        // capacity=0 is treated as 1 to avoid division by zero
        let candidates = vec![
            make_endpoint("a", 0, 0, 5, 1), // ratio = 5/1 = 5.0
            make_endpoint("b", 0, 0, 2, 1), // ratio = 2/1 = 2.0
        ];
        let request = make_request();
        let ctx = RoutingContext {
            request: &request,
            candidates: &candidates,
        };
        let selected = strategy.select(&ctx).unwrap();
        assert_eq!(selected.id.backend, "b");
    }

    #[test]
    fn least_busy_weight_tiebreak() {
        let strategy = LeastBusyStrategy::new(0.10);
        // Same ratio, different weights — higher weight wins
        let candidates = vec![
            make_endpoint("a", 0, 10, 2, 5),  // ratio 0.2, weight 5
            make_endpoint("b", 0, 10, 2, 10), // ratio 0.2, weight 10
        ];
        let request = make_request();
        let ctx = RoutingContext {
            request: &request,
            candidates: &candidates,
        };
        let selected = strategy.select(&ctx).unwrap();
        assert_eq!(selected.id.backend, "b");
    }

    // --- LeastConnectionsStrategy tests ---

    #[test]
    fn least_connections_selects_fewest() {
        let strategy = LeastConnectionsStrategy;
        let candidates = vec![
            make_endpoint("a", 0, 100, 10, 1),
            make_endpoint("b", 0, 100, 3, 1),
            make_endpoint("c", 0, 100, 7, 1),
        ];
        let request = make_request();
        let ctx = RoutingContext {
            request: &request,
            candidates: &candidates,
        };
        let selected = strategy.select(&ctx).unwrap();
        assert_eq!(selected.id.backend, "b");
    }

    #[test]
    fn least_connections_deterministic_tie() {
        let strategy = LeastConnectionsStrategy;
        let candidates = vec![
            make_endpoint("a", 0, 100, 5, 1),
            make_endpoint("b", 0, 100, 5, 1),
        ];
        let request = make_request();
        let ctx = RoutingContext {
            request: &request,
            candidates: &candidates,
        };
        // min_by_key is stable, so first element wins on tie
        let selected = strategy.select(&ctx).unwrap();
        assert_eq!(selected.id.backend, "a");
    }

    #[test]
    fn least_connections_empty_candidates() {
        let strategy = LeastConnectionsStrategy;
        let request = make_request();
        let ctx = RoutingContext {
            request: &request,
            candidates: &[],
        };
        assert!(strategy.select(&ctx).is_none());
    }

    // --- HealthWeightedStrategy tests ---

    #[test]
    fn health_weighted_reduces_weight_with_failures() {
        // Use a deterministic delegate that picks the highest weight
        let delegate = Box::new(WeightedPickHighest);
        let strategy = HealthWeightedStrategy::new(delegate);

        let mut endpoint_a = make_endpoint("a", 0, 10, 0, 100);
        endpoint_a.consecutive_failures = 3; // factor = 0.7^3 = 0.343 → weight ~34

        let endpoint_b = make_endpoint("b", 0, 10, 0, 50);
        // factor = 1.0 → weight 50

        let candidates = vec![endpoint_a, endpoint_b];
        let request = make_request();
        let ctx = RoutingContext {
            request: &request,
            candidates: &candidates,
        };
        let selected = strategy.select(&ctx).unwrap();
        // "b" has adjusted weight 50 > "a" adjusted weight ~34
        assert_eq!(selected.id.backend, "b");
    }

    #[test]
    fn health_weighted_minimum_floor() {
        let factor = HealthWeightedStrategy::health_factor(100);
        // 0.7^100 is extremely small
        let original_weight = 100.0_f64;
        let min_weight = (0.1 * original_weight).max(1.0);
        let adjusted = (original_weight * factor).max(min_weight);
        // Should floor at 10% of original
        assert!((adjusted - 10.0).abs() < 0.001);
    }

    #[test]
    fn health_weighted_passthrough_zero_failures() {
        let delegate = Box::new(WeightedPickHighest);
        let strategy = HealthWeightedStrategy::new(delegate);

        let endpoint_a = make_endpoint("a", 0, 10, 0, 100);
        let endpoint_b = make_endpoint("b", 0, 10, 0, 50);

        let candidates = vec![endpoint_a, endpoint_b];
        let request = make_request();
        let ctx = RoutingContext {
            request: &request,
            candidates: &candidates,
        };
        let selected = strategy.select(&ctx).unwrap();
        // With zero failures, weights are unchanged; "a" has higher weight
        assert_eq!(selected.id.backend, "a");
    }

    /// Test delegate that deterministically picks the candidate with the highest weight.
    #[derive(Debug)]
    struct WeightedPickHighest;

    impl RoutingStrategy for WeightedPickHighest {
        fn select(&self, ctx: &RoutingContext<'_>) -> Option<EndpointSnapshot> {
            ctx.candidates.iter().max_by_key(|e| e.weight).cloned()
        }

        fn name(&self) -> &str {
            "weighted_pick_highest"
        }
    }
}
