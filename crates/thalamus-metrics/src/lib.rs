//! Read-only Prometheus instant queries for live serving metrics.
//!
//! Presets require Prometheus series to carry the exact `namespace`, `service`,
//! and `deployment` labels supplied by the caller. In particular, GPU exporter
//! series often need scrape-time relabeling to provide these labels. A missing
//! series is an empty result, not a zero; no label or metric mapping is inferred.
//! The caller must derive `Authorization` from its authenticated principal.

use std::time::{Duration, SystemTime};

use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

const MAX_LABEL_BYTES: usize = 256;
const MAX_PROMQL_BYTES: usize = 16 * 1024;

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid Prometheus URL: {0}")]
    InvalidUrl(String),
    #[error("label {0} must be between 1 and 256 bytes")]
    InvalidLabel(&'static str),
    #[error("free-form PromQL requires platform administrator authorization")]
    Forbidden,
    #[error("PromQL must be between 1 and 16384 bytes")]
    InvalidPromql,
    #[error("Prometheus HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("invalid Prometheus response: {0}")]
    InvalidResponse(#[from] serde_json::Error),
    #[error("Prometheus query failed ({kind}): {message}")]
    Source { kind: String, message: String },
    #[error("Prometheus response is missing query data")]
    MissingData,
}

/// Authenticated caller's platform role, established by the API layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Authorization {
    Admin,
    NonAdmin,
}

/// Exact labels selecting one deployment of a published service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Target {
    pub namespace: String,
    pub service: String,
    pub deployment: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Percentile {
    P50,
    P90,
    P95,
    P99,
}

impl Percentile {
    fn promql(self) -> &'static str {
        match self {
            Self::P50 => "0.5",
            Self::P90 => "0.9",
            Self::P95 => "0.95",
            Self::P99 => "0.99",
        }
    }
}

/// Latencies are seconds; throughput is output tokens/second; queue is waiting
/// requests; KV cache and GPU utilization use the exporter's native units.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Preset {
    TimeToFirstToken(Percentile),
    InterTokenLatency(Percentile),
    Throughput,
    Queue,
    KvCache,
    GpuUtilization,
}

/// The Prometheus result is retained verbatim, including each sample timestamp,
/// labels, string-valued samples (including NaN/Inf), and native histograms.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QueryResult {
    #[serde(rename = "resultType")]
    pub result_type: String,
    pub result: Value,
}

/// A successful observation records when it was received as well as source
/// sample timestamps in `result`. An empty vector is still an empty vector.
#[derive(Clone, Debug)]
pub struct Sample {
    pub observed_at: SystemTime,
    pub result: QueryResult,
    pub warnings: Vec<String>,
}

/// A source failure is never represented as a numeric zero. A stale sample is
/// supplied only when the caller explicitly passes the previous successful one.
#[derive(Debug)]
pub enum Observation {
    Current(Sample),
    Stale {
        attempted_at: SystemTime,
        previous: Sample,
        error: Error,
    },
    Unknown {
        attempted_at: SystemTime,
        error: Error,
    },
}

impl Observation {
    fn from_result(result: Result<Sample, Error>, previous: Option<Sample>) -> Self {
        match result {
            Ok(sample) => Self::Current(sample),
            Err(error) => match previous {
                Some(previous) => Self::Stale {
                    attempted_at: SystemTime::now(),
                    previous,
                    error,
                },
                None => Self::Unknown {
                    attempted_at: SystemTime::now(),
                    error,
                },
            },
        }
    }
}

/// Bounded exact-match selectors, encoded as PromQL string literals rather than
/// regular expressions. Values are checked before any query is sent.
fn label(value: &str, name: &'static str) -> Result<String, Error> {
    if value.is_empty() || value.len() > MAX_LABEL_BYTES {
        return Err(Error::InvalidLabel(name));
    }
    let mut escaped = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            c if c.is_control() && (c as u32) <= 0xff => {
                use std::fmt::Write;
                write!(&mut escaped, "\\x{:02x}", c as u32)
                    .expect("writing to a string cannot fail");
            }
            c if c.is_control() => {
                use std::fmt::Write;
                write!(&mut escaped, "\\u{:04x}", c as u32)
                    .expect("writing to a string cannot fail");
            }
            c => escaped.push(c),
        }
    }
    Ok(escaped)
}

/// Construct one of the fixed, bounded, exact-label queries.
pub fn preset_query(preset: Preset, target: &Target) -> Result<String, Error> {
    let selector = format!(
        "namespace=\"{}\",service=\"{}\",deployment=\"{}\"",
        label(&target.namespace, "namespace")?,
        label(&target.service, "service")?,
        label(&target.deployment, "deployment")?,
    );
    Ok(match preset {
        Preset::TimeToFirstToken(p) => format!(
            "histogram_quantile({}, sum by (le) (rate(vllm:time_to_first_token_seconds_bucket{{{selector}}}[5m])))",
            p.promql()
        ),
        Preset::InterTokenLatency(p) => format!(
            "histogram_quantile({}, sum by (le) (rate(vllm:inter_token_latency_seconds_bucket{{{selector}}}[5m])))",
            p.promql()
        ),
        Preset::Throughput => format!("sum(rate(vllm:generation_tokens_total{{{selector}}}[5m]))"),
        Preset::Queue => format!("sum(vllm:num_requests_waiting{{{selector}}})"),
        Preset::KvCache => format!("avg(vllm:kv_cache_usage_perc{{{selector}}})"),
        Preset::GpuUtilization => format!("avg(DCGM_FI_DEV_GPU_UTIL{{{selector}}})"),
    })
}

#[derive(Deserialize)]
struct Response {
    status: String,
    data: Option<QueryResult>,
    #[serde(rename = "errorType")]
    error_type: Option<String>,
    error: Option<String>,
    #[serde(default)]
    warnings: Vec<String>,
}

fn parse_response(body: &str) -> Result<Sample, Error> {
    let response: Response = serde_json::from_str(body)?;
    if response.status != "success" {
        return Err(Error::Source {
            kind: response.error_type.unwrap_or_else(|| "unknown".into()),
            message: response
                .error
                .unwrap_or_else(|| "unspecified Prometheus error".into()),
        });
    }
    Ok(Sample {
        observed_at: SystemTime::now(),
        result: response.data.ok_or(Error::MissingData)?,
        warnings: response.warnings,
    })
}

/// Configured client for Prometheus's `/api/v1/query` instant-query endpoint.
#[derive(Clone, Debug)]
pub struct PrometheusClient {
    client: Client,
    query_url: Url,
}

impl PrometheusClient {
    /// The base URL may contain a reverse-proxy path prefix. Query strings and
    /// fragments are rejected rather than silently changing the endpoint.
    pub fn new(base_url: &str) -> Result<Self, Error> {
        let client = Client::builder().timeout(Duration::from_secs(15)).build()?;
        Self::with_client(base_url, client)
    }

    /// Supply a preconfigured client, e.g. for TLS or authentication headers.
    pub fn with_client(base_url: &str, client: Client) -> Result<Self, Error> {
        let mut url = Url::parse(base_url).map_err(|e| Error::InvalidUrl(e.to_string()))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(Error::InvalidUrl(
                "expected an HTTP(S) base URL without query or fragment".into(),
            ));
        }
        url.path_segments_mut()
            .map_err(|_| Error::InvalidUrl("URL cannot be a base".into()))?
            .pop_if_empty()
            .extend(["api", "v1", "query"]);
        Ok(Self {
            client,
            query_url: url,
        })
    }

    async fn execute(&self, query: &str) -> Result<Sample, Error> {
        let body = self
            .client
            .post(self.query_url.clone())
            .form(&[("query", query)])
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        parse_response(&body)
    }

    pub async fn query_preset(&self, preset: Preset, target: &Target) -> Result<Sample, Error> {
        self.execute(&preset_query(preset, target)?).await
    }

    /// Authorization is checked before the HTTP request. The API layer must
    /// supply the caller's actual authenticated role, never a user input flag.
    pub async fn query_promql(
        &self,
        query: &str,
        authorization: Authorization,
    ) -> Result<Sample, Error> {
        if authorization != Authorization::Admin {
            return Err(Error::Forbidden);
        }
        if query.is_empty() || query.len() > MAX_PROMQL_BYTES {
            return Err(Error::InvalidPromql);
        }
        self.execute(query).await
    }

    /// Convert a preset query failure to explicit stale/unknown state. Only
    /// pass a previous sample for the same preset and target.
    pub async fn observe_preset(
        &self,
        preset: Preset,
        target: &Target,
        previous: Option<Sample>,
    ) -> Observation {
        Observation::from_result(self.query_preset(preset, target).await, previous)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> Target {
        Target {
            namespace: "prod".into(),
            service: "coding".into(),
            deployment: "gpu-a".into(),
        }
    }

    #[test]
    fn presets_use_exact_labels_and_real_metrics() {
        let t = target();
        let selector = "namespace=\"prod\",service=\"coding\",deployment=\"gpu-a\"";
        for (preset, metric) in [
            (
                Preset::TimeToFirstToken(Percentile::P50),
                "vllm:time_to_first_token_seconds_bucket",
            ),
            (
                Preset::InterTokenLatency(Percentile::P99),
                "vllm:inter_token_latency_seconds_bucket",
            ),
            (Preset::Throughput, "vllm:generation_tokens_total"),
            (Preset::Queue, "vllm:num_requests_waiting"),
            (Preset::KvCache, "vllm:kv_cache_usage_perc"),
            (Preset::GpuUtilization, "DCGM_FI_DEV_GPU_UTIL"),
        ] {
            let q = preset_query(preset, &t).unwrap();
            assert!(q.contains(&format!("{metric}{{{selector}}}")), "{q}");
        }
        assert!(
            preset_query(Preset::InterTokenLatency(Percentile::P99), &t)
                .unwrap()
                .starts_with("histogram_quantile(0.99,")
        );
        for preset in [
            Preset::TimeToFirstToken(Percentile::P95),
            Preset::InterTokenLatency(Percentile::P95),
        ] {
            assert!(
                preset_query(preset, &t)
                    .unwrap()
                    .starts_with("histogram_quantile(0.95,")
            );
        }
    }

    #[test]
    fn untrusted_labels_cannot_break_the_selector() {
        let mut t = target();
        t.service = "x\"} or up{job=\"evil\\\n\t".into();
        let q = preset_query(Preset::Queue, &t).unwrap();
        assert_eq!(
            q,
            "sum(vllm:num_requests_waiting{namespace=\"prod\",service=\"x\\\"} or up{job=\\\"evil\\\\\\n\\t\",deployment=\"gpu-a\"})"
        );
        t.service = "x".repeat(257);
        assert!(matches!(
            preset_query(Preset::Queue, &t),
            Err(Error::InvalidLabel("service"))
        ));
        t.service.clear();
        assert!(matches!(
            preset_query(Preset::Queue, &t),
            Err(Error::InvalidLabel("service"))
        ));
    }

    #[test]
    fn preserves_timestamp_empty_and_special_values() {
        let sample = parse_response(r#"{"status":"success","data":{"resultType":"vector","result":[{"metric":{"service":"coding"},"value":[1720000000.25,"NaN"]}]},"warnings":["partial"]}"#).unwrap();
        assert_eq!(sample.result.result[0]["value"][0], 1720000000.25);
        assert_eq!(sample.result.result[0]["value"][1], "NaN");
        assert_eq!(sample.warnings, vec!["partial"]);
        let empty =
            parse_response(r#"{"status":"success","data":{"resultType":"vector","result":[]}}"#)
                .unwrap();
        assert_eq!(empty.result.result, serde_json::json!([]));
    }

    #[test]
    fn failure_keeps_prior_sample_or_is_unknown() {
        let previous =
            parse_response(r#"{"status":"success","data":{"resultType":"vector","result":[]}}"#)
                .unwrap();
        let failure = || {
            parse_response(r#"{"status":"error","errorType":"execution","error":"unavailable"}"#)
        };
        match Observation::from_result(failure(), Some(previous.clone())) {
            Observation::Stale {
                previous: sample,
                error: Error::Source { kind, message },
                ..
            } => {
                assert_eq!(sample.result, previous.result);
                assert_eq!(kind, "execution");
                assert_eq!(message, "unavailable");
            }
            _ => panic!("expected stale source failure"),
        }
        assert!(matches!(
            Observation::from_result(failure(), None),
            Observation::Unknown { .. }
        ));
        assert!(matches!(
            parse_response("{}"),
            Err(Error::InvalidResponse(_))
        ));
    }

    #[test]
    fn endpoint_retains_prefix_and_rejects_ambiguous_urls() {
        let client = PrometheusClient::new("https://example.test/prometheus/").unwrap();
        assert_eq!(
            client.query_url.as_str(),
            "https://example.test/prometheus/api/v1/query"
        );
        assert!(PrometheusClient::new("https://example.test/?query=x").is_err());
        assert!(PrometheusClient::new("file:///tmp/prometheus").is_err());
    }

    #[tokio::test]
    async fn free_form_rejects_non_admin_before_contacting_source() {
        let client = PrometheusClient::new("http://127.0.0.1:1").unwrap();
        assert!(matches!(
            client.query_promql("up", Authorization::NonAdmin).await,
            Err(Error::Forbidden)
        ));
        assert!(matches!(
            client.query_promql("", Authorization::Admin).await,
            Err(Error::InvalidPromql)
        ));
        assert!(matches!(
            client
                .query_promql(&"x".repeat(MAX_PROMQL_BYTES + 1), Authorization::Admin)
                .await,
            Err(Error::InvalidPromql)
        ));
    }

    #[tokio::test]
    async fn sends_real_http_query_and_decodes_samples() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = stream.read(&mut buffer).unwrap();
                assert_ne!(count, 0);
                request.extend_from_slice(&buffer[..count]);
            }
            let headers = String::from_utf8_lossy(&request);
            assert!(headers.starts_with("POST /prefix/api/v1/query HTTP/1.1\r\n"));
            let body = r#"{"status":"success","data":{"resultType":"vector","result":[{"metric":{},"value":[1234,"7"]}]}}"#;
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        });
        let client = PrometheusClient::new(&format!("http://{address}/prefix")).unwrap();
        let sample = client.query_preset(Preset::Queue, &target()).await.unwrap();
        assert_eq!(
            sample.result.result[0]["value"],
            serde_json::json!([1234, "7"])
        );
        server.join().unwrap();
    }
}
