pub use crate::types::{HttpRequest, HttpResponse, LlmRequest};

pub trait AdapterPlugin: Send + Sync {
    fn name(&self) -> &'static str;
    fn build_request(&self, endpoint_url: &str, request: &LlmRequest) -> Option<HttpRequest>;
    fn parse_response(&self, response: &HttpResponse) -> Option<crate::types::ChatResponse>;
}

#[macro_export]
macro_rules! register_adapter_plugin {
    ($plugin:expr) => {
        #[extism_pdk::plugin_fn]
        pub fn build_request(
            extism_pdk::Json(input): extism_pdk::Json<(String, $crate::types::LlmRequest)>,
        ) -> extism_pdk::FnResult<extism_pdk::Json<Option<$crate::types::HttpRequest>>> {
            let plugin = $plugin;
            let (endpoint_url, request) = input;
            Ok(extism_pdk::Json(plugin.build_request(&endpoint_url, &request)))
        }

        #[extism_pdk::plugin_fn]
        pub fn parse_response(
            extism_pdk::Json(response): extism_pdk::Json<$crate::types::HttpResponse>,
        ) -> extism_pdk::FnResult<extism_pdk::Json<Option<$crate::types::ChatResponse>>> {
            let plugin = $plugin;
            Ok(extism_pdk::Json(plugin.parse_response(&response)))
        }
    };
}
