//! Request priority resolution
//!
//! Priority can be overridden by the caller via the `X-Thalamus-Priority`
//! header, otherwise it falls back to the authenticated principal's default,
//! then to the team's default, and finally to the configured default queue
//! priority.

use axum::http::HeaderMap;

use crate::features::routing::queue::Priority;
use crate::middleware::auth::Auth;
use crate::shared::config::types::RoutingConfig;

const PRIORITY_HEADER: &str = "X-Thalamus-Priority";

/// Resolve the priority for an LLM request.
///
/// Resolution order:
/// 1. `X-Thalamus-Priority` request header, if present and valid.
/// 2. The authenticated principal's configured default priority.
/// 3. The configured `routing.default_queue` mapped to a `Priority` level.
///
/// `auth` is `None` for unauthenticated requests (the proxy currently allows
/// optional auth).
#[must_use]
pub fn resolve_priority(
    headers: &HeaderMap,
    auth: Option<&Auth>,
    routing_config: &RoutingConfig,
) -> Priority {
    if let Some(value) = headers.get(PRIORITY_HEADER).and_then(|h| h.to_str().ok()) {
        let parsed = Priority::from_name(value);
        if value.to_ascii_lowercase() == parsed.as_str() {
            return parsed;
        }
    }

    if let Some(auth) = auth
        && let Some(priority) = auth.priority
    {
        return priority;
    }

    Priority::from_name(&routing_config.default_queue)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use uuid::Uuid;

    fn config(default_queue: &str) -> RoutingConfig {
        use crate::shared::config::types::{QueueConfig, StrategyConfig};
        use std::collections::HashMap;

        let mut priority_queues = HashMap::new();
        priority_queues.insert(
            default_queue.to_string(),
            QueueConfig {
                priority: 0,
                max_queue_size: 10,
                timeout: "30s".to_string(),
            },
        );

        RoutingConfig {
            strategy: StrategyConfig {
                name: "round_robin".to_string(),
                prefer_loaded_models: true,
                consider_queue_depth: true,
                fallback_strategy: String::new(),
                hysteresis_threshold: 0.10,
                health_weighted: false,
                admission_control: true,
            },
            priority_queues,
            default_queue: default_queue.to_string(),
        }
    }

    fn auth_with_priority(priority: Priority) -> Auth {
        Auth {
            user_id: Uuid::new_v4(),
            team_id: Uuid::new_v4(),
            project_id: None,
            scopes: None,
            roles: None,
            key_id: None,
            token_id: None,
            priority: Some(priority),
        }
    }

    #[test]
    fn header_overrides_default() {
        let mut headers = HeaderMap::new();
        headers.insert(PRIORITY_HEADER, HeaderValue::from_static("batch"));

        assert_eq!(
            resolve_priority(&headers, None, &config("realtime")),
            Priority::Batch
        );
    }

    #[test]
    fn header_overrides_auth_default() {
        let mut headers = HeaderMap::new();
        headers.insert(PRIORITY_HEADER, HeaderValue::from_static("critical"));
        let auth = auth_with_priority(Priority::Background);

        assert_eq!(
            resolve_priority(&headers, Some(&auth), &config("realtime")),
            Priority::Critical
        );
    }

    #[test]
    fn auth_default_used_without_header() {
        let auth = auth_with_priority(Priority::Batch);

        assert_eq!(
            resolve_priority(&HeaderMap::new(), Some(&auth), &config("realtime")),
            Priority::Batch
        );
    }

    #[test]
    fn default_queue_is_used_without_auth_or_header() {
        assert_eq!(
            resolve_priority(&HeaderMap::new(), None, &config("batch")),
            Priority::Batch
        );
    }

    #[test]
    fn invalid_header_falls_back_to_default() {
        let mut headers = HeaderMap::new();
        headers.insert(PRIORITY_HEADER, HeaderValue::from_static("urgent"));

        assert_eq!(
            resolve_priority(&headers, None, &config("realtime")),
            Priority::Realtime
        );
    }
}
