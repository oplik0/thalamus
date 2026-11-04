//! Bind a prepared runner's endpoint to a separately observed route.
//!
//! Job configuration selects what the runner will request; it is not itself
//! evidence of the route or serving runtime. Callers must independently obtain
//! the snapshot and verify the prepared Job, its ownership and runner identity.

use k8s_openapi::api::{batch::v1::Job, core::v1::Container};
use serde_json::Value;
use thalamus_runs::{Run, RunKind, RunRequest};
use url::Url;

use crate::RouteSnapshot;

type BindingResult<T> = std::result::Result<T, &'static str>;

/// The operator-trusted route selected for the runner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteChoice {
    Candidate,
    Public,
}

/// Non-secret endpoint and backend assertions extracted from a runner Job.
#[derive(Debug)]
pub struct Target {
    pub choice: RouteChoice,
    urls: Vec<Url>,
    pub expected_backend: Option<String>,
    agentic: bool,
}

/// Extract the actual runner configuration, binding it to the persisted run
/// and to the operator-trusted route choice. This does not observe runtime state.
pub fn target_from_job(run: &Run, job: &Job, trusted_target: &str) -> Result<Target, &'static str> {
    let runner = runner(job)?;
    let mut run_id = [0; 36];
    let expected_id: &str = run.id.hyphenated().encode_lower(&mut run_id);
    if literal_env(runner, "THALAMUS_RUN_ID")? != expected_id {
        return Err("runner run ID does not match the run");
    }
    let request: RunRequest = serde_json::from_str(literal_env(runner, "THALAMUS_RUN_REQUEST")?)
        .map_err(|_| "invalid runner request")?;
    if request != run.request {
        return Err("runner request does not match the persisted run");
    }
    let name = match &run.request.kind {
        RunKind::Validation => "THALAMUS_VALIDATION_CONFIG",
        RunKind::Benchmark => "THALAMUS_BENCHMARK_CONFIG",
        RunKind::Debug => return Err("debug runs do not use runtime evidence"),
    };
    let config: Value = serde_json::from_str(literal_env(runner, name)?)
        .map_err(|_| "invalid runner configuration")?;
    target_from_config(
        &run.request.kind,
        &run.request.service,
        &config,
        trusted_target,
    )
}

/// Match all runner endpoints against one independently observed route and its
/// sole requested deployment. The caller must select the expected route from
/// trusted configuration using `target.choice`.
pub fn check_snapshot(
    run: &Run,
    target: &Target,
    snapshot: &RouteSnapshot,
    expected_route: &str,
) -> Result<(), &'static str> {
    check_observation(
        &run.request.service,
        &run.request.deployment,
        target,
        snapshot,
        expected_route,
    )
}

/// Match a trusted runner configuration to an independent route observation
/// before a run or Job exists. `service`, `deployment`, and `expected_route`
/// must come from the caller's trusted service and route selection.
pub fn check_observation(
    service: &str,
    deployment: &str,
    target: &Target,
    snapshot: &RouteSnapshot,
    expected_route: &str,
) -> Result<(), &'static str> {
    if snapshot.service != service || snapshot.route != expected_route {
        return Err("observed route identity does not match the run");
    }
    if snapshot.agentic != target.agentic {
        return Err("observed route topology does not match the runner protocols");
    }
    if snapshot.deployments.len() != 1 || snapshot.deployments[0].deployment != deployment {
        return Err("observed route does not have exactly the requested deployment");
    }
    if snapshot.agentic != snapshot.deployments[0].underlying_model.is_some() {
        return Err("observed route is missing its independently joined model chain");
    }
    if snapshot.weights.len() != 1
        || snapshot
            .weights
            .get(deployment)
            .is_none_or(|weight| *weight == 0)
    {
        return Err("observed route does not send traffic exclusively to the requested deployment");
    }
    if !target.urls.iter().all(|url| address_matches(url, snapshot)) {
        return Err("runner URL does not match the observed route");
    }
    if target
        .expected_backend
        .as_deref()
        .is_some_and(|expected| expected != snapshot.deployments[0].backend)
    {
        return Err("runner expected backend does not match the observed backend");
    }
    Ok(())
}

fn address_matches(url: &Url, snapshot: &RouteSnapshot) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let Some(port) = url.port_or_known_default() else {
        return false;
    };
    let protocol = match url.scheme() {
        "http" => "HTTP",
        "https" => "HTTPS",
        _ => return false,
    };
    snapshot.hostnames.iter().any(|name| name == host)
        && snapshot.paths.iter().any(|path| path == url.path())
        && snapshot.listeners.iter().any(|listener| {
            listener.hostname == host && listener.port == port && listener.protocol == protocol
        })
}

fn runner(job: &Job) -> BindingResult<&Container> {
    let containers = &job
        .spec
        .as_ref()
        .ok_or("Job spec is missing")?
        .template
        .spec
        .as_ref()
        .ok_or("Job pod spec is missing")?
        .containers;
    let mut matches = containers
        .iter()
        .filter(|container| container.name == "runner");
    let result = matches.next().ok_or("Job runner container is missing")?;
    if matches.next().is_some() {
        return Err("Job has multiple runner containers");
    }
    Ok(result)
}

fn literal_env<'a>(container: &'a Container, name: &str) -> BindingResult<&'a str> {
    let mut matches = container
        .env
        .iter()
        .flatten()
        .filter(|env| env.name == name);
    let entry = matches
        .next()
        .ok_or("required runner environment is missing")?;
    if matches.next().is_some() || entry.value_from.is_some() {
        return Err("runner environment must have one literal value");
    }
    entry
        .value
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or("required runner environment is empty")
}

/// Extract non-secret target assertions from trusted runner configuration. This
/// does not prove that a Job was created or that any route was observed.
pub fn target_from_config(
    kind: &RunKind,
    service: &str,
    config: &Value,
    trusted_target: &str,
) -> Result<Target, &'static str> {
    let object = config
        .as_object()
        .ok_or("runner configuration must be an object")?;
    if object.get("model").and_then(Value::as_str) != Some(service) {
        return Err("runner model does not match the observed service model");
    }
    let base = object
        .get("base_url")
        .and_then(Value::as_str)
        .ok_or("runner base URL is missing")?;
    let parsed = Url::parse(base).map_err(|_| "invalid runner base URL")?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || base.ends_with('/')
    {
        return Err("invalid runner base URL");
    }

    let configured_choice = match trusted_target {
        "candidate" => RouteChoice::Candidate,
        "public" => RouteChoice::Public,
        _ => return Err("invalid trusted runner route target"),
    };
    let mut paths = ["chat/completions", "", ""];
    let mut path_count = 1;
    let (choice, expected_backend) = match kind {
        RunKind::Validation => {
            let choice = match object.get("suite").and_then(Value::as_str) {
                Some("candidate") => RouteChoice::Candidate,
                Some("public") => RouteChoice::Public,
                _ => return Err("invalid validation suite"),
            };
            if choice != configured_choice {
                return Err("validation suite does not match trusted runner route target");
            }
            let header = optional_string(object.get("backend_header"))?;
            let expected = optional_string(object.get("expected_backend"))?;
            if !header.is_some_and(|name| name.eq_ignore_ascii_case("x-thalamus-backend"))
                || expected.is_none()
            {
                return Err("invalid validation backend assertion");
            }
            if choice == RouteChoice::Candidate {
                let old = optional_string(object.get("old_backend"))?;
                if old == expected {
                    return Err("invalid validation backend assertion");
                }
            }
            let responses = boolean(object.get("responses"))?;
            let continuation = boolean(object.get("responses_continuation"))?;
            if responses || continuation {
                paths[path_count] = "responses";
                path_count += 1;
            }
            if boolean(object.get("anthropic"))? {
                paths[path_count] = "messages";
                path_count += 1;
            }
            (choice, expected.map(str::to_owned))
        }
        RunKind::Benchmark => {
            // The pinned adapter strips the terminal /v1 before passing the
            // base to upstream, which sends /v1/completions.
            if configured_choice != RouteChoice::Candidate {
                return Err("benchmark runner must target the candidate route");
            }
            if !parsed.path().ends_with("/v1") {
                return Err("benchmark base URL must end in /v1");
            }
            paths[0] = "completions";
            (configured_choice, None)
        }
        RunKind::Debug => return Err("debug runs do not use runtime evidence"),
    };
    let mut urls = Vec::with_capacity(path_count);
    for &path in &paths[..path_count] {
        // Runners concatenate the path; Url::join can replace a base segment.
        urls.push(Url::parse(&format!("{base}/{path}")).map_err(|_| "invalid runner URL")?);
    }
    Ok(Target {
        choice,
        urls,
        expected_backend,
        agentic: crate::admission::requires_agentic(kind, config)
            .map_err(|_| "invalid runner protocol setting")?,
    })
}

fn optional_string(value: Option<&Value>) -> BindingResult<Option<&str>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(Some(value)),
        _ => Err("invalid runner backend assertion"),
    }
}

fn boolean(value: Option<&Value>) -> BindingResult<bool> {
    match value {
        None => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        _ => Err("invalid runner protocol setting"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use k8s_openapi::api::{
        batch::v1::JobSpec,
        core::v1::{EnvVar, PodSpec, PodTemplateSpec},
    };
    use serde_json::json;
    use thalamus_core::{DesiredIdentity, RuntimeIdentity};
    use thalamus_runs::{EvidenceIdentity, RunConditions, Workload};
    use uuid::Uuid;

    use crate::{DeploymentSnapshot, RouteListener};

    fn validation_config(suite: &str) -> Value {
        json!({
            "base_url": "https://example.test/candidates/deployment/v1",
            "model": "service",
            "suite": suite,
            "backend_header": "X-Thalamus-Backend",
            "expected_backend": "service/deployment/revision"
        })
    }

    fn benchmark_config() -> Value {
        json!({
            "base_url": "https://example.test/candidates/deployment/v1",
            "model": "service"
        })
    }

    fn run(kind: RunKind) -> Run {
        let now: DateTime<Utc> = DateTime::from_timestamp(0, 0).unwrap();
        let request = RunRequest {
            project_id: Uuid::from_u128(1),
            kind,
            service: "service".into(),
            deployment: "deployment".into(),
            runner: "runner".into(),
            workload: Workload {
                name: "functional".into(),
                seed: 1,
                concurrency: 1,
                duration_seconds: 60,
                warmup_seconds: 0,
                max_requests: 100,
                model_budget_tokens: 100,
                tool_budget_calls: 1,
            },
            // Caller-supplied claims must not substitute for a live snapshot.
            evidence: EvidenceIdentity {
                desired_configuration: "claimed-desired".into(),
                observed_runtime: "claimed-runtime".into(),
                test_definition: "claimed-test".into(),
                conditions: RunConditions {
                    hardware: "claimed-hardware".into(),
                    nodes: vec!["claimed-node".into()],
                    replicas: 1,
                    cache_state: "claimed-cache".into(),
                    competing_traffic: "claimed-traffic".into(),
                    observed_at: now,
                },
            },
        };
        let id = Uuid::from_u128(2);
        Run {
            id,
            team: "team".into(),
            actor: "actor".into(),
            idempotency_key: "key".into(),
            fingerprint: request.fingerprint().unwrap(),
            request,
            job_name: thalamus_runs::job_name(id),
            job_uid: None,
            ownership_nonce: Uuid::from_u128(3),
            job_spec: None,
            job_namespace: None,
            state: "pending".into(),
            artifacts: None,
            error: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn env(name: &str, value: String) -> EnvVar {
        EnvVar {
            name: name.into(),
            value: Some(value),
            ..Default::default()
        }
    }

    fn job(run: &Run, config: &Value) -> Job {
        let name = match &run.request.kind {
            RunKind::Benchmark => "THALAMUS_BENCHMARK_CONFIG",
            _ => "THALAMUS_VALIDATION_CONFIG",
        };
        let container = Container {
            name: "runner".into(),
            env: Some(vec![
                env("THALAMUS_RUN_ID", run.id.to_string()),
                env(
                    "THALAMUS_RUN_REQUEST",
                    serde_json::to_string(&run.request).unwrap(),
                ),
                env(name, config.to_string()),
            ]),
            ..Default::default()
        };
        Job {
            spec: Some(JobSpec {
                template: PodTemplateSpec {
                    spec: Some(PodSpec {
                        containers: vec![container],
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn runner_mut(job: &mut Job) -> &mut Container {
        &mut job
            .spec
            .as_mut()
            .unwrap()
            .template
            .spec
            .as_mut()
            .unwrap()
            .containers[0]
    }

    fn snapshot(path: &str) -> RouteSnapshot {
        RouteSnapshot {
            observed_at: DateTime::from_timestamp(0, 0).unwrap(),
            service: "service".into(),
            route: "deployment-candidate".into(),
            agentic: false,
            hostnames: vec!["example.test".into()],
            paths: vec![path.into()],
            ports: vec![443],
            listeners: vec![RouteListener {
                hostname: "example.test".into(),
                port: 443,
                protocol: "HTTPS".into(),
            }],
            weights: [("deployment".into(), 1)].into_iter().collect(),
            deployments: vec![DeploymentSnapshot {
                deployment: "deployment".into(),
                revision: "revision".into(),
                flux_revision: "main@sha1:revision".into(),
                backend: "service/deployment/revision".into(),
                desired: DesiredIdentity {
                    rendered_resources: "observed-desired".into(),
                    resolved_dependencies: "observed-dependencies".into(),
                },
                runtime: RuntimeIdentity {
                    image_digests: Default::default(),
                    model_content: "observed-model".into(),
                    tokenizer_content: "observed-tokenizer".into(),
                    effective_configuration: "observed-config".into(),
                    component_versions: Default::default(),
                },
                underlying_model: None,
                engine_spec: json!({}),
                rollout_configuration: format!("sha256:{}", "a".repeat(64)),
                nodes: vec!["observed-node".into()],
            }],
        }
    }

    #[test]
    fn validation_uses_the_actual_base_and_candidate_assertion() {
        let run = run(RunKind::Validation);
        let target = target_from_job(
            &run,
            &job(&run, &validation_config("candidate")),
            "candidate",
        )
        .unwrap();
        assert_eq!(target.choice, RouteChoice::Candidate);
        assert_eq!(
            target.expected_backend.as_deref(),
            Some("service/deployment/revision")
        );
        let route = snapshot("/candidates/deployment/v1/chat/completions");
        assert!(check_snapshot(&run, &target, &route, "deployment-candidate").is_ok());
        assert!(
            check_snapshot(
                &run,
                &target,
                &snapshot("/v1/chat/completions"),
                "deployment-candidate"
            )
            .is_err()
        );
    }

    #[test]
    fn benchmark_targets_completions_not_chat() {
        let run = run(RunKind::Benchmark);
        let target = target_from_job(&run, &job(&run, &benchmark_config()), "candidate").unwrap();
        assert_eq!(target.choice, RouteChoice::Candidate);
        assert_eq!(target.expected_backend, None);
        let route = snapshot("/candidates/deployment/v1/completions");
        assert!(check_snapshot(&run, &target, &route, "deployment-candidate").is_ok());
        assert!(
            check_snapshot(
                &run,
                &target,
                &snapshot("/candidates/deployment/v1/chat/completions"),
                "deployment-candidate"
            )
            .is_err()
        );
        let mut bad = benchmark_config();
        bad["base_url"] = json!("https://example.test/candidates/deployment/v2");
        assert!(target_from_job(&run, &job(&run, &bad), "candidate").is_err());
    }

    #[test]
    fn listener_tuple_must_match_host_port_and_scheme() {
        let url = Url::parse("https://example.test:8443/v1/chat/completions").unwrap();
        let mut route = snapshot("/v1/chat/completions");
        route.ports.push(8443);
        route.listeners.push(RouteListener {
            hostname: "other.test".into(),
            port: 8443,
            protocol: "HTTPS".into(),
        });
        assert!(!address_matches(&url, &route));
        route.listeners.push(RouteListener {
            hostname: "example.test".into(),
            port: 8443,
            protocol: "HTTP".into(),
        });
        assert!(!address_matches(&url, &route));
        route.listeners.push(RouteListener {
            hostname: "example.test".into(),
            port: 8443,
            protocol: "HTTPS".into(),
        });
        assert!(address_matches(&url, &route));
        route.hostnames.clear();
        assert!(!address_matches(&url, &route));
    }

    #[test]
    fn enabled_validation_protocols_all_need_observed_paths() {
        let run = run(RunKind::Validation);
        let mut config = validation_config("public");
        config["base_url"] = json!("https://example.test/v1");
        config["responses"] = json!(true);
        config["anthropic"] = json!(true);
        let target = target_from_job(&run, &job(&run, &config), "public").unwrap();
        assert_eq!(target.choice, RouteChoice::Public);
        let mut route = snapshot("/v1/chat/completions");
        route.route = "service-public".into();
        route.agentic = true;
        route.deployments[0].underlying_model = Some(crate::UnderlyingModelIdentity {
            rollout_configuration: route.deployments[0].rollout_configuration.clone(),
            runtime: route.deployments[0].runtime.clone(),
        });
        assert!(check_snapshot(&run, &target, &route, "service-public").is_err());
        route.paths.push("/v1/responses".into());
        assert!(check_snapshot(&run, &target, &route, "service-public").is_err());
        route.paths.push("/v1/messages".into());
        assert!(check_snapshot(&run, &target, &route, "service-public").is_ok());
        route.agentic = false;
        assert!(check_snapshot(&run, &target, &route, "service-public").is_err());
        route.agentic = true;

        config["responses"] = json!(false);
        config["anthropic"] = json!(false);
        config["responses_continuation"] = json!(true);
        let continuation = target_from_job(&run, &job(&run, &config), "public").unwrap();
        route.paths.retain(|path| !path.ends_with("/responses"));
        assert!(check_snapshot(&run, &continuation, &route, "service-public").is_err());
        config["responses_continuation"] = json!("true");
        assert!(target_from_job(&run, &job(&run, &config), "public").is_err());
    }

    #[test]
    fn rejects_credentials_untrusted_model_and_missing_assertions() {
        let benchmark = run(RunKind::Benchmark);
        for base in [
            "https://user:secret@example.test/v1",
            "https://example.test/v1?query=1",
            "https://example.test/v1#fragment",
            "https://example.test/v1/",
            "ftp://example.test/v1",
            "not a URL",
        ] {
            let mut config = benchmark_config();
            config["base_url"] = json!(base);
            assert!(
                target_from_job(&benchmark, &job(&benchmark, &config), "candidate").is_err(),
                "{base}"
            );
        }
        let mut config = benchmark_config();
        config["model"] = json!("other");
        assert!(target_from_job(&benchmark, &job(&benchmark, &config), "candidate").is_err());

        let validation = run(RunKind::Validation);
        let mut config = validation_config("candidate");
        config.as_object_mut().unwrap().remove("expected_backend");
        assert!(target_from_job(&validation, &job(&validation, &config), "candidate").is_err());
        config["expected_backend"] = json!("service/deployment/revision");
        config["backend_header"] = json!("x-untrusted-backend");
        assert!(target_from_job(&validation, &job(&validation, &config), "candidate").is_err());
        config["backend_header"] = json!(null);
        assert!(target_from_job(&validation, &job(&validation, &config), "candidate").is_err());
        config["backend_header"] = json!("x-thalamus-backend");
        config["expected_backend"] = json!("  ");
        assert!(target_from_job(&validation, &job(&validation, &config), "candidate").is_err());
    }

    #[test]
    fn validation_suites_require_independent_backend_assertions() {
        let run = run(RunKind::Validation);
        for suite in ["candidate", "public"] {
            let valid = validation_config(suite);
            assert!(target_from_config(&RunKind::Validation, "service", &valid, suite).is_ok());
            assert!(target_from_job(&run, &job(&run, &valid), suite).is_ok());
            for field in ["backend_header", "expected_backend"] {
                let mut missing = valid.clone();
                missing.as_object_mut().unwrap().remove(field);
                assert!(
                    target_from_config(&RunKind::Validation, "service", &missing, suite).is_err()
                );
                assert!(target_from_job(&run, &job(&run, &missing), suite).is_err());
                for bad in [json!(null), json!(""), json!("  "), json!(42)] {
                    let mut invalid = valid.clone();
                    invalid[field] = bad;
                    assert!(
                        target_from_config(&RunKind::Validation, "service", &invalid, suite)
                            .is_err()
                    );
                    assert!(target_from_job(&run, &job(&run, &invalid), suite).is_err());
                }
            }
            let mut missing_both = valid;
            missing_both
                .as_object_mut()
                .unwrap()
                .remove("backend_header");
            missing_both
                .as_object_mut()
                .unwrap()
                .remove("expected_backend");
            assert!(target_from_job(&run, &job(&run, &missing_both), suite).is_err());
        }
    }

    #[test]
    fn candidate_old_backend_is_optional_but_must_be_distinct_when_present() {
        let run = run(RunKind::Validation);
        let valid = validation_config("candidate");
        assert!(target_from_config(&RunKind::Validation, "service", &valid, "candidate").is_ok());
        assert!(target_from_job(&run, &job(&run, &valid), "candidate").is_ok());
        let mut null = valid.clone();
        null["old_backend"] = json!(null);
        assert!(target_from_config(&RunKind::Validation, "service", &null, "candidate").is_ok());
        assert!(target_from_job(&run, &job(&run, &null), "candidate").is_ok());
        let mut distinct = valid.clone();
        distinct["old_backend"] = json!("service/deployment/old-revision");
        assert!(
            target_from_config(&RunKind::Validation, "service", &distinct, "candidate").is_ok()
        );
        assert!(target_from_job(&run, &job(&run, &distinct), "candidate").is_ok());
        for bad in [
            json!(""),
            json!("  "),
            json!("service/deployment/revision"),
            json!(42),
        ] {
            let mut invalid = valid.clone();
            invalid["old_backend"] = bad;
            assert!(
                target_from_config(&RunKind::Validation, "service", &invalid, "candidate").is_err()
            );
            assert!(target_from_job(&run, &job(&run, &invalid), "candidate").is_err());
        }
    }

    #[test]
    fn public_validation_backend_must_match_the_observation() {
        let run = run(RunKind::Validation);
        let mut config = validation_config("public");
        config["base_url"] = json!("https://example.test/v1");
        let target = target_from_job(&run, &job(&run, &config), "public").unwrap();
        let mut route = snapshot("/v1/chat/completions");
        route.route = "service-public".into();
        assert!(check_snapshot(&run, &target, &route, "service-public").is_ok());
        route.deployments[0].backend = "service/deployment/other".into();
        assert!(check_snapshot(&run, &target, &route, "service-public").is_err());
    }

    #[test]
    fn route_target_comes_from_trusted_runner_configuration() {
        let validation = run(RunKind::Validation);
        let public = validation_config("public");
        let validation_job = job(&validation, &public);
        let target = target_from_job(&validation, &validation_job, "public").unwrap();
        assert_eq!(target.choice, RouteChoice::Public);
        assert_eq!(
            target.expected_backend.as_deref(),
            Some("service/deployment/revision")
        );
        assert!(target_from_job(&validation, &validation_job, "candidate").is_err());
        assert!(target_from_job(&validation, &validation_job, "unknown").is_err());

        let benchmark = run(RunKind::Benchmark);
        let job = job(&benchmark, &benchmark_config());
        assert!(target_from_job(&benchmark, &job, "candidate").is_ok());
        assert!(target_from_job(&benchmark, &job, "public").is_err());
        assert!(target_from_job(&benchmark, &job, "unknown").is_err());
    }

    #[test]
    fn job_runner_and_required_environment_are_unique_literal_and_bound_to_run() {
        let run = run(RunKind::Validation);
        let valid = job(&run, &validation_config("candidate"));
        assert!(target_from_job(&run, &valid, "candidate").is_ok());

        let mut missing = valid.clone();
        missing.spec = None;
        assert!(target_from_job(&run, &missing, "candidate").is_err());
        missing = valid.clone();
        missing.spec.as_mut().unwrap().template.spec = None;
        assert!(target_from_job(&run, &missing, "candidate").is_err());
        missing = valid.clone();
        runner_mut(&mut missing).name = "other".into();
        assert!(target_from_job(&run, &missing, "candidate").is_err());
        let mut duplicate = valid.clone();
        let pod = duplicate
            .spec
            .as_mut()
            .unwrap()
            .template
            .spec
            .as_mut()
            .unwrap();
        pod.containers.push(pod.containers[0].clone());
        assert!(target_from_job(&run, &duplicate, "candidate").is_err());

        for index in 0..3 {
            let mut missing = valid.clone();
            runner_mut(&mut missing).env.as_mut().unwrap().remove(index);
            assert!(target_from_job(&run, &missing, "candidate").is_err());
            let mut duplicate = valid.clone();
            let entries = runner_mut(&mut duplicate).env.as_mut().unwrap();
            entries.push(entries[index].clone());
            assert!(target_from_job(&run, &duplicate, "candidate").is_err());
            let mut reference = valid.clone();
            runner_mut(&mut reference).env.as_mut().unwrap()[index].value_from =
                Some(Default::default());
            assert!(target_from_job(&run, &reference, "candidate").is_err());
            let mut empty = valid.clone();
            runner_mut(&mut empty).env.as_mut().unwrap()[index].value = Some(String::new());
            assert!(target_from_job(&run, &empty, "candidate").is_err());
        }
        let mut wrong = valid.clone();
        runner_mut(&mut wrong).env.as_mut().unwrap()[0].value =
            Some(Uuid::from_u128(99).to_string());
        assert!(target_from_job(&run, &wrong, "candidate").is_err());
        wrong = valid.clone();
        let mut other_request = run.request.clone();
        other_request.evidence.test_definition = "other-test".into();
        runner_mut(&mut wrong).env.as_mut().unwrap()[1].value =
            Some(serde_json::to_string(&other_request).unwrap());
        assert!(target_from_job(&run, &wrong, "candidate").is_err());
        wrong = valid.clone();
        runner_mut(&mut wrong).env.as_mut().unwrap()[1].value = Some("not json".into());
        assert!(target_from_job(&run, &wrong, "candidate").is_err());
        wrong = valid.clone();
        runner_mut(&mut wrong).env.as_mut().unwrap()[2].value = Some("not json".into());
        assert!(target_from_job(&run, &wrong, "candidate").is_err());
        let debug = self::run(RunKind::Debug);
        assert!(
            target_from_job(
                &debug,
                &job(&debug, &validation_config("candidate")),
                "candidate"
            )
            .is_err()
        );
    }

    #[test]
    fn snapshot_must_have_the_exact_service_route_deployment_and_backend() {
        let run = run(RunKind::Validation);
        let target = target_from_job(
            &run,
            &job(&run, &validation_config("candidate")),
            "candidate",
        )
        .unwrap();
        let observed = snapshot("/candidates/deployment/v1/chat/completions");
        assert!(check_snapshot(&run, &target, &observed, "deployment-candidate").is_ok());
        assert!(check_snapshot(&run, &target, &observed, "other-route").is_err());
        let mut wrong = observed.clone();
        wrong.service = "other-service".into();
        assert!(check_snapshot(&run, &target, &wrong, "deployment-candidate").is_err());
        wrong = observed.clone();
        wrong.deployments.clear();
        assert!(check_snapshot(&run, &target, &wrong, "deployment-candidate").is_err());
        wrong = observed.clone();
        wrong.deployments.push(wrong.deployments[0].clone());
        assert!(check_snapshot(&run, &target, &wrong, "deployment-candidate").is_err());
        wrong = observed.clone();
        wrong.deployments[0].deployment = "other".into();
        assert!(check_snapshot(&run, &target, &wrong, "deployment-candidate").is_err());
        wrong = observed;
        wrong.deployments[0].backend = "other-backend".into();
        assert!(check_snapshot(&run, &target, &wrong, "deployment-candidate").is_err());
    }

    #[test]
    fn observation_requires_one_positive_weight_for_the_requested_deployment() {
        for (kind, suite) in [
            (RunKind::Validation, "candidate"),
            (RunKind::Validation, "public"),
            (RunKind::Benchmark, "candidate"),
        ] {
            let mut config = match &kind {
                RunKind::Validation => validation_config(suite),
                RunKind::Benchmark => benchmark_config(),
                RunKind::Debug => unreachable!(),
            };
            if suite == "public" {
                config["base_url"] = json!("https://example.test/v1");
            }
            let run = run(kind);
            let target = target_from_job(&run, &job(&run, &config), suite).unwrap();
            let path = if suite == "public" {
                "/v1/chat/completions"
            } else if run.request.kind == RunKind::Benchmark {
                "/candidates/deployment/v1/completions"
            } else {
                "/candidates/deployment/v1/chat/completions"
            };
            let expected_route = if suite == "public" {
                "service-public"
            } else {
                "deployment-candidate"
            };
            let mut route = snapshot(path);
            route.route = expected_route.into();
            route.weights.insert("deployment".into(), 100);
            assert!(check_snapshot(&run, &target, &route, expected_route).is_ok());
            assert!(
                check_observation("service", "deployment", &target, &route, expected_route).is_ok()
            );

            for weights in [
                vec![],
                vec![("deployment", 0)],
                vec![("other", 1)],
                vec![("deployment", 1), ("other", 0)],
                vec![("deployment", 1), ("other", 1)],
            ] {
                route.weights = weights
                    .into_iter()
                    .map(|(name, weight)| (name.to_owned(), weight))
                    .collect();
                assert!(check_snapshot(&run, &target, &route, expected_route).is_err());
                assert!(
                    check_observation("service", "deployment", &target, &route, expected_route)
                        .is_err()
                );
            }
        }
    }

    #[test]
    fn admission_and_job_use_the_same_endpoint_and_observation_matcher() {
        for kind in [RunKind::Validation, RunKind::Benchmark] {
            let config = match &kind {
                RunKind::Validation => validation_config("candidate"),
                RunKind::Benchmark => benchmark_config(),
                RunKind::Debug => unreachable!(),
            };
            let run = run(kind);
            let from_config =
                target_from_config(&run.request.kind, "service", &config, "candidate").unwrap();
            let from_job = target_from_job(&run, &job(&run, &config), "candidate").unwrap();
            assert_eq!(from_config.choice, from_job.choice);
            assert_eq!(from_config.expected_backend, from_job.expected_backend);
            assert_eq!(from_config.urls, from_job.urls);
            let path = match &run.request.kind {
                RunKind::Validation => "/candidates/deployment/v1/chat/completions",
                RunKind::Benchmark => "/candidates/deployment/v1/completions",
                RunKind::Debug => unreachable!(),
            };
            let mut route = snapshot(path);
            assert_eq!(
                check_observation(
                    "service",
                    "deployment",
                    &from_config,
                    &route,
                    "deployment-candidate"
                ),
                check_snapshot(&run, &from_job, &route, "deployment-candidate")
            );
            assert!(
                check_observation(
                    "service",
                    "deployment",
                    &from_config,
                    &route,
                    "deployment-candidate"
                )
                .is_ok()
            );
            route.deployments.clear();
            assert_eq!(
                check_observation(
                    "service",
                    "deployment",
                    &from_config,
                    &route,
                    "deployment-candidate"
                ),
                check_snapshot(&run, &from_job, &route, "deployment-candidate")
            );
            assert!(
                check_observation(
                    "service",
                    "deployment",
                    &from_config,
                    &route,
                    "deployment-candidate"
                )
                .is_err()
            );
        }
    }
}
