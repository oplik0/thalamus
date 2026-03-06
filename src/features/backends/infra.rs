use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::{Stream, StreamExt};

use crate::Error;
use crate::Result;
use crate::features::backends::adapter::adapter_for_backend;
use crate::features::backends::domain::{
    AdapterMap, BackendClient, BackendRegistry, EndpointId, EndpointSnapshot, EndpointState,
};
use crate::shared::config::types::{AuthConfig, BackendConfig, Config, RetryConfig};
use crate::shared::models::{ChatResponse, EmbeddingRequest, LlmRequest, StreamEvent};
use crate::shared::utils::parse_duration_or_default;

#[derive(Debug)]
pub struct InMemoryBackendRegistry {
    endpoints: HashMap<EndpointId, Arc<EndpointState>>,
}

impl InMemoryBackendRegistry {
    #[must_use]
    pub fn from_config(backends: &HashMap<String, BackendConfig>) -> Self {
        let mut endpoints = HashMap::new();

        for (backend_name, backend) in backends {
            for (index, endpoint) in backend.endpoints.iter().enumerate() {
                let id = EndpointId {
                    backend: backend_name.clone(),
                    index,
                };

                let timeout = parse_duration_or_default(&backend.timeout, Duration::from_secs(30));

                let state = EndpointState {
                    id: id.clone(),
                    config: endpoint.clone(),
                    backend_auth: backend.auth.clone(),
                    backend_timeout: timeout,
                    retry_config: backend.retry_config.clone(),
                    healthy: std::sync::atomic::AtomicBool::new(true),
                    consecutive_failures: std::sync::atomic::AtomicU32::new(0),
                    consecutive_successes: std::sync::atomic::AtomicU32::new(0),
                    active_requests: std::sync::atomic::AtomicU32::new(0),
                };

                endpoints.insert(id, Arc::new(state));
            }
        }

        Self { endpoints }
    }

    fn snapshot(state: &EndpointState) -> EndpointSnapshot {
        EndpointSnapshot {
            id: state.id.clone(),
            url: state.config.url.clone(),
            models: state.config.models.clone(),
            currently_loaded_models: state.config.currently_loaded_models.clone(),
            model_loading_aware: state.config.model_loading_aware,
            tags: state.config.tags.clone(),
            weight: state.config.weight,
            capacity: state.config.capacity,
            healthy: state.healthy.load(Ordering::Acquire),
            active_requests: state.active_requests.load(Ordering::Relaxed),
        }
    }

    pub fn health_transition(
        &self,
        id: &EndpointId,
        succeeded: bool,
        unhealthy_threshold: u32,
        healthy_threshold: u32,
    ) {
        if let Some(state) = self.endpoints.get(id) {
            if succeeded {
                state.consecutive_failures.store(0, Ordering::Relaxed);
                let successes = state.consecutive_successes.fetch_add(1, Ordering::Relaxed) + 1;
                if successes >= healthy_threshold {
                    state.healthy.store(true, Ordering::Release);
                }
            } else {
                state.consecutive_successes.store(0, Ordering::Relaxed);
                let failures = state.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
                if failures >= unhealthy_threshold {
                    state.healthy.store(false, Ordering::Release);
                }
            }
        }
    }
}

impl BackendRegistry for InMemoryBackendRegistry {
    fn endpoints_for_model(&self, model: &str) -> Vec<EndpointSnapshot> {
        self.endpoints
            .values()
            .map(|state| Self::snapshot(state))
            .filter(|snapshot| snapshot.healthy && snapshot.supports_model(model))
            .collect()
    }

    fn healthy_endpoints(&self) -> Vec<EndpointSnapshot> {
        self.endpoints
            .values()
            .map(|state| Self::snapshot(state))
            .filter(|snapshot| snapshot.healthy)
            .collect()
    }

    fn auth_for(&self, id: &EndpointId) -> AuthConfig {
        self.endpoints
            .get(id)
            .map_or(AuthConfig::None, |state| state.backend_auth.clone())
    }

    fn timeout_for(&self, id: &EndpointId) -> Duration {
        self.endpoints
            .get(id)
            .map_or(Duration::from_secs(30), |state| state.backend_timeout)
    }

    fn retry_for(&self, id: &EndpointId) -> Option<RetryConfig> {
        self.endpoints
            .get(id)
            .and_then(|state| state.retry_config.clone())
    }

    fn acquire(&self, id: &EndpointId) {
        if let Some(state) = self.endpoints.get(id) {
            state.active_requests.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn release(&self, id: &EndpointId) {
        if let Some(state) = self.endpoints.get(id) {
            state
                .active_requests
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                    if v > 0 { Some(v - 1) } else { None }
                })
                .ok();
        }
    }

    fn mark_health(&self, id: &EndpointId, healthy: bool) {
        if let Some(state) = self.endpoints.get(id) {
            state.healthy.store(healthy, Ordering::Release);
            if healthy {
                state.consecutive_failures.store(0, Ordering::Relaxed);
            } else {
                state.consecutive_successes.store(0, Ordering::Relaxed);
            }
        }
    }
}

pub struct AdaptingBackendClient {
    client: reqwest::Client,
    registry: Arc<dyn BackendRegistry>,
    adapters: AdapterMap,
}

impl std::fmt::Debug for AdaptingBackendClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdaptingBackendClient")
            .finish_non_exhaustive()
    }
}

impl AdaptingBackendClient {
    #[must_use]
    pub fn new(
        client: reqwest::Client,
        registry: Arc<dyn BackendRegistry>,
        adapters: AdapterMap,
    ) -> Self {
        Self {
            client,
            registry,
            adapters,
        }
    }

    #[must_use]
    pub fn adapters_from_config(config: &Config) -> AdapterMap {
        let mut adapters = HashMap::new();
        for (name, backend) in &config.backends {
            adapters.insert(name.clone(), adapter_for_backend(name, backend));
        }
        adapters
    }

    fn adapter_for(
        &self,
        endpoint: &EndpointSnapshot,
    ) -> Result<Arc<dyn crate::features::backends::domain::BackendAdapter>> {
        self.adapters
            .get(&endpoint.id.backend)
            .cloned()
            .ok_or_else(|| {
                Error::Backend(format!(
                    "No adapter configured for backend '{}'",
                    endpoint.id.backend
                ))
            })
    }
}

#[async_trait]
impl BackendClient for AdaptingBackendClient {
    async fn send(
        &self,
        endpoint: &EndpointSnapshot,
        request: &LlmRequest,
    ) -> Result<ChatResponse> {
        let adapter = self.adapter_for(endpoint)?;
        let auth = self.registry.auth_for(&endpoint.id);
        let timeout = self.registry.timeout_for(&endpoint.id);
        let retry = self.registry.retry_for(&endpoint.id);

        send_with_retry(
            &self.client,
            &*adapter,
            endpoint,
            request,
            &auth,
            timeout,
            retry,
        )
        .await
    }

    async fn send_stream(
        &self,
        endpoint: &EndpointSnapshot,
        request: &LlmRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let adapter = self.adapter_for(endpoint)?;
        let auth = self.registry.auth_for(&endpoint.id);
        let timeout = self.registry.timeout_for(&endpoint.id);

        let req = adapter.build_request(&self.client, &endpoint.url, request, &auth, timeout)?;
        let response = self.client.execute(req).await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.bytes().await.unwrap_or_default();
            return Err(Error::Backend(format!(
                "Backend stream error {}: {}",
                status.as_u16(),
                String::from_utf8_lossy(&body)
            )));
        }

        let stream = response
            .bytes_stream()
            .map(|item| item.map_err(Error::from));
        Ok(adapter.parse_stream(Box::pin(stream)))
    }

    async fn send_embedding(
        &self,
        endpoint: &EndpointSnapshot,
        request: &EmbeddingRequest,
    ) -> Result<serde_json::Value> {
        let adapter = self.adapter_for(endpoint)?;
        let auth = self.registry.auth_for(&endpoint.id);
        let timeout = self.registry.timeout_for(&endpoint.id);

        let req = adapter.build_embedding_request(
            &self.client,
            &endpoint.url,
            request,
            &auth,
            timeout,
        )?;
        let response = self.client.execute(req).await?;
        let status = response.status();
        let body = response.bytes().await?;
        adapter.parse_embedding_response(status, &body)
    }
}

async fn send_with_retry(
    client: &reqwest::Client,
    adapter: &dyn crate::features::backends::domain::BackendAdapter,
    endpoint: &EndpointSnapshot,
    request: &LlmRequest,
    auth: &AuthConfig,
    timeout: Duration,
    retry_config: Option<RetryConfig>,
) -> Result<ChatResponse> {
    let Some(retry) = retry_config else {
        let req = adapter.build_request(client, &endpoint.url, request, auth, timeout)?;
        let response = client.execute(req).await?;
        let status = response.status();
        let body = response.bytes().await?;
        return adapter.parse_response(status, &body);
    };

    let mut attempt = 0;
    let mut delay = parse_duration_or_default(&retry.initial_delay, Duration::from_millis(250));
    let max_delay = parse_duration_or_default(&retry.max_delay, Duration::from_secs(5));

    loop {
        let req = adapter.build_request(client, &endpoint.url, request, auth, timeout)?;
        let exec_result = client.execute(req).await;

        match exec_result {
            Ok(response) => {
                let status = response.status();
                let body = response.bytes().await?;

                if status.is_server_error() && attempt < retry.max_retries {
                    tokio::time::sleep(delay).await;
                    delay = backoff_delay(delay, max_delay, retry.exponential_backoff);
                    attempt += 1;
                    continue;
                }

                return adapter.parse_response(status, &body);
            }
            Err(error) => {
                let retryable = error.is_timeout() && retry.retry_on_timeout;
                if retryable && attempt < retry.max_retries {
                    tokio::time::sleep(delay).await;
                    delay = backoff_delay(delay, max_delay, retry.exponential_backoff);
                    attempt += 1;
                    continue;
                }
                return Err(Error::from(error));
            }
        }
    }
}

fn backoff_delay(current: Duration, max: Duration, exponential: bool) -> Duration {
    if !exponential {
        return current;
    }

    let doubled = current.as_millis().saturating_mul(2);
    let jitter = (rand::random::<u16>() as u128) % 25;
    let with_jitter = doubled.saturating_add(jitter);
    let capped = std::cmp::min(with_jitter, max.as_millis());
    Duration::from_millis(capped as u64)
}

#[derive(Debug)]
pub struct RoundRobinCursor {
    cursor: AtomicUsize,
}

impl Default for RoundRobinCursor {
    fn default() -> Self {
        Self {
            cursor: AtomicUsize::new(0),
        }
    }
}

impl RoundRobinCursor {
    #[must_use]
    pub fn next_index(&self, len: usize) -> usize {
        self.cursor.fetch_add(1, Ordering::Relaxed) % len
    }
}
