//! Priority queue system for request routing
//!
//! When all backend endpoints are at capacity, requests are queued by priority
//! and dispatched when capacity becomes available.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, oneshot};
use tokio_util::sync::CancellationToken;

use crate::Result;
use crate::features::backends::domain::EndpointSnapshot;
use crate::shared::config::types::RoutingConfig;
use crate::shared::models::LlmRequest;

/// Priority levels for request queuing, lower value = higher priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Priority {
    Critical = 0,
    Realtime = 1,
    Interactive = 2,
    Batch = 3,
    Background = 4,
}

impl Priority {
    /// Number of priority levels.
    pub const COUNT: usize = 5;

    #[must_use]
    pub fn from_name(name: &str) -> Self {
        match name.to_ascii_lowercase().as_str() {
            "critical" => Self::Critical,
            "realtime" => Self::Realtime,
            "interactive" => Self::Interactive,
            "batch" => Self::Batch,
            "background" => Self::Background,
            _ => Self::Interactive,
        }
    }

    /// Return the next lower priority level (for aging/promotion).
    #[must_use]
    pub fn promoted(self) -> Self {
        match self {
            Self::Critical => Self::Critical,
            Self::Realtime => Self::Critical,
            Self::Interactive => Self::Realtime,
            Self::Batch => Self::Interactive,
            Self::Background => Self::Batch,
        }
    }
}

struct QueuedRequest {
    request: LlmRequest,
    effective_priority: Priority,
    enqueued_at: Instant,
    last_aged_at: Instant,
    responder: oneshot::Sender<Result<EndpointSnapshot>>,
}

/// Manages priority queues for requests waiting for backend capacity.
pub struct PriorityQueueManager {
    queues: Mutex<[VecDeque<QueuedRequest>; Priority::COUNT]>,
    max_queue_size: u32,
    queue_timeout: Duration,
    aging_interval: Duration,
}

impl std::fmt::Debug for PriorityQueueManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PriorityQueueManager")
            .field("max_queue_size", &self.max_queue_size)
            .field("queue_timeout", &self.queue_timeout)
            .field("aging_interval", &self.aging_interval)
            .finish()
    }
}

impl PriorityQueueManager {
    #[must_use]
    pub fn from_config(config: &RoutingConfig) -> Self {
        // Use the default queue config for timeout and size limits
        let default_queue = config
            .priority_queues
            .get(&config.default_queue)
            .or_else(|| config.priority_queues.values().next());

        let (max_queue_size, queue_timeout) = match default_queue {
            Some(q) => {
                let timeout = crate::shared::utils::parse_duration_or_default(
                    &q.timeout,
                    Duration::from_secs(30),
                );
                (q.max_queue_size, timeout)
            }
            None => (100, Duration::from_secs(30)),
        };

        Self {
            queues: Mutex::new(std::array::from_fn(|_| VecDeque::new())),
            max_queue_size,
            queue_timeout,
            aging_interval: Duration::from_secs(5),
        }
    }

    /// Total number of requests currently in all queues.
    pub async fn total_queued(&self) -> usize {
        let queues = self.queues.lock().await;
        queues.iter().map(VecDeque::len).sum()
    }

    /// Enqueue a request. Returns a receiver that will yield the selected endpoint
    /// when one becomes available, or an error on timeout/queue-full.
    pub async fn enqueue(
        &self,
        request: LlmRequest,
        priority: Priority,
    ) -> std::result::Result<oneshot::Receiver<Result<EndpointSnapshot>>, crate::Error> {
        let mut queues = self.queues.lock().await;
        let queue = &queues[priority as usize];

        if queue.len() >= self.max_queue_size as usize {
            return Err(crate::Error::ServiceUnavailable(
                "Request queue is full".to_string(),
            ));
        }

        let (tx, rx) = oneshot::channel();
        let now = Instant::now();
        queues[priority as usize].push_back(QueuedRequest {
            request,
            effective_priority: priority,
            enqueued_at: now,
            last_aged_at: now,
            responder: tx,
        });

        Ok(rx)
    }

    /// Try to dequeue the highest-priority request and route it.
    /// Called when an endpoint is released.
    pub async fn try_dispatch(&self, route_fn: &dyn Fn(&LlmRequest) -> Result<EndpointSnapshot>) {
        let mut queues = self.queues.lock().await;
        let now = Instant::now();

        // Iterate from highest priority (0) to lowest
        for priority_idx in 0..Priority::COUNT {
            // Remove expired entries first
            let mut i = 0;
            while i < queues[priority_idx].len() {
                if now.duration_since(queues[priority_idx][i].enqueued_at) > self.queue_timeout {
                    if let Some(entry) = queues[priority_idx].remove(i) {
                        let _ = entry.responder.send(Err(crate::Error::ServiceUnavailable(
                            "Queue timeout exceeded".to_string(),
                        )));
                    }
                } else {
                    i += 1;
                }
            }

            while let Some(entry) = queues[priority_idx].pop_front() {
                match route_fn(&entry.request) {
                    Ok(endpoint) => {
                        let _ = entry.responder.send(Ok(endpoint));
                        return;
                    }
                    Err(_) => {
                        // No capacity yet, put it back at the front
                        queues[priority_idx].push_front(entry);
                        break;
                    }
                }
            }
        }
    }

    /// Age (promote) requests that have been waiting too long.
    /// Moves entries to a higher-priority queue every `aging_interval`.
    pub async fn age_requests(&self) {
        let mut queues = self.queues.lock().await;
        let now = Instant::now();

        // Process from lowest priority to highest (skip Critical, can't promote further)
        for priority_idx in (1..Priority::COUNT).rev() {
            let mut i = 0;
            while i < queues[priority_idx].len() {
                if now.duration_since(queues[priority_idx][i].last_aged_at) >= self.aging_interval {
                    let mut entry = queues[priority_idx].remove(i).unwrap();
                    let new_priority = entry.effective_priority.promoted();
                    entry.effective_priority = new_priority;
                    entry.last_aged_at = now;
                    queues[new_priority as usize].push_back(entry);
                    // Don't increment i since we removed the element
                } else {
                    i += 1;
                }
            }
        }
    }

    /// Spawn a background task that periodically ages queued requests.
    pub fn spawn_aging_task(self: &Arc<Self>, shutdown: CancellationToken) {
        let manager = Arc::clone(self);
        let interval = manager.aging_interval;
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        manager.age_requests().await;
                    }
                    _ = shutdown.cancelled() => break,
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::backends::domain::EndpointId;
    use crate::shared::models::ChatRequest;

    fn make_endpoint(backend: &str) -> EndpointSnapshot {
        EndpointSnapshot {
            id: EndpointId {
                backend: backend.to_string(),
                index: 0,
            },
            url: format!("http://{backend}"),
            models: vec!["model-a".to_string()],
            currently_loaded_models: vec![],
            model_loading_aware: false,
            tags: vec![],
            weight: 1,
            capacity: 10,
            healthy: true,
            active_requests: 0,
            consecutive_failures: 0,
            consecutive_successes: 0,
        }
    }

    fn make_config() -> RoutingConfig {
        use crate::shared::config::types::{QueueConfig, StrategyConfig};
        use std::collections::HashMap;

        let mut priority_queues = HashMap::new();
        priority_queues.insert(
            "realtime".to_string(),
            QueueConfig {
                priority: 1,
                max_queue_size: 10,
                timeout: "5s".to_string(),
            },
        );

        RoutingConfig {
            strategy: StrategyConfig {
                name: "round_robin".to_string(),
                prefer_loaded_models: true,
                consider_queue_depth: true,
                fallback_strategy: "round_robin".to_string(),
                hysteresis_threshold: 0.10,
                health_weighted: false,
                admission_control: true,
            },
            priority_queues,
            default_queue: "realtime".to_string(),
        }
    }

    fn make_request() -> LlmRequest {
        LlmRequest::Chat(ChatRequest::simple("model-a", "hello"))
    }

    #[test]
    fn priority_ordering() {
        assert!(Priority::Critical < Priority::Realtime);
        assert!(Priority::Realtime < Priority::Interactive);
        assert!(Priority::Interactive < Priority::Batch);
        assert!(Priority::Batch < Priority::Background);
    }

    #[test]
    fn priority_from_name() {
        assert_eq!(Priority::from_name("critical"), Priority::Critical);
        assert_eq!(Priority::from_name("realtime"), Priority::Realtime);
        assert_eq!(Priority::from_name("interactive"), Priority::Interactive);
        assert_eq!(Priority::from_name("batch"), Priority::Batch);
        assert_eq!(Priority::from_name("background"), Priority::Background);
        assert_eq!(Priority::from_name("unknown"), Priority::Interactive);
    }

    #[test]
    fn priority_promotion() {
        assert_eq!(Priority::Background.promoted(), Priority::Batch);
        assert_eq!(Priority::Batch.promoted(), Priority::Interactive);
        assert_eq!(Priority::Interactive.promoted(), Priority::Realtime);
        assert_eq!(Priority::Realtime.promoted(), Priority::Critical);
        assert_eq!(Priority::Critical.promoted(), Priority::Critical);
    }

    #[tokio::test]
    async fn enqueue_and_dispatch() {
        let config = make_config();
        let manager = PriorityQueueManager::from_config(&config);
        let request = make_request();

        let rx = manager
            .enqueue(request, Priority::Interactive)
            .await
            .unwrap();

        assert_eq!(manager.total_queued().await, 1);

        let endpoint = make_endpoint("backend-a");
        let route_fn = |_req: &LlmRequest| -> Result<EndpointSnapshot> { Ok(endpoint.clone()) };

        manager.try_dispatch(&route_fn).await;
        assert_eq!(manager.total_queued().await, 0);

        let result = rx.await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.unwrap().id.backend, "backend-a");
    }

    #[tokio::test]
    async fn queue_full_rejection() {
        let config = make_config();
        let manager = PriorityQueueManager::from_config(&config);

        // Fill the queue to max (10)
        for _ in 0..10 {
            manager
                .enqueue(make_request(), Priority::Interactive)
                .await
                .unwrap();
        }

        // 11th should fail
        let result = manager.enqueue(make_request(), Priority::Interactive).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn dispatch_respects_priority_order() {
        let config = make_config();
        let manager = PriorityQueueManager::from_config(&config);

        // Enqueue background first, then critical
        let _rx_bg = manager
            .enqueue(make_request(), Priority::Background)
            .await
            .unwrap();
        let rx_crit = manager
            .enqueue(make_request(), Priority::Critical)
            .await
            .unwrap();

        assert_eq!(manager.total_queued().await, 2);

        let endpoint = make_endpoint("backend-a");
        let route_fn = |_req: &LlmRequest| -> Result<EndpointSnapshot> { Ok(endpoint.clone()) };

        // First dispatch should pick the critical request
        manager.try_dispatch(&route_fn).await;
        assert_eq!(manager.total_queued().await, 1);

        let result = rx_crit.await.unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn dispatch_when_no_capacity() {
        let config = make_config();
        let manager = PriorityQueueManager::from_config(&config);

        let _rx = manager
            .enqueue(make_request(), Priority::Interactive)
            .await
            .unwrap();

        // Route function always fails (no capacity)
        let route_fn = |_req: &LlmRequest| -> Result<EndpointSnapshot> {
            Err(crate::Error::ServiceUnavailable("at capacity".to_string()))
        };

        manager.try_dispatch(&route_fn).await;
        // Request should still be in queue
        assert_eq!(manager.total_queued().await, 1);
    }

    #[tokio::test]
    async fn aging_promotes_requests() {
        // Create a manager with a very short aging interval for testing
        let manager = PriorityQueueManager {
            queues: Mutex::new(std::array::from_fn(|_| VecDeque::new())),
            max_queue_size: 100,
            queue_timeout: Duration::from_secs(60),
            aging_interval: Duration::from_millis(10),
        };

        let _rx = manager
            .enqueue(make_request(), Priority::Background)
            .await
            .unwrap();

        // Wait for the aging interval to pass
        tokio::time::sleep(Duration::from_millis(15)).await;

        manager.age_requests().await;

        // Check that the request was promoted from Background(4) to Batch(3)
        let queues = manager.queues.lock().await;
        assert_eq!(queues[Priority::Background as usize].len(), 0);
        assert_eq!(queues[Priority::Batch as usize].len(), 1);
    }
}
