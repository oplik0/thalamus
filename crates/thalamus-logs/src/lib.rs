//! Bounded access to a Loki-compatible log service.
//!
//! The caller must obtain an [`AuthorizedDeployment`] from trusted authorization
//! policy, not from request parameters. Only streams explicitly carrying the
//! exact `team`, `namespace`, `service` and `deployment` labels are returned.
//! Deployments whose log source does not provide these labels must not use this provider.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const MAX_WINDOW_SECONDS: u64 = 3600;
const MAX_LINES: usize = 500;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Error)]
pub enum LogError {
    #[error("invalid log service configuration")]
    Configuration,
    #[error("deployment is not authorized")]
    Unauthorized,
    #[error("invalid log query bounds")]
    Bounds,
    #[error("log service request failed")]
    Backend,
    #[error("invalid or untrusted log service response")]
    Response,
    #[error("log service response exceeds the byte limit")]
    ResponseTooLarge,
}

/// A binding supplied by the authorization layer after checking team ownership.
/// Never construct this from a caller's claimed team and deployment alone.
#[derive(Debug, Clone)]
pub struct AuthorizedDeployment {
    team: String,
    namespace: String,
    service: String,
    deployment: String,
}

impl AuthorizedDeployment {
    pub fn new(
        team: impl Into<String>,
        namespace: impl Into<String>,
        service: impl Into<String>,
        deployment: impl Into<String>,
    ) -> Result<Self, LogError> {
        let team = team.into();
        let namespace = namespace.into();
        let service = service.into();
        let deployment = deployment.into();
        if team.is_empty() || namespace.is_empty() || service.is_empty() || deployment.is_empty() {
            return Err(LogError::Unauthorized);
        }
        Ok(Self {
            team,
            namespace,
            service,
            deployment,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct LogEntry {
    /// Unix timestamp in nanoseconds, as supplied by Loki.
    pub timestamp_ns: u64,
    pub line: String,
}

pub struct LokiClient {
    client: Client,
    endpoint: Url,
}

impl LokiClient {
    /// The endpoint is operator configuration, never a request-supplied URL.
    pub fn new(endpoint: &str) -> Result<Self, LogError> {
        let mut endpoint = Url::parse(endpoint).map_err(|_| LogError::Configuration)?;
        if endpoint.scheme() != "https"
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            return Err(LogError::Configuration);
        }
        let path = format!(
            "{}/loki/api/v1/query_range",
            endpoint.path().trim_end_matches('/')
        );
        endpoint.set_path(&path);
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|_| LogError::Configuration)?;
        Ok(Self { client, endpoint })
    }

    /// Query a bounded recent interval. The requested names must exactly match
    /// the trusted binding; no selector or query language is accepted.
    pub async fn recent(
        &self,
        binding: &AuthorizedDeployment,
        team: &str,
        service: &str,
        deployment: &str,
        seconds: u64,
        limit: usize,
    ) -> Result<Vec<LogEntry>, LogError> {
        if binding.team != team || binding.service != service || binding.deployment != deployment {
            return Err(LogError::Unauthorized);
        }
        if !(1..=MAX_WINDOW_SECONDS).contains(&seconds) || !(1..=MAX_LINES).contains(&limit) {
            return Err(LogError::Bounds);
        }
        let end = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| LogError::Bounds)?
            .as_nanos();
        let end = u64::try_from(end).map_err(|_| LogError::Bounds)?;
        let start = end
            .checked_sub(seconds * 1_000_000_000)
            .ok_or(LogError::Bounds)?;
        let selector = selector(binding);
        let mut response = self
            .client
            .get(self.endpoint.clone())
            .query(&[
                ("query", selector),
                ("start", start.to_string()),
                ("end", end.to_string()),
                ("limit", limit.to_string()),
                ("direction", "backward".to_owned()),
            ])
            .send()
            .await
            .map_err(|_| LogError::Backend)?;
        if !response.status().is_success() {
            return Err(LogError::Backend);
        }
        if response
            .content_length()
            .is_some_and(|n| n > MAX_RESPONSE_BYTES as u64)
        {
            return Err(LogError::ResponseTooLarge);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| LogError::Backend)? {
            if chunk.len() > MAX_RESPONSE_BYTES - bytes.len() {
                return Err(LogError::ResponseTooLarge);
            }
            bytes.extend_from_slice(&chunk);
        }
        parse_response(&bytes, binding, start, end, limit)
    }
}

fn selector(binding: &AuthorizedDeployment) -> String {
    // JSON string quoting is compatible with LogQL's double-quoted label values.
    format!(
        "{{team={},namespace={},service={},deployment={}}}",
        serde_json::to_string(&binding.team).expect("string serialization cannot fail"),
        serde_json::to_string(&binding.namespace).expect("string serialization cannot fail"),
        serde_json::to_string(&binding.service).expect("string serialization cannot fail"),
        serde_json::to_string(&binding.deployment).expect("string serialization cannot fail")
    )
}

#[derive(Deserialize)]
struct Envelope {
    status: String,
    data: Data,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Data {
    result_type: String,
    result: Vec<Stream>,
}
#[derive(Deserialize)]
struct Stream {
    stream: std::collections::HashMap<String, String>,
    values: Vec<(String, String)>,
}

fn parse_response(
    bytes: &[u8],
    binding: &AuthorizedDeployment,
    start: u64,
    end: u64,
    limit: usize,
) -> Result<Vec<LogEntry>, LogError> {
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(LogError::ResponseTooLarge);
    }
    let response: Envelope = serde_json::from_slice(bytes).map_err(|_| LogError::Response)?;
    if response.status != "success" || response.data.result_type != "streams" {
        return Err(LogError::Response);
    }
    let mut entries = Vec::new();
    for stream in response.data.result {
        if stream.stream.get("team") != Some(&binding.team)
            || stream.stream.get("namespace") != Some(&binding.namespace)
            || stream.stream.get("service") != Some(&binding.service)
            || stream.stream.get("deployment") != Some(&binding.deployment)
        {
            return Err(LogError::Response);
        }
        for (timestamp, line) in stream.values {
            if entries.len() == limit {
                return Err(LogError::Response);
            }
            let timestamp_ns = timestamp.parse::<u64>().map_err(|_| LogError::Response)?;
            if timestamp_ns < start || timestamp_ns > end {
                return Err(LogError::Response);
            }
            entries.push(LogEntry { timestamp_ns, line });
        }
    }
    entries.sort_unstable_by(|a, b| b.timestamp_ns.cmp(&a.timestamp_ns));
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn binding() -> AuthorizedDeployment {
        AuthorizedDeployment::new("alpha", "ns", "svc", "deploy-a").unwrap()
    }
    fn response(labels: serde_json::Value, values: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&json!({"status":"success","data":{"resultType":"streams","result":[{"stream":labels,"values":values}]}})).unwrap()
    }
    #[test]
    fn selector_escapes_injection() {
        let binding = AuthorizedDeployment::new("a\"} |= \"secret", "ns", "svc", "d\\x\n").unwrap();
        assert_eq!(
            selector(&binding),
            "{team=\"a\\\"} |= \\\"secret\",namespace=\"ns\",service=\"svc\",deployment=\"d\\\\x\\n\"}"
        );
    }
    #[test]
    fn parses_and_orders_timestamped_lines() {
        let body = response(
            json!({"team":"alpha","namespace":"ns","service":"svc","deployment":"deploy-a"}),
            json!([["12", "later"], ["11", "earlier"]]),
        );
        let logs = parse_response(&body, &binding(), 10, 20, 2).unwrap();
        assert_eq!(
            logs[0],
            LogEntry {
                timestamp_ns: 12,
                line: "later".into()
            }
        );
        assert_eq!(logs[1].timestamp_ns, 11);
    }
    #[test]
    fn rejects_unattributed_or_cross_tenant_streams() {
        for labels in [
            json!({"namespace":"ns","service":"svc","deployment":"deploy-a"}),
            json!({"team":"beta","namespace":"ns","service":"svc","deployment":"deploy-a"}),
            json!({"team":"alpha","namespace":"other","service":"svc","deployment":"deploy-a"}),
            json!({"team":"alpha","namespace":"ns","service":"other","deployment":"deploy-a"}),
            json!({"team":"alpha","namespace":"ns","service":"svc","deployment":"other"}),
        ] {
            assert!(matches!(
                parse_response(
                    &response(labels, json!([["11", "secret"]])),
                    &binding(),
                    10,
                    20,
                    2
                ),
                Err(LogError::Response)
            ));
        }
    }
    #[test]
    fn rejects_malformed_out_of_range_and_excess_lines() {
        let labels =
            json!({"team":"alpha","namespace":"ns","service":"svc","deployment":"deploy-a"});
        for values in [
            json!([["bad", "line"]]),
            json!([["21", "line"]]),
            json!([["11", "one"], ["12", "two"]]),
        ] {
            assert!(matches!(
                parse_response(&response(labels.clone(), values), &binding(), 10, 20, 1),
                Err(LogError::Response)
            ));
        }
        assert!(matches!(
            parse_response(b"not json", &binding(), 10, 20, 1),
            Err(LogError::Response)
        ));
        assert!(matches!(
            parse_response(&vec![0; MAX_RESPONSE_BYTES + 1], &binding(), 10, 20, 1),
            Err(LogError::ResponseTooLarge)
        ));
    }
    #[tokio::test]
    async fn rejects_unauthorized_names_and_invalid_bounds_before_network() {
        let client = LokiClient::new("https://example.com").unwrap();
        let b = binding();
        for (team, service, deployment) in [
            ("beta", "svc", "deploy-a"),
            ("alpha", "other", "deploy-a"),
            ("alpha", "svc", "other"),
        ] {
            assert!(matches!(
                client.recent(&b, team, service, deployment, 60, 10).await,
                Err(LogError::Unauthorized)
            ));
        }
        for (seconds, limit) in [(0, 1), (3601, 1), (1, 0), (1, 501)] {
            assert!(matches!(
                client
                    .recent(&b, "alpha", "svc", "deploy-a", seconds, limit)
                    .await,
                Err(LogError::Bounds)
            ));
        }
    }
    #[test]
    fn rejects_insecure_or_ambiguous_endpoints() {
        for url in [
            "http://example.com",
            "https://user@example.com",
            "https://example.com/?query=x",
            "https://example.com/#x",
        ] {
            assert!(matches!(LokiClient::new(url), Err(LogError::Configuration)));
        }
    }
}
