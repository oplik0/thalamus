pub use crate::types::{HttpRequest, HttpResponse, LlmRequest};

pub trait AdapterPlugin: Send + Sync {
    fn name(&self) -> &'static str;
    fn build_request(&self, endpoint_url: &str, request: &LlmRequest) -> Option<HttpRequest>;
    fn parse_response(&self, response: &HttpResponse) -> Option<crate::types::ChatResponse>;
}

#[macro_export]
macro_rules! register_adapter_plugin {
    ($plugin:expr) => {
        #[$crate::extism_pdk::plugin_fn]
        pub fn build_request(
            $crate::extism_pdk::Json(input): $crate::extism_pdk::Json<(
                String,
                $crate::types::LlmRequest,
            )>,
        ) -> $crate::extism_pdk::FnResult<
            $crate::extism_pdk::Json<Option<$crate::types::HttpRequest>>,
        > {
            let plugin = $plugin;
            let (endpoint_url, request) = input;
            Ok($crate::extism_pdk::Json(
                plugin.build_request(&endpoint_url, &request),
            ))
        }

        #[$crate::extism_pdk::plugin_fn]
        pub fn parse_response(
            $crate::extism_pdk::Json(response): $crate::extism_pdk::Json<
                $crate::types::HttpResponse,
            >,
        ) -> $crate::extism_pdk::FnResult<
            $crate::extism_pdk::Json<Option<$crate::types::ChatResponse>>,
        > {
            let plugin = $plugin;
            Ok($crate::extism_pdk::Json(plugin.parse_response(&response)))
        }
    };
}
