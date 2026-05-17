use extism_pdk::*;
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
struct RoutingContext {
    request: LlmRequest,
    candidates: Vec<EndpointSnapshot>,
}

#[derive(Serialize)]
struct LlmRequest {
    model: String,
}

#[derive(Serialize, Deserialize)]
struct EndpointSnapshot {
    id: EndpointId,
    url: String,
    models: Vec<String>,
    weight: u32,
    capacity: u32,
    healthy: bool,
    active_requests: u32,
}

#[derive(Serialize, Deserialize)]
struct EndpointId {
    backend: String,
    index: usize,
}

#[derive(Serialize)]
struct RoutingResult {
    endpoint_id: Option<EndpointId>,
}

#[plugin_fn]
pub fn select(Json(ctx): Json<RoutingContext>) -> FnResult<Json<RoutingResult>> {
    // Simple logic: select the first healthy candidate
    let selected = ctx.candidates.first().cloned();
    Ok(Json(RoutingResult {
        endpoint_id: selected.map(|c| c.id),
    }))
}
