use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EndpointId {
    pub backend: String,
    pub index: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Endpoint {
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
    pub consecutive_failures: u32,
    pub consecutive_successes: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmRequest {
    pub model: String,
    // Minimal for now; can be extended later
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    pub model: String,
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: serde_json::Value,
}
