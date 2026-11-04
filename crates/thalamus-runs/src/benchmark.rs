//! Versioned inference-perf evidence and comparisons of measured load curves.
//!
//! A deserialized result is not trusted until it has been validated against its
//! persisted run. In particular, raw artifacts must go through
//! [`parse_benchmark_result`] before being returned to an API consumer.

use crate::{Error, Result, Run, RunConditions, RunKind, RunRequest, Workload};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Number;
use std::collections::BTreeMap;
use utoipa::ToSchema;
use uuid::Uuid;

pub const BENCHMARK_SCHEMA_VERSION: u32 = 1;
pub const BENCHMARK_MAX_POINTS: usize = 16;
pub const BENCHMARK_MAX_RESULT_BYTES: usize = 2 * 1024 * 1024;
pub const INFERENCE_PERF_IMAGE: &str = "quay.io/inference-perf/inference-perf@sha256:d247e6ad725dc5bb56df71d115abc8ca455bba9f0ccd4110e14bbb56504250ec";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkResult {
    pub schema_version: u32,
    #[serde(deserialize_with = "canonical_uuid")]
    #[schema(value_type = String, format = "uuid")]
    pub run_id: Uuid,
    #[serde(deserialize_with = "exact_request")]
    pub request: RunRequest,
    pub runner: BenchmarkRunner,
    pub tokenizer: BenchmarkTokenizer,
    pub definition: BenchmarkDefinition,
    pub points: Vec<BenchmarkPoint>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkRunner {
    pub name: String,
    pub version: String,
    pub adapter_version: String,
    /// Digest-pinned adapter image from the persisted prepared Job.
    pub image: String,
    pub upstream_image: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkTokenizer {
    pub repository: String,
    pub revision: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkDefinition {
    pub name: String,
    pub seed: u64,
    pub warmup_seconds: u32,
    pub spec: BenchmarkWorkloadSpec,
}

/// This is the exact named workload object, including SLO thresholds. Numbers
/// are retained as JSON numbers so comparison does not normalize the spec.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkWorkloadSpec {
    pub load: BenchmarkLoad,
    pub data: BenchmarkData,
    pub goodput: BenchmarkGoodputThresholds,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum BenchmarkLoad {
    Constant { stages: Vec<ConstantStage> },
    Concurrent { stages: Vec<ConcurrentStage> },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ConstantStage {
    pub rate: u32,
    pub duration: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ConcurrentStage {
    pub concurrency_level: u32,
    pub num_requests: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum BenchmarkData {
    Random {
        input_len: u32,
        output_len: u32,
    },
    SharedPrefix {
        num_groups: u32,
        num_prompts_per_group: u32,
        system_prompt_len: u32,
        question_len: u32,
        output_len: u32,
        enable_multi_turn_chat: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkGoodputThresholds {
    #[schema(value_type = f64)]
    pub ttft: Number,
    #[schema(value_type = f64)]
    pub itl: Number,
}

/// A stage repeats the corresponding configured load stage with its type.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum BenchmarkStage {
    Constant {
        rate: u32,
        duration: u32,
    },
    Concurrent {
        concurrency_level: u32,
        num_requests: u32,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkPoint {
    pub stage: BenchmarkStage,
    pub successes: u64,
    pub failures: u64,
    /// Upstream's observed request-start rate: recorded request count divided
    /// by the time between first and last start, not the scheduled load rate.
    /// Null when the upstream start span was unavailable or zero.
    #[serde(deserialize_with = "required_option")]
    #[schema(required = true, nullable)]
    pub achieved_request_rate: Option<f64>,
    #[serde(deserialize_with = "required_option")]
    #[schema(required = true, nullable)]
    pub ttft_seconds: Option<BenchmarkPercentiles>,
    #[serde(deserialize_with = "required_option")]
    #[schema(required = true, nullable)]
    pub itl_seconds: Option<BenchmarkPercentiles>,
    pub throughput: BenchmarkThroughput,
    #[serde(deserialize_with = "required_option")]
    #[schema(required = true, nullable)]
    pub goodput: Option<BenchmarkGoodput>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkPercentiles {
    pub p50: f64,
    pub p95: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkThroughput {
    pub requests_per_second: f64,
    #[serde(deserialize_with = "required_option")]
    #[schema(required = true, nullable)]
    pub output_tokens_per_second: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkGoodput {
    pub requests_per_second: f64,
}

fn canonical_uuid<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Uuid, D::Error> {
    let value = String::deserialize(deserializer)?;
    let id = Uuid::parse_str(&value).map_err(serde::de::Error::custom)?;
    if id.to_string() != value {
        return Err(serde::de::Error::custom("run_id must be a canonical UUID"));
    }
    Ok(id)
}

// serde_json::Value normally keeps the last occurrence of a duplicate key.
// An exact request must reject duplicates instead of silently hiding a forged
// field behind the canonical one.
struct UniqueJson(serde_json::Value);

impl<'de> Deserialize<'de> for UniqueJson {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        deserializer.deserialize_any(UniqueJsonVisitor)
    }
}

struct UniqueJsonVisitor;

impl<'de> serde::de::Visitor<'de> for UniqueJsonVisitor {
    type Value = UniqueJson;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON value without duplicate object keys")
    }

    fn visit_bool<E: serde::de::Error>(self, value: bool) -> std::result::Result<Self::Value, E> {
        Ok(UniqueJson(serde_json::Value::Bool(value)))
    }

    fn visit_i64<E: serde::de::Error>(self, value: i64) -> std::result::Result<Self::Value, E> {
        Ok(UniqueJson(value.into()))
    }

    fn visit_u64<E: serde::de::Error>(self, value: u64) -> std::result::Result<Self::Value, E> {
        Ok(UniqueJson(value.into()))
    }

    fn visit_f64<E: serde::de::Error>(self, value: f64) -> std::result::Result<Self::Value, E> {
        Number::from_f64(value)
            .map(|value| UniqueJson(serde_json::Value::Number(value)))
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    fn visit_str<E: serde::de::Error>(self, value: &str) -> std::result::Result<Self::Value, E> {
        Ok(UniqueJson(value.to_owned().into()))
    }

    fn visit_string<E: serde::de::Error>(
        self,
        value: String,
    ) -> std::result::Result<Self::Value, E> {
        Ok(UniqueJson(value.into()))
    }

    fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
        Ok(UniqueJson(serde_json::Value::Null))
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(
        self,
        mut sequence: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        let mut values = Vec::new();
        while let Some(UniqueJson(value)) = sequence.next_element()? {
            values.push(value);
        }
        Ok(UniqueJson(serde_json::Value::Array(values)))
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(
        self,
        mut entries: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        let mut values = serde_json::Map::new();
        while let Some(key) = entries.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(serde::de::Error::custom("duplicate request field"));
            }
            let UniqueJson(value) = entries.next_value()?;
            values.insert(key, value);
        }
        Ok(UniqueJson(serde_json::Value::Object(values)))
    }
}

// Do not silently discard extra request/evidence/workload fields or accept a
// differently encoded UUID: the adapter copies the job's exact RunRequest.
fn exact_request<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<RunRequest, D::Error> {
    let UniqueJson(value) = UniqueJson::deserialize(deserializer)?;
    let request = RunRequest::deserialize(&value).map_err(serde::de::Error::custom)?;
    let canonical = serde_json::to_value(&request).map_err(serde::de::Error::custom)?;
    if value != canonical {
        return Err(serde::de::Error::custom(
            "request is not the exact canonical RunRequest",
        ));
    }
    Ok(request)
}

// With serde's default Option handling an absent field is indistinguishable
// from an explicit null. The artifact contract requires the latter.
fn required_option<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

fn invalid(message: &str) -> Error {
    Error::Invalid(format!("invalid benchmark result: {message}"))
}

fn measured(value: f64) -> bool {
    value.is_finite() && value >= 0.0
}

fn threshold(value: &Number) -> bool {
    value
        .as_f64()
        .is_some_and(|value| value.is_finite() && value > 0.0 && value <= 3600.0)
}

fn valid_repository(repository: &str) -> bool {
    if repository.is_empty() || repository.len() > 256 {
        return false;
    }
    let mut count = 0;
    for segment in repository.split('/') {
        count += 1;
        if segment.is_empty()
            || segment.starts_with('.')
            || segment.starts_with('-')
            || segment.ends_with('.')
            || segment.ends_with('-')
            || segment.contains("..")
            || segment.contains("--")
            || !segment
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
        {
            return false;
        }
    }
    count <= 2
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordedConfig {
    #[serde(rename = "base_url")]
    _base_url: String,
    #[serde(rename = "model")]
    _model: String,
    benchmark: RecordedBenchmark,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordedBenchmark {
    tokenizer: BenchmarkTokenizer,
    workloads: BTreeMap<String, BenchmarkWorkloadSpec>,
}

fn recorded_env<'a>(env: &'a [serde_json::Value], name: &str) -> Result<&'a str> {
    let mut matches = env
        .iter()
        .filter(|entry| entry.get("name").and_then(|v| v.as_str()) == Some(name));
    let entry = matches
        .next()
        .ok_or_else(|| invalid("missing prepared runner environment"))?;
    if matches.next().is_some() || entry.get("valueFrom").is_some_and(|value| !value.is_null()) {
        return Err(invalid("ambiguous prepared runner environment"));
    }
    entry
        .get("value")
        .and_then(|value| value.as_str())
        .ok_or_else(|| invalid("invalid prepared runner environment"))
}

fn validate_prepared_job(run: &Run, result: &BenchmarkResult) -> Result<()> {
    let containers = run
        .job_spec
        .as_ref()
        .and_then(|spec| spec.pointer("/template/spec/containers"))
        .and_then(|containers| containers.as_array())
        .ok_or_else(|| invalid("missing prepared Job"))?;
    let mut runners = containers
        .iter()
        .filter(|container| container.get("name").and_then(|name| name.as_str()) == Some("runner"));
    let runner = runners
        .next()
        .ok_or_else(|| invalid("missing prepared runner"))?;
    if runners.next().is_some() {
        return Err(invalid("ambiguous prepared runner"));
    }
    let image = runner
        .get("image")
        .and_then(|image| image.as_str())
        .ok_or_else(|| invalid("missing prepared runner image"))?;
    let (image_name, digest) = image
        .rsplit_once("@sha256:")
        .ok_or_else(|| invalid("runner image is not digest pinned"))?;
    if image.len() > 2048
        || image_name.is_empty()
        || image_name.contains("://")
        || image_name
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '@' | '?' | '#' | '\\'))
        || digest.len() != 64
        || !digest
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        || image != result.runner.image
    {
        return Err(invalid("prepared runner image does not match"));
    }
    let env = runner
        .get("env")
        .and_then(|env| env.as_array())
        .ok_or_else(|| invalid("missing prepared runner environment"))?;
    if recorded_env(env, "THALAMUS_RUNNER_IMAGE")? != image {
        return Err(invalid("prepared runner image environment does not match"));
    }
    let encoded = recorded_env(env, "THALAMUS_BENCHMARK_CONFIG")?;
    if encoded.len() > 64 * 1024 {
        return Err(invalid("prepared benchmark configuration is too large"));
    }
    // Do not return the config or any JSON error that could contain its target
    // URL or model; only its immutable test identities belong in the result.
    let config: RecordedConfig = serde_json::from_str(encoded)
        .map_err(|_| invalid("invalid prepared benchmark configuration"))?;
    if config.benchmark.tokenizer != result.tokenizer
        || config.benchmark.workloads.get(&run.request.workload.name)
            != Some(&result.definition.spec)
    {
        return Err(invalid("prepared benchmark configuration does not match"));
    }
    Ok(())
}

impl BenchmarkWorkloadSpec {
    fn validate(&self, points: &[BenchmarkPoint], workload: &Workload) -> Result<()> {
        if points.is_empty() || points.len() > BENCHMARK_MAX_POINTS {
            return Err(invalid("point count is outside the allowed bounds"));
        }
        let mut dispatch = u64::from(workload.warmup_seconds);
        let mut scheduled_seconds = u64::from(workload.warmup_seconds);
        match &self.load {
            BenchmarkLoad::Constant { stages } => {
                if stages.len() != points.len() {
                    return Err(invalid("load stages and points do not correspond"));
                }
                for (stage, point) in stages.iter().zip(points) {
                    if !(1..=256).contains(&stage.rate)
                        || !(1..=3600).contains(&stage.duration)
                        || point.stage
                            != (BenchmarkStage::Constant {
                                rate: stage.rate,
                                duration: stage.duration,
                            })
                    {
                        return Err(invalid("invalid or mismatched constant stage"));
                    }
                    dispatch += u64::from(stage.rate) * u64::from(stage.duration);
                    scheduled_seconds += u64::from(stage.duration);
                }
            }
            BenchmarkLoad::Concurrent { stages } => {
                if stages.len() != points.len() {
                    return Err(invalid("load stages and points do not correspond"));
                }
                for (stage, point) in stages.iter().zip(points) {
                    if !(1..=256).contains(&stage.concurrency_level)
                        || stage.concurrency_level > workload.concurrency
                        || !(1..=1_000_000).contains(&stage.num_requests)
                        || point.stage
                            != (BenchmarkStage::Concurrent {
                                concurrency_level: stage.concurrency_level,
                                num_requests: stage.num_requests,
                            })
                    {
                        return Err(invalid("invalid or mismatched concurrent stage"));
                    }
                    dispatch += u64::from(stage.num_requests);
                }
            }
        }
        let data_valid = match &self.data {
            BenchmarkData::Random {
                input_len,
                output_len,
            } => (1..=65_536).contains(input_len) && (1..=16_384).contains(output_len),
            BenchmarkData::SharedPrefix {
                num_groups,
                num_prompts_per_group,
                system_prompt_len,
                question_len,
                output_len,
                ..
            } => {
                (1..=128).contains(num_groups)
                    && (1..=128).contains(num_prompts_per_group)
                    && (1..=65_536).contains(system_prompt_len)
                    && (1..=65_536).contains(question_len)
                    && (1..=16_384).contains(output_len)
            }
        };
        if !data_valid || !threshold(&self.goodput.ttft) || !threshold(&self.goodput.itl) {
            return Err(invalid("invalid workload data or goodput thresholds"));
        }
        let output_len = match &self.data {
            BenchmarkData::Random { output_len, .. }
            | BenchmarkData::SharedPrefix { output_len, .. } => *output_len,
        };
        if dispatch > u64::from(workload.max_requests)
            || scheduled_seconds > u64::from(workload.duration_seconds)
            || dispatch * u64::from(output_len) > u64::from(workload.model_budget_tokens)
        {
            return Err(invalid("workload exceeds the requested run budget"));
        }
        Ok(())
    }
}

impl BenchmarkResult {
    /// Validate the entire artifact against the authoritative persisted run.
    /// This must also be called for any result constructed or mutated in memory.
    pub fn validate_for(&self, run: &Run) -> Result<()> {
        if self.schema_version != BENCHMARK_SCHEMA_VERSION {
            return Err(invalid("unsupported schema version"));
        }
        if run.id.is_nil() || self.run_id != run.id || self.request != run.request {
            return Err(invalid("run identity or request does not match"));
        }
        run.request.validate()?;
        if run.request.kind != RunKind::Benchmark {
            return Err(invalid("run is not a benchmark"));
        }
        if self.runner.name != "inference-perf"
            || self.runner.version != "0.7.0"
            || self.runner.adapter_version != "1"
            || self.runner.upstream_image != INFERENCE_PERF_IMAGE
        {
            return Err(invalid("unsupported runner identity"));
        }
        if !valid_repository(&self.tokenizer.repository) {
            return Err(invalid("invalid tokenizer repository"));
        }
        if self.tokenizer.revision.len() != 40
            || !self
                .tokenizer
                .revision
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            return Err(invalid("tokenizer revision is not an immutable commit"));
        }
        let workload = &run.request.workload;
        if self.definition.name != workload.name
            || self.definition.seed != workload.seed
            || self.definition.warmup_seconds != workload.warmup_seconds
        {
            return Err(invalid("definition does not match the requested workload"));
        }
        validate_prepared_job(run, self)?;
        self.definition.spec.validate(&self.points, workload)?;
        for point in &self.points {
            let expected_requests = match &point.stage {
                BenchmarkStage::Constant { rate, duration } => {
                    u64::from(*rate) * u64::from(*duration)
                }
                BenchmarkStage::Concurrent { num_requests, .. } => u64::from(*num_requests),
            };
            if point.successes.checked_add(point.failures) != Some(expected_requests) {
                return Err(invalid("stage request counts do not match configured load"));
            }
            if point.successes > 0 && point.goodput.is_none() {
                return Err(invalid("configured goodput measurement is missing"));
            }
            if point.successes == 0
                && (point.ttft_seconds.is_some()
                    || point.itl_seconds.is_some()
                    || point.goodput.is_some()
                    || point.throughput.output_tokens_per_second.is_some()
                    || point.throughput.requests_per_second != 0.0)
            {
                return Err(invalid("measurements require successful requests"));
            }
            for distribution in [&point.ttft_seconds, &point.itl_seconds]
                .into_iter()
                .flatten()
            {
                if !measured(distribution.p50)
                    || !measured(distribution.p95)
                    || distribution.p95 < distribution.p50
                {
                    return Err(invalid("invalid latency percentiles"));
                }
            }
            if point
                .achieved_request_rate
                .is_some_and(|value| !measured(value))
                || !measured(point.throughput.requests_per_second)
                || point
                    .throughput
                    .output_tokens_per_second
                    .is_some_and(|value| !measured(value))
                || point
                    .goodput
                    .as_ref()
                    .is_some_and(|value| !measured(value.requests_per_second))
            {
                return Err(invalid("invalid throughput or goodput"));
            }
        }
        Ok(())
    }
}

/// Parse a size-bounded, versioned result and bind it to its stored run.
pub fn parse_benchmark_result(run: &Run, bytes: &[u8]) -> Result<BenchmarkResult> {
    if bytes.len() > BENCHMARK_MAX_RESULT_BYTES {
        return Err(invalid("artifact is too large"));
    }
    let result: BenchmarkResult = serde_json::from_slice(bytes)?;
    result.validate_for(run)?;
    Ok(result)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ComparedValue<T> {
    pub changed: bool,
    pub baseline: T,
    pub candidate: T,
}

impl<T: PartialEq> ComparedValue<T> {
    fn new(baseline: T, candidate: T) -> Self {
        Self {
            changed: baseline != candidate,
            baseline,
            candidate,
        }
    }
}

/// `changed` compares all conditions except `observed_at`. Both original
/// condition snapshots remain available, including their observation times.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ComparedConditions {
    pub changed: bool,
    pub baseline: RunConditions,
    pub candidate: RunConditions,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct BenchmarkComparison {
    pub baseline: BenchmarkResult,
    pub candidate: BenchmarkResult,
    pub desired_configuration: ComparedValue<String>,
    pub observed_runtime: ComparedValue<String>,
    pub conditions: ComparedConditions,
    pub deployment: ComparedValue<String>,
    /// Timestamps are provenance, not a condition difference by themselves.
    #[schema(value_type = ComparedValue<String>)]
    pub observed_at: ComparedValue<chrono::DateTime<chrono::Utc>>,
}

fn same_conditions(left: &RunConditions, right: &RunConditions) -> bool {
    left.hardware == right.hardware
        && left.nodes == right.nodes
        && left.replicas == right.replicas
        && left.cache_state == right.cache_state
        && left.competing_traffic == right.competing_traffic
}

/// Compare complete measured curves only when their test and measurement
/// identities are identical. Changed runtime/conditions remain explicit; they
/// do not prevent comparing an upgrade against its baseline.
pub fn compare_benchmark_results(
    baseline_run: &Run,
    baseline: BenchmarkResult,
    candidate_run: &Run,
    candidate: BenchmarkResult,
) -> Result<BenchmarkComparison> {
    baseline.validate_for(baseline_run)?;
    candidate.validate_for(candidate_run)?;
    let left = &baseline_run.request;
    let right = &candidate_run.request;
    if baseline_run.team != candidate_run.team
        || left.project_id != right.project_id
        || left.service != right.service
        || left.runner != right.runner
        || left.workload != right.workload
        || left.evidence.test_definition != right.evidence.test_definition
        || baseline.runner != candidate.runner
        || baseline.tokenizer != candidate.tokenizer
        || baseline.definition != candidate.definition
    {
        return Err(Error::Invalid(
            "benchmark test or measurement identities differ".into(),
        ));
    }
    let lc = &left.evidence.conditions;
    let rc = &right.evidence.conditions;
    Ok(BenchmarkComparison {
        baseline,
        candidate,
        desired_configuration: ComparedValue::new(
            left.evidence.desired_configuration.clone(),
            right.evidence.desired_configuration.clone(),
        ),
        observed_runtime: ComparedValue::new(
            left.evidence.observed_runtime.clone(),
            right.evidence.observed_runtime.clone(),
        ),
        conditions: ComparedConditions {
            changed: !same_conditions(lc, rc),
            baseline: lc.clone(),
            candidate: rc.clone(),
        },
        deployment: ComparedValue::new(left.deployment.clone(), right.deployment.clone()),
        observed_at: ComparedValue::new(lc.observed_at, rc.observed_at),
    })
}
