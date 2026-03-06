use std::sync::Arc;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::features::backends::infra::InMemoryBackendRegistry;
use crate::shared::config::types::BackendConfig;
use crate::shared::utils::parse_duration_or_default;

pub fn spawn_health_checks(
    http_client: reqwest::Client,
    registry: Arc<InMemoryBackendRegistry>,
    backends: &std::collections::HashMap<String, BackendConfig>,
    shutdown: CancellationToken,
) -> Vec<JoinHandle<()>> {
    let mut handles = Vec::new();

    for (backend_name, backend) in backends {
        let Some(health_config) = &backend.health_check else {
            continue;
        };

        if !health_config.enabled {
            continue;
        }

        let interval =
            parse_duration_or_default(&health_config.interval, std::time::Duration::from_secs(10));
        let timeout =
            parse_duration_or_default(&health_config.timeout, std::time::Duration::from_secs(3));
        let endpoint = health_config
            .endpoint
            .clone()
            .unwrap_or_else(|| "/health".to_string());

        for (index, endpoint_config) in backend.endpoints.iter().enumerate() {
            let id = crate::features::backends::domain::EndpointId {
                backend: backend_name.clone(),
                index,
            };

            let url = format!("{}{}", endpoint_config.url.trim_end_matches('/'), endpoint);
            let client = http_client.clone();
            let registry = Arc::clone(&registry);
            let unhealthy_threshold = health_config.unhealthy_threshold;
            let healthy_threshold = health_config.healthy_threshold;
            let token = shutdown.clone();

            handles.push(tokio::spawn(async move {
                let mut ticker = tokio::time::interval(interval);
                loop {
                    tokio::select! {
                        _ = token.cancelled() => break,
                        _ = ticker.tick() => {
                            let outcome = client.get(&url).timeout(timeout).send().await;
                            let healthy = outcome
                                .as_ref()
                                .map(|res| res.status().is_success())
                                .unwrap_or(false);

                            registry.health_transition(&id, healthy, unhealthy_threshold, healthy_threshold);
                        }
                    }
                }
            }));
        }
    }

    handles
}
