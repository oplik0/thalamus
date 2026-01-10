use crate::features::backends::domain::EndpointSnapshot;
use crate::shared::models::LlmRequest;

#[derive(Debug, Clone, PartialEq)]
pub struct RoutingContext<'a> {
    pub request: &'a LlmRequest,
    pub candidates: &'a [EndpointSnapshot],
}

pub trait RoutingStrategy: Send + Sync {
    fn select(&self, ctx: &RoutingContext<'_>) -> Option<EndpointSnapshot>;
    fn name(&self) -> &str;
}
