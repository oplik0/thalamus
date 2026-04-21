//! Backend configuration builder for tests
//!
//! Provides a fluent API for constructing BackendConfig instances
//! for use with mock backends in E2E tests.

use std::collections::HashMap;

use thalamus::shared::config::types::{
    AuthConfig, BackendConfig, EndpointConfig, HealthCheckConfig, RetryConfig,
};

/// Builder for constructing BackendConfig instances
///
/// # Example
/// ```rust
/// let config = BackendConfigBuilder::new("my-backend")
///     .with_endpoint("http://localhost:8080", 10, vec!["gpt-4"])
///     .with_bearer_auth("test-token")
///     .with_health_check(true, "1s", "3s")
///     .with_timeout("30s")
///     .build();
/// ```
#[derive(Debug)]
pub struct BackendConfigBuilder {
    name: String,
    endpoints: Vec<EndpointConfig>,
    auth: AuthConfig,
    health_check: Option<HealthCheckConfig>,
    retry_config: Option<RetryConfig>,
    timeout: String,
}

impl BackendConfigBuilder {
    /// Create a new builder with the given backend name
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            endpoints: Vec::new(),
            auth: AuthConfig::None,
            health_check: None,
            retry_config: None,
            timeout: "30s".to_string(),
        }
    }

    /// Add an endpoint to this backend
    ///
    /// # Arguments
    /// * `url` - The endpoint URL
    /// * `capacity` - Maximum concurrent requests
    /// * `models` - List of supported models
    pub fn with_endpoint(
        mut self,
        url: impl Into<String>,
        capacity: u32,
        models: Vec<impl Into<String>>,
    ) -> Self {
        let endpoint = EndpointConfig {
            url: url.into(),
            capacity,
            models: models.into_iter().map(|m| m.into()).collect(),
            currently_loaded_models: Vec::new(),
            model_loading_aware: false,
            tags: Vec::new(),
            weight: 1,
        };
        self.endpoints.push(endpoint);
        self
    }

    /// Add an endpoint with custom weight (for weighted routing)
    pub fn with_weighted_endpoint(
        mut self,
        url: impl Into<String>,
        capacity: u32,
        models: Vec<impl Into<String>>,
        weight: u32,
    ) -> Self {
        let endpoint = EndpointConfig {
            url: url.into(),
            capacity,
            models: models.into_iter().map(|m| m.into()).collect(),
            currently_loaded_models: Vec::new(),
            model_loading_aware: false,
            tags: Vec::new(),
            weight,
        };
        self.endpoints.push(endpoint);
        self
    }

    /// Add an endpoint with model loading awareness
    pub fn with_model_aware_endpoint(
        mut self,
        url: impl Into<String>,
        capacity: u32,
        models: Vec<impl Into<String>>,
        currently_loaded: Vec<impl Into<String>>,
    ) -> Self {
        let endpoint = EndpointConfig {
            url: url.into(),
            capacity,
            models: models.into_iter().map(|m| m.into()).collect(),
            currently_loaded_models: currently_loaded.into_iter().map(|m| m.into()).collect(),
            model_loading_aware: true,
            tags: Vec::new(),
            weight: 1,
        };
        self.endpoints.push(endpoint);
        self
    }

    /// Set Bearer token authentication
    pub fn with_bearer_auth(mut self, token: impl Into<String>) -> Self {
        self.auth = AuthConfig::BearerToken {
            token: token.into(),
        };
        self
    }

    /// Set API key authentication
    pub fn with_api_key_auth(
        mut self,
        header: impl Into<String>,
        token: impl Into<String>,
    ) -> Self {
        self.auth = AuthConfig::ApiKey {
            header: header.into(),
            token: token.into(),
        };
        self
    }

    /// Set Basic authentication
    pub fn with_basic_auth(
        mut self,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        self.auth = AuthConfig::Basic {
            username: username.into(),
            password: password.into(),
        };
        self
    }

    /// Set custom authentication
    pub fn with_custom_auth(mut self, token: impl Into<String>) -> Self {
        self.auth = AuthConfig::Custom {
            token: token.into(),
        };
        self
    }

    /// Enable/disable health checks
    ///
    /// # Arguments
    /// * `enabled` - Whether health checks are enabled
    /// * `interval` - Check interval (e.g., "1s", "5s")
    /// * `timeout` - Request timeout (e.g., "3s", "10s")
    pub fn with_health_check(
        mut self,
        enabled: bool,
        interval: impl Into<String>,
        timeout: impl Into<String>,
    ) -> Self {
        self.health_check = Some(HealthCheckConfig {
            enabled,
            interval: interval.into(),
            timeout: timeout.into(),
            endpoint: Some("/health".to_string()),
            unhealthy_threshold: 3,
            healthy_threshold: 2,
        });
        self
    }

    /// Configure retry behavior
    pub fn with_retry(
        mut self,
        max_retries: u32,
        initial_delay: impl Into<String>,
        max_delay: impl Into<String>,
    ) -> Self {
        self.retry_config = Some(RetryConfig {
            max_retries,
            initial_delay: initial_delay.into(),
            max_delay: max_delay.into(),
            exponential_backoff: true,
            retry_on_timeout: true,
        });
        self
    }

    /// Set request timeout
    pub fn with_timeout(mut self, timeout: impl Into<String>) -> Self {
        self.timeout = timeout.into();
        self
    }

    /// Build the BackendConfig
    pub fn build(self) -> BackendConfig {
        BackendConfig {
            endpoints: self.endpoints,
            auth: self.auth,
            health_check: self.health_check,
            retry_config: self.retry_config,
            timeout: self.timeout,
        }
    }

    /// Build and insert into a HashMap with the configured name
    pub fn build_and_insert(self, map: &mut HashMap<String, BackendConfig>) -> BackendConfig {
        let name = self.name.clone();
        let config = self.build();
        map.insert(name, config.clone());
        config
    }
}

/// Builder for constructing complete routing configurations
///
/// # Example
/// ```rust
/// let routing_config = RoutingConfigBuilder::new("round_robin")
///     .with_admission_control(true)
///     .with_loaded_model_preference(true)
///     .with_queue("realtime", 1, 100, "30s")
///     .build();
/// ```
#[derive(Debug)]
pub struct RoutingConfigBuilder {
    strategy_name: String,
    prefer_loaded_models: bool,
    consider_queue_depth: bool,
    fallback_strategy: String,
    hysteresis_threshold: f64,
    health_weighted: bool,
    admission_control: bool,
    priority_queues: HashMap<String, thalamus::shared::config::types::QueueConfig>,
    default_queue: String,
}

impl RoutingConfigBuilder {
    /// Create a new builder with the specified strategy
    ///
    /// Available strategies: round_robin, random, weighted, least_busy,
    /// least_connections, model_aware, health_weighted
    pub fn new(strategy: impl Into<String>) -> Self {
        Self {
            strategy_name: strategy.into(),
            prefer_loaded_models: true,
            consider_queue_depth: true,
            fallback_strategy: "round_robin".to_string(),
            hysteresis_threshold: 0.10,
            health_weighted: false,
            admission_control: true,
            priority_queues: HashMap::new(),
            default_queue: "realtime".to_string(),
        }
    }

    /// Set whether to prefer backends with models already loaded
    pub fn with_loaded_model_preference(mut self, prefer: bool) -> Self {
        self.prefer_loaded_models = prefer;
        self
    }

    /// Set whether to consider queue depth in routing decisions
    pub fn with_queue_depth_consideration(mut self, consider: bool) -> Self {
        self.consider_queue_depth = consider;
        self
    }

    /// Set the fallback strategy when primary fails
    pub fn with_fallback_strategy(mut self, fallback: impl Into<String>) -> Self {
        self.fallback_strategy = fallback.into();
        self
    }

    /// Set the hysteresis threshold for load-based routing
    pub fn with_hysteresis_threshold(mut self, threshold: f64) -> Self {
        self.hysteresis_threshold = threshold;
        self
    }

    /// Enable/disable health-weighted routing
    pub fn with_health_weighted(mut self, enabled: bool) -> Self {
        self.health_weighted = enabled;
        self
    }

    /// Enable/disable admission control
    pub fn with_admission_control(mut self, enabled: bool) -> Self {
        self.admission_control = enabled;
        self
    }

    /// Add a priority queue
    ///
    /// # Arguments
    /// * `name` - Queue identifier
    /// * `priority` - Priority level (lower = higher priority)
    /// * `max_size` - Maximum queue size
    /// * `timeout` - Queue timeout (e.g., "30s")
    pub fn with_queue(
        mut self,
        name: impl Into<String>,
        priority: u32,
        max_size: u32,
        timeout: impl Into<String>,
    ) -> Self {
        let queue = thalamus::shared::config::types::QueueConfig {
            priority,
            max_queue_size: max_size,
            timeout: timeout.into(),
        };
        self.priority_queues.insert(name.into(), queue);
        self
    }

    /// Set the default queue
    pub fn with_default_queue(mut self, queue: impl Into<String>) -> Self {
        self.default_queue = queue.into();
        self
    }

    /// Build the routing configuration
    pub fn build(self) -> thalamus::shared::config::types::RoutingConfig {
        use thalamus::shared::config::types::{QueueConfig, RoutingConfig, StrategyConfig};

        // Ensure default queue exists
        let mut queues = self.priority_queues;
        if !queues.contains_key(&self.default_queue) {
            queues.insert(
                self.default_queue.clone(),
                QueueConfig {
                    priority: 1,
                    max_queue_size: 100,
                    timeout: "30s".to_string(),
                },
            );
        }

        RoutingConfig {
            strategy: StrategyConfig {
                name: self.strategy_name,
                prefer_loaded_models: self.prefer_loaded_models,
                consider_queue_depth: self.consider_queue_depth,
                fallback_strategy: self.fallback_strategy,
                hysteresis_threshold: self.hysteresis_threshold,
                health_weighted: self.health_weighted,
                admission_control: self.admission_control,
            },
            priority_queues: queues,
            default_queue: self.default_queue,
        }
    }
}
