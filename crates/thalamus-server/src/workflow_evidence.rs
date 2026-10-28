//! Bind an approved run plan to a fresh, independently observed route.
//!
//! The profile and the runner configuration select what to run; neither is an
//! observation. The worker independently checks its actual Job and reobserves
//! the route before launch and at ingestion.
use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thalamus_runs::{RunKind, RunRequest, Workload};
use thalamus_runtime::{
    admission::{
        request_evidence, requires_agentic, resolve_runner_config, validate_profiles,
        validate_test_workload, RunnerProfiles, TestProfile,
    },
    binding::RouteChoice,
    RouteSnapshot, RuntimeConfig, ServiceRoutes,
};
use uuid::Uuid;

use crate::{
    run_promotion::flux_commit,
    workflows::{evidence_fresh, unavailable, valid_name},
    ApiError, AppState,
};

/// Operator-approved run inputs. In particular, profiles cannot carry evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProfileRequest {
    pub(crate) project_id: Uuid,
    pub(crate) kind: RunKind,
    pub(crate) service: String,
    pub(crate) runner: String,
    pub(crate) workload: Workload,
}

// The same operator configuration is supplied to the server and worker. Only
// the worker actually executes the image and command; they are not observations.
#[derive(Deserialize)]
struct RunnerConfig {
    kind: RunKind,
    #[serde(rename = "image")]
    _image: String,
    #[serde(rename = "command")]
    _command: Vec<String>,
    workloads: Vec<String>,
    #[serde(rename = "service_account")]
    _service_account: String,
    key_secret: Option<String>,
    route_target: String,
}

fn runner_target(json: &str, profile: &ProfileRequest) -> Result<String, ApiError> {
    let mut runners: BTreeMap<String, RunnerConfig> = serde_json::from_str(json)
        .map_err(|_| unavailable("trusted runner configuration is invalid"))?;
    let runner = runners
        .remove(&profile.runner)
        .ok_or_else(|| unavailable("approved runner is not configured"))?;
    if runner.kind != profile.kind
        || !runner.workloads.contains(&profile.workload.name)
        || !matches!(runner.route_target.as_str(), "candidate" | "public")
        || (profile.kind == RunKind::Benchmark && runner.route_target != "candidate")
        || matches!(profile.kind, RunKind::Debug)
        || runner.key_secret.as_deref().is_none_or(str::is_empty)
        || profile.workload.model_budget_tokens == 0
    {
        return Err(unavailable("approved runner or workload is not supported"));
    }
    Ok(runner.route_target)
}

fn approved_test_profile<'a>(
    profiles: &'a RunnerProfiles,
    profile: &ProfileRequest,
    route_target: &str,
) -> Result<&'a TestProfile, ApiError> {
    validate_profiles(profiles)
        .map_err(|_| unavailable("trusted runner test profiles are invalid"))?;
    let selected = profiles
        .get(&profile.service)
        .and_then(|runners| runners.get(&profile.runner))
        .ok_or_else(|| unavailable("approved runner test profile is unavailable"))?;
    if selected.kind != profile.kind || selected.route_target != route_target {
        return Err(unavailable("approved runner and test profile do not match"));
    }
    validate_test_workload(selected, &profile.workload)
        .map_err(|_| unavailable("approved test options exceed workload limits"))?;
    Ok(selected)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ObservationRoute {
    Candidate,
    AgenticCandidate,
    Benchmark,
    Public,
    AgenticPublic,
}

/// Choose the observer from approved test options, never from a caller URL.
fn observation_route(
    kind: &RunKind,
    choice: RouteChoice,
    options: &Value,
) -> Result<ObservationRoute, ApiError> {
    let agentic = requires_agentic(kind, options)
        .map_err(|_| unavailable("approved runner test options are invalid"))?;
    match (kind, choice, agentic) {
        (RunKind::Validation, RouteChoice::Candidate, false) => Ok(ObservationRoute::Candidate),
        (RunKind::Validation, RouteChoice::Candidate, true) => {
            Ok(ObservationRoute::AgenticCandidate)
        }
        (RunKind::Validation, RouteChoice::Public, false) => Ok(ObservationRoute::Public),
        (RunKind::Validation, RouteChoice::Public, true) => Ok(ObservationRoute::AgenticPublic),
        (RunKind::Benchmark, RouteChoice::Candidate, false) => Ok(ObservationRoute::Benchmark),
        _ => Err(unavailable("approved runner route is unsupported")),
    }
}

/// Check the service binding only after the caller authorizes the deployment.
/// Configuration selects ownership and a public route; it is not an observation.
fn configured_binding<'a>(
    config: &'a RuntimeConfig,
    team: Uuid,
    profile: &ProfileRequest,
    deployment: &str,
    choice: RouteChoice,
) -> Result<&'a ServiceRoutes, ApiError> {
    let binding = config
        .routes
        .get(&profile.service)
        .ok_or_else(|| unavailable("service has no runtime route configuration"))?;
    if team.is_nil()
        || profile.project_id.is_nil()
        || binding.team_id != team
        || binding.project_id != profile.project_id
        || binding.service.name != profile.service
        || binding.public.is_empty()
        || !valid_name(deployment)
    {
        return Err(unavailable(
            "run does not match configured service ownership",
        ));
    }
    if binding.public.strip_suffix("-candidate") == Some(deployment)
        || binding.public.strip_suffix("-benchmark") == Some(deployment)
    {
        return Err(unavailable("public and candidate routes must be distinct"));
    }
    match (choice, &profile.kind) {
        (RouteChoice::Public, RunKind::Validation)
        | (RouteChoice::Candidate, RunKind::Validation)
        | (RouteChoice::Candidate, RunKind::Benchmark) => Ok(binding),
        _ => Err(unavailable("approved runner route is unsupported")),
    }
}

/// A rollout stage authorizes a particular deployment revision and merged
/// catalog commit. Verify both against the fresh observation, never against the
/// profile or caller. Transactional stage/run binding remains the caller's job.
fn check_rollout_candidate(
    snapshot: &RouteSnapshot,
    authorized: Option<(&str, &str)>,
) -> Result<(), ApiError> {
    let Some((revision, catalog_commit)) = authorized else {
        return Ok(());
    };
    let [observed] = snapshot.deployments.as_slice() else {
        return Err(unavailable("rollout candidate observation is ambiguous"));
    };
    if revision.is_empty()
        || observed.revision != revision
        || flux_commit(&observed.flux_revision) != Some(catalog_commit)
    {
        return Err(unavailable(
            "observed rollout candidate revision or catalog commit does not match",
        ));
    }
    Ok(())
}

struct AdmissionTarget<'a> {
    deployment: &'a str,
    route_target: &'a str,
    public_route: &'a str,
}

/// Convert only a checked snapshot to a persistable request. This separate pure
/// step also makes it possible to test the fail-closed boundary without K8s.
fn from_observation(
    profile: ProfileRequest,
    target: AdmissionTarget<'_>,
    snapshot: &RouteSnapshot,
    now: DateTime<Utc>,
    max_age_seconds: i64,
    options: &Value,
) -> Result<RunRequest, ApiError> {
    if !evidence_fresh(snapshot.observed_at, now, max_age_seconds) {
        return Err(unavailable(
            "runtime observation is stale or has a future timestamp",
        ));
    }
    if snapshot.service != profile.service {
        return Err(unavailable(
            "observed route does not belong to the requested service",
        ));
    }
    resolve_runner_config(
        snapshot,
        target.deployment,
        &profile.kind,
        target.route_target,
        target.public_route,
        options,
    )
    .map_err(|_| unavailable("approved test options do not match the observed route"))?;
    let evidence = request_evidence(
        snapshot,
        target.deployment,
        &profile.runner,
        &profile.workload,
        options,
    )
    .map_err(|_| unavailable("observed runtime evidence is incomplete"))?;
    let request = RunRequest {
        project_id: profile.project_id,
        kind: profile.kind,
        service: profile.service,
        deployment: target.deployment.to_owned(),
        runner: profile.runner,
        workload: profile.workload,
        evidence,
    };
    request
        .validate()
        .map_err(|_| unavailable("approved run profile is invalid"))?;
    Ok(request)
}

/// Observe the exact operator-selected route at submission. Profile data supplies
/// the plan only; every serving identity and observation time comes from the
/// configured live observer. Fail closed if the observer or configuration is
/// unavailable, or if the runner and observed route do not match.
pub(crate) async fn bind_evidence(
    state: &AppState,
    team: Uuid,
    profile: ProfileRequest,
    deployment: &str,
    max_evidence_age_seconds: i64,
    authorized_candidate: Option<(&str, &str)>,
) -> Result<RunRequest, ApiError> {
    let config = state
        .config
        .runtime
        .as_ref()
        .ok_or_else(|| unavailable("runtime observation is not configured"))?;
    let observer = state
        .runtime_observer
        .as_ref()
        .ok_or_else(|| unavailable("runtime observation is unavailable"))?;
    if observer.config() != config {
        return Err(unavailable("runtime observer configuration does not match"));
    }
    let runners = std::env::var("THALAMUS_RUNNERS_JSON")
        .map_err(|_| unavailable("trusted runner configuration is unavailable"))?;
    let route_target = runner_target(&runners, &profile)?;
    let runner_configs = std::env::var("THALAMUS_VALIDATION_CONFIGS_JSON")
        .map_err(|_| unavailable("trusted runner configuration is unavailable"))?;
    let profiles: RunnerProfiles = serde_json::from_str(&runner_configs)
        .map_err(|_| unavailable("trusted runner configuration is invalid"))?;
    let test_profile = approved_test_profile(&profiles, &profile, &route_target)?;
    let choice = match route_target.as_str() {
        "candidate" => RouteChoice::Candidate,
        "public" => RouteChoice::Public,
        _ => return Err(unavailable("approved runner route is unsupported")),
    };
    let binding = configured_binding(config, team, &profile, deployment, choice)?;
    // The trusted options choose the direct or full agentic route. Both methods
    // derive their resource names internally after service/deployment authorization.
    let snapshot = match observation_route(&profile.kind, choice, &test_profile.options)? {
        ObservationRoute::Candidate => {
            observer
                .observe_candidate(&profile.service, deployment)
                .await
        }
        ObservationRoute::AgenticCandidate => {
            observer
                .observe_agentic_candidate(&profile.service, deployment)
                .await
        }
        ObservationRoute::Benchmark => {
            observer
                .observe_benchmark(&profile.service, deployment)
                .await
        }
        ObservationRoute::Public => {
            observer
                .observe_route(&profile.service, &binding.public)
                .await
        }
        ObservationRoute::AgenticPublic => observer.observe_agentic_public(&profile.service).await,
    }
    .map_err(|_| unavailable("live runtime observation is unavailable or ambiguous"))?;
    check_rollout_candidate(&snapshot, authorized_candidate)?;
    from_observation(
        profile,
        AdmissionTarget {
            deployment,
            route_target: &route_target,
            public_route: &binding.public,
        },
        &snapshot,
        Utc::now(),
        max_evidence_age_seconds,
        &test_profile.options,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::collections::BTreeSet;
    use thalamus_core::{DesiredIdentity, PublishedService, RuntimeIdentity};
    use thalamus_runtime::{
        DeploymentSnapshot, RouteListener, ServiceRoutes, UnderlyingModelIdentity,
    };

    fn profile() -> ProfileRequest {
        ProfileRequest {
            project_id: Uuid::from_u128(2),
            kind: RunKind::Validation,
            service: "svc".into(),
            runner: "runner".into(),
            workload: Workload {
                name: "functional".into(),
                seed: 1,
                concurrency: 1,
                duration_seconds: 30,
                warmup_seconds: 0,
                max_requests: 20,
                model_budget_tokens: 4096,
                tool_budget_calls: 1,
            },
        }
    }

    fn options() -> Value {
        json!({"forbidden_model": "other", "context_prompt": "hello", "context_tokens": 2,
            "reasoning": false, "responses_continuation": false, "timeout_seconds": 30})
    }

    fn benchmark_options() -> Value {
        json!({
            "tokenizer": {"repository": "org/tokenizer", "revision": "0123456789abcdef0123456789abcdef01234567"},
            "workloads": {"functional": {
                "load": {"type": "constant", "stages": [{"rate": 1, "duration": 10}]},
                "data": {"type": "random", "input_len": 16, "output_len": 8},
                "goodput": {"ttft": 1.0, "itl": 0.1}
            }}
        })
    }

    #[test]
    fn protocol_options_select_full_agentic_observation() {
        let direct = options();
        assert_eq!(
            observation_route(&RunKind::Validation, RouteChoice::Candidate, &direct).unwrap(),
            ObservationRoute::Candidate
        );
        assert_eq!(
            observation_route(&RunKind::Validation, RouteChoice::Public, &direct).unwrap(),
            ObservationRoute::Public
        );
        for field in ["responses", "responses_continuation", "anthropic"] {
            let mut agentic = options();
            agentic[field] = json!(true);
            assert_eq!(
                observation_route(&RunKind::Validation, RouteChoice::Candidate, &agentic).unwrap(),
                ObservationRoute::AgenticCandidate
            );
            assert_eq!(
                observation_route(&RunKind::Validation, RouteChoice::Public, &agentic).unwrap(),
                ObservationRoute::AgenticPublic
            );
        }
        let benchmark = benchmark_options();
        assert_eq!(
            observation_route(&RunKind::Benchmark, RouteChoice::Candidate, &benchmark).unwrap(),
            ObservationRoute::Benchmark
        );
        assert!(observation_route(&RunKind::Benchmark, RouteChoice::Public, &benchmark).is_err());
        let mut invalid = options();
        invalid["responses"] = json!("true");
        assert!(observation_route(&RunKind::Validation, RouteChoice::Candidate, &invalid).is_err());
    }

    fn target<'a>(deployment: &'a str, route_target: &'a str) -> AdmissionTarget<'a> {
        AdmissionTarget {
            deployment,
            route_target,
            public_route: "svc-public",
        }
    }

    fn config() -> RuntimeConfig {
        RuntimeConfig {
            namespace: "inference".into(),
            route_version: "v1".into(),
            backend_version: "v1".into(),
            policy_version: "v1".into(),
            pool_version: "v1".into(),
            serving_version: "v1".into(),
            source_version: "v1".into(),
            flux_version: "v1".into(),
            helm_version: "v1".into(),
            trusted_agentic_images: vec![],
            routes: BTreeMap::from([(
                "svc".into(),
                ServiceRoutes {
                    public: "svc-public".into(),
                    team_id: Uuid::from_u128(1),
                    project_id: Uuid::from_u128(2),
                    service: PublishedService {
                        name: "svc".into(),
                        protocols: BTreeSet::new(),
                        deployments: vec![],
                        access_policy: "policy".into(),
                        owners: vec![],
                    },
                },
            )]),
        }
    }

    fn snapshot(at: DateTime<Utc>) -> RouteSnapshot {
        let sha = format!("sha256:{}", "a".repeat(64));
        RouteSnapshot {
            observed_at: at,
            agentic: false,
            service: "svc".into(),
            route: "model-a-candidate".into(),
            hostnames: vec!["candidate.example".into()],
            paths: vec!["/v1/chat/completions".into()],
            ports: vec![443],
            listeners: vec![RouteListener {
                hostname: "candidate.example".into(),
                port: 443,
                protocol: "HTTPS".into(),
            }],
            weights: BTreeMap::from([("model-a".into(), 1)]),
            deployments: vec![DeploymentSnapshot {
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
                    image_digests: BTreeMap::from([("main".into(), sha.clone())]),
                    model_content: sha.clone(),
                    tokenizer_content: sha.clone(),
                    effective_configuration: sha.clone(),
                    component_versions: BTreeMap::from([("engine".into(), sha)]),
                },
                underlying_model: None,
                engine_spec: json!({"image": "registry.example/model", "args": ["--served-model-name", "svc"]}),
                nodes: vec!["node-a".into(), "node-a".into()],
            }],
        }
    }

    #[test]
    fn agentic_conversion_binds_actual_candidate_and_public_routes() {
        let now = Utc::now();
        let mut options = options();
        options["responses"] = json!(true);
        options["anthropic"] = json!(true);

        let mut candidate = snapshot(now);
        candidate.agentic = true;
        let name_hash = format!("{:x}", Sha256::digest(b"svc/team/model-a"));
        candidate.route = format!("agentic-cand-{}", &name_hash[..32]);
        candidate.hostnames = vec!["agentic.example".into()];
        candidate.listeners[0].hostname = "agentic.example".into();
        candidate.paths = vec![
            "/candidates/model-a/v1/chat/completions".into(),
            "/candidates/model-a/v1/responses".into(),
            "/candidates/model-a/v1/messages".into(),
        ];
        let underlying = UnderlyingModelIdentity {
            rollout_configuration: candidate.deployments[0].rollout_configuration.clone(),
            runtime: candidate.deployments[0].runtime.clone(),
        };
        candidate.deployments[0].underlying_model = Some(underlying);
        candidate.deployments[0].runtime.image_digests.insert(
            "agentic/router".into(),
            format!("sha256:{}", "b".repeat(64)),
        );
        let request = from_observation(
            profile(),
            target("model-a", "candidate"),
            &candidate,
            now,
            60,
            &options,
        )
        .unwrap();
        assert_eq!(
            request.evidence,
            request_evidence(
                &candidate,
                "model-a",
                "runner",
                &profile().workload,
                &options
            )
            .unwrap()
        );

        let mut public = candidate.clone();
        public.route = "svc-team".into();
        public.paths = vec![
            "/v1/chat/completions".into(),
            "/v1/responses".into(),
            "/v1/messages".into(),
        ];
        assert!(from_observation(
            profile(),
            target("model-a", "public"),
            &public,
            now,
            60,
            &options,
        )
        .is_ok());
        public.paths.retain(|path| path != "/v1/messages");
        assert!(from_observation(
            profile(),
            target("model-a", "public"),
            &public,
            now,
            60,
            &options,
        )
        .is_err());
        candidate.agentic = false;
        assert!(from_observation(
            profile(),
            target("model-a", "candidate"),
            &candidate,
            now,
            60,
            &options,
        )
        .is_err());
    }

    #[test]
    fn profile_contains_only_approved_plan() {
        let value = serde_json::to_value(profile()).unwrap();
        assert_eq!(
            serde_json::from_value::<ProfileRequest>(value.clone()).unwrap(),
            profile()
        );
        let mut invalid = value;
        invalid["evidence"] = json!({"observed_runtime": "forged"});
        assert!(serde_json::from_value::<ProfileRequest>(invalid).is_err());
        let mut invalid = serde_json::to_value(profile()).unwrap();
        invalid["deployment"] = json!("forged-deployment");
        assert!(serde_json::from_value::<ProfileRequest>(invalid).is_err());
    }

    #[test]
    fn runner_target_requires_matching_approved_kind_workload_and_route() {
        let runner = json!({"runner": {"kind": "validation", "image": "registry/runner@sha256:abc",
            "command": ["runner"], "workloads": ["functional"], "service_account": "runner",
            "key_secret": "run-key", "route_target": "candidate"}});
        assert_eq!(
            runner_target(&runner.to_string(), &profile()).unwrap(),
            "candidate"
        );
        let mut p = profile();
        p.kind = RunKind::Benchmark;
        assert!(runner_target(&runner.to_string(), &p).is_err());
        p = profile();
        p.workload.name = "not-approved".into();
        assert!(runner_target(&runner.to_string(), &p).is_err());
        p = profile();
        p.workload.model_budget_tokens = 0;
        assert!(runner_target(&runner.to_string(), &p).is_err());
        let mut bad = runner;
        bad["runner"]["route_target"] = json!("unknown");
        assert!(runner_target(&bad.to_string(), &profile()).is_err());
    }

    #[test]
    fn service_profiles_bind_only_live_endpoint_and_backend() {
        let input = json!({"svc": {"runner": {
            "kind": "validation", "route_target": "candidate",
            "options": {"forbidden_model": "other", "context_prompt": "hello", "context_tokens": 2,
                "reasoning": false, "responses_continuation": false, "timeout_seconds": 30}
        }}});
        let profiles: RunnerProfiles = serde_json::from_value(input.clone()).unwrap();
        let selected = approved_test_profile(&profiles, &profile(), "candidate").unwrap();
        assert!(approved_test_profile(&profiles, &profile(), "public").is_err());
        let mut insufficient = profile();
        insufficient.workload.model_budget_tokens = 1;
        assert!(approved_test_profile(&profiles, &insufficient, "candidate").is_err());
        let now = Utc::now();
        let observed = snapshot(now);
        let resolved = resolve_runner_config(
            &observed,
            "model-a",
            &RunKind::Validation,
            "candidate",
            "svc-public",
            &selected.options,
        )
        .unwrap();
        assert_eq!(resolved["base_url"], "https://candidate.example/v1");
        assert_eq!(resolved["model"], "svc");
        assert_eq!(resolved["expected_backend"], "svc/model-a/rev-a");
        let run = from_observation(
            profile(),
            target("model-a", "candidate"),
            &observed,
            now,
            60,
            &selected.options,
        )
        .unwrap();
        assert_eq!(run.deployment, "model-a");

        // No new operator configuration is needed for an authorized future
        // deployment; endpoint and backend still come only from its snapshot.
        let mut future = observed.clone();
        future.route = "next-release-candidate".into();
        future.weights = BTreeMap::from([("next-release".into(), 1)]);
        future.deployments[0].deployment = "next-release".into();
        future.deployments[0].revision = "rev-b".into();
        future.deployments[0].backend = "svc/next-release/rev-b".into();
        let next_config = resolve_runner_config(
            &future,
            "next-release",
            &RunKind::Validation,
            "candidate",
            "svc-public",
            &selected.options,
        )
        .unwrap();
        assert_eq!(next_config["expected_backend"], "svc/next-release/rev-b");
        assert_eq!(
            from_observation(
                profile(),
                target("next-release", "candidate"),
                &future,
                now,
                60,
                &selected.options,
            )
            .unwrap()
            .deployment,
            "next-release"
        );

        let mut forged = input;
        forged["svc"]["runner"]["options"]["base_url"] = json!("https://forged.example/v1");
        let forged: RunnerProfiles = serde_json::from_value(forged).unwrap();
        assert!(approved_test_profile(&forged, &profile(), "candidate").is_err());
    }
    #[test]
    fn service_binding_checks_ownership_and_supports_future_deployments() {
        let team = Uuid::from_u128(1);
        let mut routes = config();
        let p = profile();
        assert!(configured_binding(&routes, team, &p, "model-a", RouteChoice::Candidate).is_ok());
        assert_eq!(
            configured_binding(&routes, team, &p, "model-a", RouteChoice::Public)
                .unwrap()
                .public,
            "svc-public"
        );
        assert!(
            configured_binding(&routes, team, &p, "new-release", RouteChoice::Candidate).is_ok()
        );
        let mut benchmark = p.clone();
        benchmark.kind = RunKind::Benchmark;
        assert!(
            configured_binding(&routes, team, &benchmark, "model-a", RouteChoice::Candidate)
                .is_ok()
        );
        assert!(
            configured_binding(&routes, team, &benchmark, "model-a", RouteChoice::Public).is_err()
        );
        assert!(configured_binding(
            &routes,
            Uuid::from_u128(3),
            &p,
            "model-a",
            RouteChoice::Candidate
        )
        .is_err());
        let mut wrong_project = p.clone();
        wrong_project.project_id = Uuid::from_u128(3);
        assert!(configured_binding(
            &routes,
            team,
            &wrong_project,
            "model-a",
            RouteChoice::Candidate
        )
        .is_err());
        assert!(configured_binding(&routes, team, &p, "../model", RouteChoice::Candidate).is_err());
        routes.routes.get_mut("svc").unwrap().public = "model-a-candidate".into();
        assert!(configured_binding(&routes, team, &p, "model-a", RouteChoice::Public).is_err());
    }

    #[test]
    fn conversion_uses_only_independently_observed_identity() {
        let now = Utc::now();
        let observed = snapshot(now);
        let plan = profile();
        let expected = request_evidence(
            &observed,
            "model-a",
            &plan.runner,
            &plan.workload,
            &options(),
        )
        .unwrap();
        let request = from_observation(
            plan.clone(),
            target("model-a", "candidate"),
            &observed,
            now,
            60,
            &options(),
        )
        .unwrap();
        assert_eq!(request.evidence, expected);
        assert_eq!(request.evidence.conditions.observed_at, now);
        assert_eq!(request.evidence.conditions.nodes, vec!["node-a", "node-a"]);
        assert_eq!(request.evidence.conditions.replicas, 2);
        assert_eq!(request.evidence.conditions.hardware, "not observed");
        assert_eq!(request.workload, plan.workload);
        assert_eq!(request.runner, plan.runner);
    }

    #[test]
    fn runtime_identity_changes_when_live_configuration_changes() {
        let now = Utc::now();
        let mut observed = snapshot(now);
        let original = from_observation(
            profile(),
            target("model-a", "candidate"),
            &observed,
            now,
            60,
            &options(),
        )
        .unwrap();
        observed.deployments[0].runtime.effective_configuration =
            format!("sha256:{}", "b".repeat(64));
        let changed = from_observation(
            profile(),
            target("model-a", "candidate"),
            &observed,
            now,
            60,
            &options(),
        )
        .unwrap();
        assert_ne!(
            original.evidence.observed_runtime,
            changed.evidence.observed_runtime
        );
        assert_eq!(
            original.evidence.desired_configuration,
            changed.evidence.desired_configuration
        );
        assert_eq!(
            original.evidence.test_definition,
            changed.evidence.test_definition
        );
        let mut revised_options = options();
        revised_options["context_prompt"] = json!("a different prompt");
        let revised = from_observation(
            profile(),
            target("model-a", "candidate"),
            &observed,
            now,
            60,
            &revised_options,
        )
        .unwrap();
        assert_eq!(
            changed.evidence.observed_runtime,
            revised.evidence.observed_runtime
        );
        assert_ne!(
            changed.evidence.test_definition,
            revised.evidence.test_definition
        );
    }

    #[test]
    fn public_validation_and_benchmark_use_their_actual_routes() {
        let now = Utc::now();
        let mut public = snapshot(now);
        public.route = "svc-public".into();
        public.hostnames = vec!["public.example".into()];
        public.listeners[0].hostname = "public.example".into();
        assert!(from_observation(
            profile(),
            target("model-a", "public"),
            &public,
            now,
            60,
            &options()
        )
        .is_ok());

        let mut benchmark = profile();
        benchmark.kind = RunKind::Benchmark;
        let mut observed = snapshot(now);
        observed.route = "model-a-benchmark".into();
        observed.paths = vec!["/v1/completions".into()];
        let benchmark_options = benchmark_options();
        let profiles: RunnerProfiles = serde_json::from_value(json!({"svc": {"runner": {
            "kind": "benchmark", "route_target": "candidate", "options": benchmark_options
        }}}))
        .unwrap();
        let selected = approved_test_profile(&profiles, &benchmark, "candidate").unwrap();
        assert!(from_observation(
            benchmark,
            target("model-a", "candidate"),
            &observed,
            now,
            60,
            &selected.options
        )
        .is_ok());
    }

    #[test]
    fn rollout_candidate_requires_exact_live_revision_and_flux_commit() {
        let mut observed = snapshot(Utc::now());
        let commit = "0123456789012345678901234567890123456789";
        assert!(check_rollout_candidate(&observed, None).is_ok());
        assert!(check_rollout_candidate(&observed, Some(("rev-a", commit))).is_ok());
        assert!(check_rollout_candidate(&observed, Some(("other-revision", commit))).is_err());
        assert!(check_rollout_candidate(
            &observed,
            Some(("rev-a", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"))
        )
        .is_err());
        observed.deployments[0].flux_revision = format!("@sha1:{commit}");
        assert!(check_rollout_candidate(&observed, Some(("rev-a", commit))).is_err());
        let sha256 = "b".repeat(64);
        observed.deployments[0].flux_revision = format!("main@sha256:{sha256}");
        assert!(check_rollout_candidate(&observed, Some(("rev-a", &sha256))).is_ok());
        assert!(check_rollout_candidate(&observed, Some(("rev-a", commit))).is_err());
        observed.deployments[0].flux_revision = format!("main@sha1:{commit}");
        observed.deployments.push(observed.deployments[0].clone());
        assert!(check_rollout_candidate(&observed, Some(("rev-a", commit))).is_err());
    }

    #[test]
    fn conversion_rejects_stale_future_ambiguous_or_unreachable_observations() {
        let now = Utc::now();
        let mut observed = snapshot(now);
        assert!(from_observation(
            profile(),
            target("model-a", "candidate"),
            &observed,
            now,
            0,
            &options()
        )
        .is_err());
        observed.observed_at = now + Duration::seconds(1);
        assert!(from_observation(
            profile(),
            target("model-a", "candidate"),
            &observed,
            now,
            60,
            &options()
        )
        .is_err());
        observed.observed_at = now - Duration::seconds(61);
        assert!(from_observation(
            profile(),
            target("model-a", "candidate"),
            &observed,
            now,
            60,
            &options()
        )
        .is_err());
        observed.observed_at = now - Duration::seconds(60);
        assert!(from_observation(
            profile(),
            target("model-a", "candidate"),
            &observed,
            now,
            60,
            &options()
        )
        .is_ok());
        observed.observed_at = now;
        observed.route = "other-route".into();
        assert!(from_observation(
            profile(),
            target("model-a", "candidate"),
            &observed,
            now,
            60,
            &options()
        )
        .is_err());
        observed.route = "model-a-candidate".into();
        observed.deployments.push(observed.deployments[0].clone());
        assert!(from_observation(
            profile(),
            target("model-a", "candidate"),
            &observed,
            now,
            60,
            &options()
        )
        .is_err());
        observed.deployments.pop();
        observed.weights.insert("model-a".into(), 0);
        assert!(from_observation(
            profile(),
            target("model-a", "candidate"),
            &observed,
            now,
            60,
            &options()
        )
        .is_err());
        observed.weights.insert("model-a".into(), 1);
        observed.deployments[0].backend.clear();
        assert!(from_observation(
            profile(),
            target("model-a", "candidate"),
            &observed,
            now,
            60,
            &options()
        )
        .is_err());
        observed.deployments[0].backend = "svc/model-a/rev-a".into();
        observed.hostnames = vec!["other.example".into()];
        assert!(from_observation(
            profile(),
            target("model-a", "candidate"),
            &observed,
            now,
            60,
            &options()
        )
        .is_err());
    }
}
