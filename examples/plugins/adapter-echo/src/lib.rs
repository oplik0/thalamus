use thalamus_plugin::adapter::{AdapterPlugin, HttpRequest, HttpResponse};
use thalamus_plugin::register_adapter_plugin;
use thalamus_plugin::types::{ChatResponse, LlmRequest};

struct EchoAdapter;

impl AdapterPlugin for EchoAdapter {
    fn name(&self) -> &'static str {
        "adapter-echo"
    }

    fn build_request(&self, endpoint_url: &str, _request: &LlmRequest) -> Option<HttpRequest> {
        Some(HttpRequest {
            method: "POST".to_string(),
            url: format!("{}/v1/chat/completions", endpoint_url.trim_end_matches('/')),
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: serde_json::json!({
                "model": "echo",
                "messages": [{"role": "user", "content": "hello"}]
            }),
        })
    }

    fn parse_response(&self, _response: &HttpResponse) -> Option<ChatResponse> {
        Some(ChatResponse {
            model: "echo".to_string(),
            status: Some("completed".to_string()),
        })
    }
}

register_adapter_plugin!(EchoAdapter);
