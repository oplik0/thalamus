use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use reqwest::StatusCode;

use crate::Result;
use crate::shared::config::types::{AuthConfig, EndpointConfig, RetryConfig};
use crate::shared::models::{ChatResponse, EmbeddingRequest, LlmRequest, StreamEvent};

#[derive(Debug, Clone, Eq)]
pub struct EndpointId {
    pub backend: String,
    pub index: usize,
}

impl PartialEq for EndpointId {
    fn eq(&self, other: &Self) -> bool {
        self.backend == other.backend && self.index == other.index
    }
}

impl Hash for EndpointId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.backend.hash(state);
        self.index.hash(state);
    }
}

#[derive(Debug)]
pub struct EndpointState {
    pub id: EndpointId,
    pub config: EndpointConfig,
    pub backend_auth: AuthConfig,
    pub backend_timeout: Duration,
    pub retry_config: Option<RetryConfig>,
    pub healthy: AtomicBool,
    pub consecutive_failures: AtomicU32,
    pub consecutive_successes: AtomicU32,
    pub active_requests: AtomicU32,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct EndpointSnapshot {
    pub id: EndpointId,
    pub url: String,
    pub models: Vec<String>,
    pub currently_loaded_models: Vec<String>,
    pub model_loading_aware: bool,
    pub tags: Vec<String>,
    pub weight: u32,
    pub capacity: u32,
    pub healthy: bool,
    pub active_requests: u32,
}

impl EndpointSnapshot {
    #[must_use]
    pub fn supports_model(&self, model: &str) -> bool {
        self.models.iter().any(|m| m == model)
    }

    #[must_use]
    pub fn has_loaded_model(&self, model: &str) -> bool {
        self.currently_loaded_models.iter().any(|m| m == model)
    }
}

pub trait BackendRegistry: Send + Sync {
    fn endpoints_for_model(&self, model: &str) -> Vec<EndpointSnapshot>;
    fn healthy_endpoints(&self) -> Vec<EndpointSnapshot>;
    fn auth_for(&self, id: &EndpointId) -> AuthConfig;
    fn timeout_for(&self, id: &EndpointId) -> Duration;
    fn retry_for(&self, id: &EndpointId) -> Option<RetryConfig>;
    fn acquire(&self, id: &EndpointId);
    fn release(&self, id: &EndpointId);
    fn mark_health(&self, id: &EndpointId, healthy: bool);
}

pub trait BackendAdapter: Send + Sync {
    fn name(&self) -> &str;

    fn build_request(
        &self,
        client: &reqwest::Client,
        endpoint_url: &str,
        request: &LlmRequest,
        auth: &AuthConfig,
        timeout: Duration,
    ) -> Result<reqwest::Request>;

    fn parse_response(&self, status: StatusCode, body: &[u8]) -> Result<ChatResponse>;

    fn parse_stream(
        &self,
        body_stream: Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>;

    fn build_embedding_request(
        &self,
        client: &reqwest::Client,
        endpoint_url: &str,
        request: &EmbeddingRequest,
        auth: &AuthConfig,
        timeout: Duration,
    ) -> Result<reqwest::Request>;

    fn parse_embedding_response(
        &self,
        status: StatusCode,
        body: &[u8],
    ) -> Result<serde_json::Value>;
}

#[async_trait]
pub trait BackendClient: Send + Sync {
    async fn send(&self, endpoint: &EndpointSnapshot, request: &LlmRequest)
    -> Result<ChatResponse>;

    async fn send_stream(
        &self,
        endpoint: &EndpointSnapshot,
        request: &LlmRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>>;

    async fn send_embedding(
        &self,
        endpoint: &EndpointSnapshot,
        request: &EmbeddingRequest,
    ) -> Result<serde_json::Value>;
}

pub type AdapterMap = HashMap<String, std::sync::Arc<dyn BackendAdapter>>;
