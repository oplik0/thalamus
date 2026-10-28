//! Admission metadata derived from a fresh route observation.
//!
//! This is not run proof: only the worker's separate prelaunch/ingestion
//! observations and its observed Job/Pod image establish execution identity.
use std::{
    collections::BTreeMap,
    io::{self, Write},
};

use k8s_openapi::api::batch::v1::Job;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thalamus_runs::{EvidenceIdentity, Run, RunConditions, RunKind, RunRequest, Workload};
use url::Url;

use crate::{Error, Result, RouteSnapshot, binding};

struct HashWriter(Sha256);
impl Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct CountWriter(usize);
impl Write for CountWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serialized_size<T: Serialize + ?Sized>(value: &T) -> Result<usize> {
    let mut writer = CountWriter::default();
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer.0)
}

fn identity<T: Serialize + ?Sized>(value: &T) -> Result<String> {
    let mut writer = HashWriter(Sha256::new());
    serde_json::to_writer(&mut writer, value)?;
    Ok(format!("sha256:{:x}", writer.0.finalize()))
}

fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(canonical_json).collect()),
        Value::Object(object) => {
            let mut keys: Vec<_> = object.keys().collect();
            keys.sort_unstable();
            let mut sorted = serde_json::Map::new();
            for key in keys {
                sorted.insert(
                    key.clone(),
                    canonical_json(object.get(key).expect("key came from object")),
                );
            }
            Value::Object(sorted)
        }
        _ => value.clone(),
    }
}

/// Operator-controlled runner/workload options, never a runtime observation.
/// Routing identity and endpoint values are intentionally absent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestProfile {
    pub kind: RunKind,
    pub route_target: String,
    pub options: Value,
}

/// Indexed by service and runner; profiles do not enumerate future deployments.
pub type RunnerProfiles = BTreeMap<String, BTreeMap<String, TestProfile>>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ValidationOptions<'a> {
    forbidden_model: &'a str,
    context_prompt: &'a str,
    context_tokens: u32,
    tool_parser: Option<&'a str>,
    reasoning: bool,
    #[serde(default)]
    responses: bool,
    #[serde(default)]
    anthropic: bool,
    responses_continuation: bool,
    timeout_seconds: u64,
}
/// Select the independently observed agentic route only when a validation
/// actually exercises a protocol exposed by the agentic service.
pub fn requires_agentic(kind: &RunKind, options: &Value) -> Result<bool> {
    if *kind != RunKind::Validation {
        return Ok(false);
    }
    let object = options
        .as_object()
        .ok_or_else(|| Error::Invalid("invalid validation test options".into()))?;
    let mut required = false;
    for key in ["responses", "responses_continuation", "anthropic"] {
        match object.get(key) {
            None => {}
            Some(Value::Bool(value)) => required |= value,
            _ => return Err(Error::Invalid(format!("invalid validation option {key}"))),
        }
    }
    Ok(required)
}
/// Match an independently observed route against an authorized logical stage.
/// This does not itself prove the snapshot; only the appropriate observer can
/// establish the agentic flag and route identity.
pub fn matches_route(
    snapshot: &RouteSnapshot,
    deployment: &str,
    kind: &RunKind,
    route_target: &str,
    public_route: &str,
    agentic: bool,
) -> bool {
    if snapshot.agentic != agentic {
        return false;
    }
    match (kind, route_target, agentic) {
        (RunKind::Validation, "candidate", true) => snapshot
            .route
            .strip_prefix("agentic-cand-")
            .is_some_and(|hash| {
                hash.len() == 32
                    && hash
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            }),
        (RunKind::Validation, "public", true) => {
            snapshot.route != public_route
                && snapshot
                    .route
                    .strip_prefix(&format!("{}-", snapshot.service))
                    .is_some_and(|instance| !instance.is_empty())
        }
        (RunKind::Validation, "candidate", false) => {
            snapshot.route == format!("{deployment}-candidate")
        }
        (RunKind::Validation, "public", false) => {
            !public_route.is_empty() && snapshot.route == public_route
        }
        (RunKind::Benchmark, "candidate", false) => {
            snapshot.route == format!("{deployment}-benchmark")
        }
        _ => false,
    }
}

fn validate_options(kind: &RunKind, service: &str, options: &Value) -> Result<()> {
    if serialized_size(options)? > 64 * 1024 {
        return Err(Error::Invalid(
            "test options exceed runner environment limit".into(),
        ));
    }
    match kind {
        RunKind::Validation => {
            let options = ValidationOptions::deserialize(options)
                .map_err(|_| Error::Invalid("invalid validation test options".into()))?;
            if options.forbidden_model.trim().is_empty()
                || options.forbidden_model == service
                || options.forbidden_model.chars().any(char::is_control)
                || options.context_prompt.is_empty()
                || options.context_prompt.len() > 2_000_000
                || options.context_tokens == 0
                || options.context_tokens > 1_000_000
                || options
                    .tool_parser
                    .as_ref()
                    .is_some_and(|s| s.trim().is_empty())
                || !(1..=300).contains(&options.timeout_seconds)
            {
                return Err(Error::Invalid(
                    "validation test options outside permitted bounds".into(),
                ));
            }
            let _ = (
                options.reasoning,
                options.responses,
                options.anthropic,
                options.responses_continuation,
            );
            Ok(())
        }
        RunKind::Benchmark => thalamus_runs::validate_benchmark_options(options)
            .map_err(|_| Error::Invalid("invalid benchmark test options".into())),
        RunKind::Debug => Err(Error::Invalid(
            "debug does not use runner test profiles".into(),
        )),
    }
}

/// Validate the shared service/runner profile schema at startup. Workload
/// authorization and profile-to-runner matching remain the caller's job.
pub fn validate_profiles(profiles: &RunnerProfiles) -> Result<()> {
    if profiles.is_empty() {
        return Err(Error::Invalid(
            "no service runner profiles configured".into(),
        ));
    }
    for (service, runners) in profiles {
        if service.is_empty() || service.chars().any(char::is_control) || runners.is_empty() {
            return Err(Error::Invalid("invalid service runner profile".into()));
        }
        for (runner, profile) in runners {
            if runner.is_empty()
                || runner.chars().any(char::is_control)
                || !matches!(profile.route_target.as_str(), "candidate" | "public")
                || (profile.kind == RunKind::Benchmark && profile.route_target != "candidate")
            {
                return Err(Error::Invalid("invalid runner profile target".into()));
            }
            validate_options(&profile.kind, service, &profile.options)?;
        }
    }
    Ok(())
}

/// Check the approved workload against the selected service-level test profile.
/// Both API admission and worker preparation must use the same bounds.
pub fn validate_test_workload(profile: &TestProfile, workload: &Workload) -> Result<()> {
    validate_options_workload(&profile.kind, &profile.options, workload)
}

/// Validate workload bounds without cloning potentially large test options.
pub fn validate_options_workload(
    kind: &RunKind,
    options: &Value,
    workload: &Workload,
) -> Result<()> {
    match kind {
        RunKind::Validation => {
            let context = options
                .get("context_tokens")
                .and_then(Value::as_u64)
                .ok_or_else(|| Error::Invalid("validation context budget is missing".into()))?;
            if workload.name != "functional"
                || workload.concurrency != 1
                || workload.warmup_seconds != 0
                || context == 0
                || context > u64::from(workload.model_budget_tokens)
                || workload.tool_budget_calls == 0
            {
                return Err(Error::Invalid(
                    "functional workload does not match the approved test".into(),
                ));
            }
            Ok(())
        }
        RunKind::Benchmark => thalamus_runs::validate_benchmark_workload(options, workload)
            .map_err(|_| {
                Error::Invalid("benchmark workload does not match the approved test".into())
            }),
        RunKind::Debug => Err(Error::Invalid(
            "debug does not use runner test profiles".into(),
        )),
    }
}

/// Recover the exact approved test options from the persisted prepared Job.
/// This is useful during crash recovery when the operator profile may have
/// changed. Callers must separately bind the Job endpoint to a live route.
pub fn options_from_job(run: &Run, job: &Job) -> Result<Value> {
    let spec = job
        .spec
        .as_ref()
        .and_then(|spec| spec.template.spec.as_ref())
        .ok_or_else(|| Error::Invalid("runner Job spec is missing".into()))?;
    let mut containers = spec.containers.iter().filter(|c| c.name == "runner");
    let runner = containers
        .next()
        .ok_or_else(|| Error::Invalid("runner container is missing".into()))?;
    if containers.next().is_some() {
        return Err(Error::Invalid("ambiguous runner container".into()));
    }
    let literal = |name: &str| -> Result<&str> {
        let mut values = runner
            .env
            .iter()
            .flatten()
            .filter(|entry| entry.name == name);
        let entry = values
            .next()
            .ok_or_else(|| Error::Invalid("runner environment is missing".into()))?;
        if values.next().is_some() || entry.value_from.is_some() {
            return Err(Error::Invalid("runner environment is ambiguous".into()));
        }
        entry
            .value
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::Invalid("runner environment is empty".into()))
    };
    let parsed: RunRequest = serde_json::from_str(literal("THALAMUS_RUN_REQUEST")?)?;
    let mut uuid_buffer = [0; 36];
    let run_id: &str = run.id.hyphenated().encode_lower(&mut uuid_buffer);
    if literal("THALAMUS_RUN_ID")? != run_id || parsed != run.request {
        return Err(Error::Invalid(
            "runner request identity differs from persisted run".into(),
        ));
    }
    let field = match &run.request.kind {
        RunKind::Validation => "THALAMUS_VALIDATION_CONFIG",
        RunKind::Benchmark => "THALAMUS_BENCHMARK_CONFIG",
        RunKind::Debug => {
            return Err(Error::Invalid(
                "debug does not use runner test profiles".into(),
            ));
        }
    };
    let mut config: Value = serde_json::from_str(literal(field)?)?;
    let object = config
        .as_object_mut()
        .ok_or_else(|| Error::Invalid("runner config is not an object".into()))?;
    if object.get("model").and_then(Value::as_str) != Some(run.request.service.as_str()) {
        return Err(Error::Invalid("runner model differs from service".into()));
    }
    let options = match &run.request.kind {
        RunKind::Validation => {
            for key in [
                "base_url",
                "model",
                "suite",
                "backend_header",
                "expected_backend",
                "old_backend",
            ] {
                object.remove(key);
            }
            config
        }
        RunKind::Benchmark => {
            if object
                .keys()
                .any(|key| !matches!(key.as_str(), "base_url" | "model" | "benchmark"))
            {
                return Err(Error::Invalid(
                    "unexpected benchmark runner config field".into(),
                ));
            }
            object
                .remove("benchmark")
                .ok_or_else(|| Error::Invalid("benchmark options missing".into()))?
        }
        RunKind::Debug => unreachable!(),
    };
    validate_options(&run.request.kind, &run.request.service, &options)?;
    Ok(options)
}

/// Compare admission metadata with a fresh independent observation, ignoring
/// only its historical collection timestamp. The observed record, never this
/// requested identity, remains the authority for a completed run.
pub fn same_request_evidence(expected: &EvidenceIdentity, observed: &EvidenceIdentity) -> bool {
    expected.desired_configuration == observed.desired_configuration
        && expected.observed_runtime == observed.observed_runtime
        && expected.test_definition == observed.test_definition
        && expected.conditions.hardware == observed.conditions.hardware
        && expected.conditions.nodes == observed.conditions.nodes
        && expected.conditions.replicas == observed.conditions.replicas
        && expected.conditions.cache_state == observed.conditions.cache_state
        && expected.conditions.competing_traffic == observed.conditions.competing_traffic
}

/// Resolve a run-scoped runner configuration from a fresh, independently
/// observed singleton route and administrator-approved test options. Callers
/// authorize the service/deployment first (committed catalog or durable rollout
/// stage); this function does not make an authorization decision.
pub fn resolve_runner_config(
    snapshot: &RouteSnapshot,
    deployment: &str,
    kind: &RunKind,
    route_target: &str,
    public_route: &str,
    operator_options: &Value,
) -> Result<Value> {
    validate_options(kind, &snapshot.service, operator_options)?;
    let agentic = requires_agentic(kind, operator_options)?;
    if !matches_route(
        snapshot,
        deployment,
        kind,
        route_target,
        public_route,
        agentic,
    ) || snapshot.deployments.len() != 1
        || snapshot.deployments[0].deployment != deployment
        || snapshot.agentic != snapshot.deployments[0].underlying_model.is_some()
        || snapshot.weights.len() != 1
        || snapshot
            .weights
            .get(deployment)
            .is_none_or(|weight| *weight == 0)
    {
        return Err(Error::Invalid(
            "route does not exclusively target the requested deployment".into(),
        ));
    }
    let suffix = match kind {
        RunKind::Validation => "/chat/completions",
        RunKind::Benchmark => "/completions",
        RunKind::Debug => unreachable!(),
    };
    let mut base_paths = snapshot
        .paths
        .iter()
        .filter_map(|path| path.strip_suffix(suffix))
        .filter(|path| path.ends_with("/v1"));
    let path = base_paths
        .next()
        .ok_or_else(|| Error::Invalid("route has no matching inference path".into()))?;
    if base_paths.next().is_some() {
        return Err(Error::Invalid("route inference path is ambiguous".into()));
    }

    // Prefer TLS when the same route also offers cleartext. A unique listener
    // is required; silently choosing between unrelated hosts or ports would
    // make an unobserved operator endpoint look authoritative.
    let mut listener = None;
    let mut rank = 0;
    let mut ambiguous = false;
    for candidate in &snapshot.listeners {
        if !snapshot.hostnames.contains(&candidate.hostname)
            || !snapshot.ports.contains(&candidate.port)
        {
            continue;
        }
        let candidate_rank = match candidate.protocol.as_str() {
            "HTTP" => 1,
            "HTTPS" => 2,
            _ => continue,
        };
        if candidate_rank > rank {
            listener = Some(candidate);
            rank = candidate_rank;
            ambiguous = false;
        } else if candidate_rank == rank {
            ambiguous = true;
        }
    }
    let listener = listener
        .filter(|_| !ambiguous)
        .ok_or_else(|| Error::Invalid("route listener is missing or ambiguous".into()))?;
    let scheme = if listener.protocol == "HTTPS" {
        "https"
    } else {
        "http"
    };
    let mut base = Url::parse(&format!(
        "{scheme}://{}:{}/",
        listener.hostname, listener.port
    ))
    .map_err(|_| Error::Invalid("invalid observed route listener".into()))?;
    base.set_path(path);
    let base_url = base.as_str().trim_end_matches('/').to_owned();
    let config = match kind {
        RunKind::Validation => {
            let mut config = operator_options
                .as_object()
                .cloned()
                .ok_or_else(|| Error::Invalid("validation options are not an object".into()))?;
            config.insert("base_url".into(), json!(base_url));
            config.insert("model".into(), json!(snapshot.service));
            config.insert("suite".into(), json!(route_target));
            config.insert("backend_header".into(), json!("x-thalamus-backend"));
            config.insert(
                "expected_backend".into(),
                json!(snapshot.deployments[0].backend),
            );
            Value::Object(config)
        }
        RunKind::Benchmark => json!({
            "base_url": base_url, "model": snapshot.service, "benchmark": operator_options
        }),
        RunKind::Debug => unreachable!(),
    };
    if serialized_size(&config)? > 64 * 1024 {
        return Err(Error::Invalid(
            "resolved runner config exceeds environment limit".into(),
        ));
    }
    if matches!(kind, RunKind::Validation) {
        thalamus_runs::validate_observed_validation_config(&config)
            .map_err(|_| Error::Invalid("invalid resolved validation configuration".into()))?;
    }
    let target = binding::target_from_config(kind, &snapshot.service, &config, route_target)
        .map_err(|_| Error::Invalid("resolved runner target is invalid".into()))?;
    binding::check_observation(
        &snapshot.service,
        deployment,
        &target,
        snapshot,
        &snapshot.route,
    )
    .map_err(|_| Error::Invalid("resolved runner does not match the observed route".into()))?;
    Ok(config)
}

/// Build the run request's descriptive metadata from a live observation and the
/// approved runner/workload. The requested runner name and workload are planned
/// test inputs, not an observed runner image. The actual runner image and Job
/// identity must be independently recorded by the worker at ingestion.
///
/// Unknown cache, traffic, and hardware/driver state are recorded as unknown,
/// and the time is the historical collection time, not a condition to recreate
/// exactly at a later observation.
pub fn request_evidence(
    snapshot: &RouteSnapshot,
    deployment: &str,
    runner: &str,
    workload: &Workload,
    operator_options: &Value,
) -> Result<EvidenceIdentity> {
    if snapshot.service.is_empty()
        || snapshot.route.is_empty()
        || deployment.is_empty()
        || runner.is_empty()
        || !operator_options.is_object()
        || runner.chars().any(char::is_control)
        || snapshot.deployments.len() != 1
    {
        return Err(Error::Invalid(
            "admission requires a single observed deployment and runner".into(),
        ));
    }
    let target = &snapshot.deployments[0];
    if target.deployment != deployment
        || snapshot.agentic != target.underlying_model.is_some()
        || target.revision.is_empty()
        || target.flux_revision.is_empty()
        || target.backend.is_empty()
        || target.nodes.is_empty()
        || target.nodes.iter().any(|n| n.is_empty())
    {
        return Err(Error::Invalid(
            "admission deployment observation is incomplete".into(),
        ));
    }
    let replicas = u32::try_from(target.nodes.len())
        .map_err(|_| Error::Invalid("too many serving replicas".into()))?;
    Ok(EvidenceIdentity {
        desired_configuration: identity(&(
            &snapshot.service,
            &snapshot.route,
            &snapshot.agentic,
            &snapshot.hostnames,
            &snapshot.paths,
            &snapshot.ports,
            &snapshot.listeners,
            &snapshot.weights,
            &target.deployment,
            &target.revision,
            &target.flux_revision,
            &target.backend,
            &target.desired,
            &target.rollout_configuration,
        ))?,
        observed_runtime: identity(&(
            &target.runtime,
            &target.underlying_model,
            &target.engine_spec,
            &target.nodes,
        ))?,
        test_definition: test_definition(runner, workload, operator_options)?,
        conditions: RunConditions {
            hardware: "not observed".into(),
            nodes: target.nodes.clone(),
            replicas,
            cache_state: "not observed".into(),
            competing_traffic: "not observed".into(),
            observed_at: snapshot.observed_at,
        },
    })
}

/// Stable planned test identity, intentionally excluding model, endpoint,
/// backend, serving revision and observation time.
pub fn test_definition(
    runner: &str,
    workload: &Workload,
    operator_options: &Value,
) -> Result<String> {
    if runner.is_empty() || !operator_options.is_object() {
        return Err(Error::Invalid("invalid planned test identity".into()));
    }
    identity(&(runner, workload, canonical_json(operator_options)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DeploymentSnapshot, RouteListener};
    use chrono::{Duration, Utc};
    use std::collections::BTreeMap;
    use thalamus_core::{DesiredIdentity, RuntimeIdentity};

    fn fixture() -> (RouteSnapshot, Workload) {
        let sha = format!("sha256:{}", "a".repeat(64));
        let deployment = DeploymentSnapshot {
            deployment: "model-a".into(),
            revision: "rev-a".into(),
            flux_revision: "main@sha1:0123456789012345678901234567890123456789".into(),
            backend: "svc/model-a/rev-a".into(),
            desired: DesiredIdentity {
                rendered_resources: sha.clone(),
                resolved_dependencies: sha.clone(),
            },
            rollout_configuration: sha.clone(),
            runtime: RuntimeIdentity {
                image_digests: BTreeMap::from([("container/main".into(), sha.clone())]),
                model_content: sha.clone(),
                tokenizer_content: sha.clone(),
                effective_configuration: sha.clone(),
                component_versions: BTreeMap::from([("engine".into(), sha)]),
            },
            underlying_model: None,
            engine_spec: serde_json::json!({"image":"registry.example/model","args":["--served-model-name","svc"]}),
            nodes: vec!["node-a".into(), "node-a".into()],
        };
        let route = RouteSnapshot {
            observed_at: Utc::now(),
            service: "svc".into(),
            route: "model-a-candidate".into(),
            agentic: false,
            hostnames: vec!["candidate.example".into()],
            paths: vec!["/candidates/model-a/v1/chat/completions".into()],
            ports: vec![443],
            listeners: vec![RouteListener {
                hostname: "candidate.example".into(),
                port: 443,
                protocol: "HTTPS".into(),
            }],
            weights: BTreeMap::from([("model-a".into(), 1)]),
            deployments: vec![deployment],
        };
        let workload = Workload {
            name: "functional".into(),
            seed: 1,
            concurrency: 1,
            duration_seconds: 30,
            warmup_seconds: 0,
            max_requests: 20,
            model_budget_tokens: 4096,
            tool_budget_calls: 1,
        };
        (route, workload)
    }

    fn options() -> Value {
        json!({"forbidden_model":"other","context_prompt":"hello","context_tokens":1024,
            "tool_parser":null,"reasoning":false,"responses":false,"anthropic":false,
            "responses_continuation":false,"timeout_seconds":30})
    }

    #[test]
    fn metadata_records_unknown_conditions_and_historical_time() {
        let (mut route, workload) = fixture();
        let first = request_evidence(&route, "model-a", "runner", &workload, &options()).unwrap();
        assert_eq!(first.conditions.replicas, 2);
        assert_eq!(first.conditions.hardware, "not observed");
        assert_eq!(first.conditions.cache_state, "not observed");
        assert_eq!(first.conditions.competing_traffic, "not observed");
        route.observed_at += Duration::seconds(5);
        let second = request_evidence(&route, "model-a", "runner", &workload, &options()).unwrap();
        assert_eq!(first.desired_configuration, second.desired_configuration);
        assert_eq!(first.observed_runtime, second.observed_runtime);
        assert_eq!(first.test_definition, second.test_definition);
        assert_ne!(first.conditions.observed_at, second.conditions.observed_at);
        assert!(same_request_evidence(&first, &second));
        let mut different_nodes = second;
        different_nodes.conditions.nodes[0] = "other-node".into();
        assert!(!same_request_evidence(&first, &different_nodes));
    }

    #[test]
    fn metadata_changes_with_route_runtime_and_test_but_separates_them() {
        let (route, workload) = fixture();
        let original =
            request_evidence(&route, "model-a", "runner", &workload, &options()).unwrap();
        let mut changed = route.clone();
        changed.hostnames[0] = "other.example".into();
        changed.paths[0] = "/other/v1/chat/completions".into();
        changed.listeners[0].port = 8443;
        changed.weights.insert("model-a".into(), 0);
        changed.deployments[0].backend = "svc/model-a/rev-b".into();
        changed.deployments[0].revision = "rev-b".into();
        changed.deployments[0].flux_revision =
            "main@sha1:fedcba9876543210fedcba9876543210fedcba98".into();
        let new_route =
            request_evidence(&changed, "model-a", "runner", &workload, &options()).unwrap();
        assert_ne!(
            original.desired_configuration,
            new_route.desired_configuration
        );
        assert_eq!(original.test_definition, new_route.test_definition);
        changed.deployments[0].runtime.model_content = format!("sha256:{}", "b".repeat(64));
        let new_runtime =
            request_evidence(&changed, "model-a", "runner", &workload, &options()).unwrap();
        assert_ne!(original.observed_runtime, new_runtime.observed_runtime);
        let mut new_options = options();
        new_options["context_prompt"] = json!("different");
        let new_test =
            request_evidence(&route, "model-a", "runner", &workload, &new_options).unwrap();
        assert_ne!(original.test_definition, new_test.test_definition);
    }

    #[test]
    fn admission_rejects_ambiguous_or_missing_observation() {
        let (mut route, workload) = fixture();
        assert!(request_evidence(&route, "other", "runner", &workload, &options()).is_err());
        route.deployments[0].nodes.clear();
        assert!(request_evidence(&route, "model-a", "runner", &workload, &options()).is_err());
        route.deployments.push(route.deployments[0].clone());
        assert!(request_evidence(&route, "model-a", "runner", &workload, &options()).is_err());
    }

    #[test]
    fn target_config_comes_from_live_route_not_operator_options() {
        let (route, _) = fixture();
        let config = resolve_runner_config(
            &route,
            "model-a",
            &RunKind::Validation,
            "candidate",
            "svc",
            &options(),
        )
        .unwrap();
        assert_eq!(
            config["base_url"],
            "https://candidate.example/candidates/model-a/v1"
        );
        assert_eq!(config["expected_backend"], "svc/model-a/rev-a");
        let mut injected = options();
        injected["base_url"] = json!("https://unobserved.example/v1");
        assert!(
            resolve_runner_config(
                &route,
                "model-a",
                &RunKind::Validation,
                "candidate",
                "svc",
                &injected
            )
            .is_err()
        );
        let mut wrong = route.clone();
        wrong.weights.insert("model-a".into(), 0);
        assert!(
            resolve_runner_config(
                &wrong,
                "model-a",
                &RunKind::Validation,
                "candidate",
                "svc",
                &options()
            )
            .is_err()
        );
        wrong = route;
        wrong.listeners.push(RouteListener {
            hostname: "candidate.example".into(),
            port: 8443,
            protocol: "HTTPS".into(),
        });
        wrong.ports.push(8443);
        assert!(
            resolve_runner_config(
                &wrong,
                "model-a",
                &RunKind::Validation,
                "candidate",
                "svc",
                &options()
            )
            .is_err()
        );
    }
    #[test]
    fn agentic_protocols_require_an_independently_joined_topology() {
        let (mut route, _) = fixture();
        let mut requested = options();
        requested["responses"] = json!(true);
        requested["anthropic"] = json!(true);
        assert!(requires_agentic(&RunKind::Validation, &requested).unwrap());
        assert!(
            resolve_runner_config(
                &route,
                "model-a",
                &RunKind::Validation,
                "candidate",
                "svc",
                &requested
            )
            .is_err()
        );

        route.route = format!("agentic-cand-{}", "a".repeat(32));
        route.agentic = true;
        route.deployments[0].underlying_model = Some(crate::UnderlyingModelIdentity {
            rollout_configuration: route.deployments[0].rollout_configuration.clone(),
            runtime: route.deployments[0].runtime.clone(),
        });
        route.paths.extend([
            "/candidates/model-a/v1/responses".into(),
            "/candidates/model-a/v1/messages".into(),
        ]);
        let config = resolve_runner_config(
            &route,
            "model-a",
            &RunKind::Validation,
            "candidate",
            "svc",
            &requested,
        )
        .unwrap();
        assert_eq!(
            config["base_url"],
            "https://candidate.example/candidates/model-a/v1"
        );
        assert!(
            resolve_runner_config(
                &route,
                "model-a",
                &RunKind::Validation,
                "candidate",
                "svc",
                &options()
            )
            .is_err()
        );

        route.paths.retain(|path| !path.ends_with("/messages"));
        assert!(
            resolve_runner_config(
                &route,
                "model-a",
                &RunKind::Validation,
                "candidate",
                "svc",
                &requested
            )
            .is_err()
        );
        requested["responses"] = json!("true");
        assert!(requires_agentic(&RunKind::Validation, &requested).is_err());
    }
}
