//! Propose a reviewed, gated GitOps change from an independently observed run.
//!
//! Neither the run request nor a client-provided image is execution evidence.
//! The candidate's committed catalog, live runtime, owned Job and immutable
//! result are joined before constructing the patch. This endpoint creates a
//! candidate PR; it does not approve or bypass the rollout's functional gate.
use std::{path::Path, process::Command, sync::Arc};

use axum::{
    extract::{Path as AxumPath, State},
    http::StatusCode,
    Extension, Json,
};
use k8s_openapi::api::batch::v1::{Job, JobSpec};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::Row;
use thalamus_changes::{
    Change, PreviewConfig, Proposal, Rollout, Store as ChangeStore, WorkerConfig,
};
use thalamus_gitops::{CommitAuthor, GitOpsRepo};
use thalamus_identity::AuthorizedIdentity;
use thalamus_runs::{parse_benchmark_result, Run, RunKind, Store as RunStore};
use thalamus_runtime::{
    admission, binding,
    records::{Records, VerifiedEvidence},
    DeploymentSnapshot, RouteSnapshot,
};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    capability_evidence::validated_contract,
    workflows::{self, PatchInput, PatchOperation},
    ApiError, AppState, RequestPrincipal,
};

#[derive(Serialize, ToSchema)]
pub(crate) struct RunPromotionView {
    pub change_id: Uuid,
    pub pr_url: String,
}

fn conflict(message: &'static str) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, message)
}

fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "run not found")
}

fn unavailable() -> ApiError {
    workflows::unavailable("run promotion is unavailable")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PromotionRoute {
    Candidate,
    AgenticCandidate,
    Benchmark,
}

struct Proof {
    run: Run,
    evidence: VerifiedEvidence,
    target: binding::Target,
    route: PromotionRoute,
}

// Only the persisted, worker-prepared Job determines which observer to use.
// A run request or the name of a recorded route is not proof of that choice.
fn promotion_route(run: &Run, job: &Job) -> Result<(PromotionRoute, Option<Value>), ApiError> {
    match &run.request.kind {
        RunKind::Validation => {
            let options = admission::options_from_job(run, job)
                .map_err(|_| conflict("prepared runner test options are invalid"))?;
            let route = if admission::requires_agentic(&run.request.kind, &options)
                .map_err(|_| conflict("prepared runner protocol options are invalid"))?
            {
                PromotionRoute::AgenticCandidate
            } else {
                PromotionRoute::Candidate
            };
            Ok((route, Some(options)))
        }
        RunKind::Benchmark => Ok((PromotionRoute::Benchmark, None)),
        RunKind::Debug => Err(conflict("debug runs cannot be promoted")),
    }
}

// The worker's persisted Job options, not a caller or summary assertion,
// determine which protocol checks must actually have passed.
fn agentic_summary_matches_options(summary: &Value, options: &Value, budget: u32) -> bool {
    let Some(checks) = summary.get("checks").and_then(Value::as_array) else {
        return false;
    };
    let Some(expected_context) = options.get("context_tokens").and_then(Value::as_u64) else {
        return false;
    };
    if expected_context == 0
        || expected_context > u64::from(budget)
        || summary.get("tool_parser").and_then(Value::as_str)
            != options.get("tool_parser").and_then(Value::as_str)
    {
        return false;
    }
    let check = |name: &str, status: &str| -> Option<&Value> {
        let mut matches = checks
            .iter()
            .filter(|item| item.get("name").and_then(Value::as_str) == Some(name));
        let found = matches.next()?;
        if matches.next().is_some() || found.get("status").and_then(Value::as_str) != Some(status) {
            return None;
        }
        Some(found)
    };
    let context = |name: &str| -> bool {
        check(name, "passed")
            .and_then(|item| item.get("detail").and_then(Value::as_str))
            .and_then(|detail| detail.strip_prefix("accepted "))
            .and_then(|detail| {
                detail.strip_suffix(" prompt tokens; quality under load not measured")
            })
            .and_then(|count| count.parse::<u64>().ok())
            .is_some_and(|count| count >= expected_context && count <= u64::from(budget))
    };
    if !context("context_limit")
        || (options
            .get("responses_continuation")
            .and_then(Value::as_bool)
            == Some(true)
            && check("responses_continuation", "passed").is_none())
    {
        return false;
    }
    let tool_status = if options.get("tool_parser").and_then(Value::as_str).is_some() {
        "passed"
    } else {
        "unsupported"
    };
    for (field, required, tool, context_name) in [
        (
            "responses",
            [
                "responses",
                "responses_authentication",
                "responses_authorization",
                "responses_streaming",
            ],
            "responses_tool_loop",
            "responses_context_limit",
        ),
        (
            "anthropic",
            [
                "anthropic",
                "anthropic_authentication",
                "anthropic_authorization",
                "anthropic_streaming",
            ],
            "anthropic_tool_loop",
            "anthropic_context_limit",
        ),
    ] {
        if options.get(field).and_then(Value::as_bool) == Some(true)
            && (required.iter().any(|name| check(name, "passed").is_none())
                || check(tool, tool_status).is_none()
                || !context(context_name))
        {
            return false;
        }
    }
    true
}

// The identity guard holds the platform-admin row lock throughout each durable
// mutation. Lock the run and its project as well, so authorization and scope
// cannot change between evidence verification and proposal/publication.
async fn load_proof(
    state: &AppState,
    guard: &mut AuthorizedIdentity<'_>,
    id: Uuid,
) -> Result<Proof, ApiError> {
    let team: String = sqlx::query_scalar("SELECT team FROM thalamus_runs WHERE id=$1 FOR SHARE")
        .bind(id)
        .fetch_optional(&mut **guard.transaction())
        .await?
        .ok_or_else(not_found)?;
    let team_id = Uuid::parse_str(&team).map_err(|_| not_found())?;
    if team_id.is_nil() || team != team_id.to_string() {
        return Err(not_found());
    }
    let run = RunStore::new(state.db.clone())
        .get_in_transaction(guard.transaction(), &team, id)
        .await
        .map_err(|error| match error {
            thalamus_runs::Error::NotFound => not_found(),
            _ => unavailable(),
        })?;
    let project: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM identity_projects WHERE id=$1 AND team_id=$2 FOR SHARE")
            .bind(run.request.project_id)
            .bind(team_id)
            .fetch_optional(&mut **guard.transaction())
            .await?;
    if project.is_none() {
        return Err(not_found());
    }
    if run.state != "succeeded"
        || !matches!(&run.request.kind, RunKind::Validation | RunKind::Benchmark)
        || (run.request.kind == RunKind::Validation && run.request.workload.name != "functional")
        || run.request.fingerprint().ok().as_deref() != Some(run.fingerprint.as_str())
        || run.job_name != thalamus_runs::job_name(id)
    {
        return Err(conflict(
            "run is not a successful trusted validation or benchmark",
        ));
    }

    let runtime = state.config.runtime.as_ref().ok_or_else(unavailable)?;
    let binding = runtime
        .routes
        .get(&run.request.service)
        .ok_or_else(unavailable)?;
    if binding.team_id != team_id
        || binding.project_id != run.request.project_id
        || binding.service.name != run.request.service
        || run.job_namespace.as_deref() != Some(runtime.namespace.as_str())
    {
        return Err(conflict(
            "run does not match the configured service and project",
        ));
    }
    let config = workflows::gitops(state)?;
    let policy = workflows::policy(config, &run.request.service)?;
    if policy
        .controller
        .as_ref()
        .is_none_or(|controller| controller.owner_team != team_id)
        || !matches!(&policy.rollout, Rollout::Gated)
    {
        return Err(conflict(
            "service requires an owner-bound gated rollout policy",
        ));
    }

    let evidence = Records::new(state.db.clone())
        .get_verified_in_transaction(guard.transaction(), id)
        .await
        .map_err(|_| conflict("verified execution evidence is invalid"))?
        .ok_or_else(|| conflict("verified execution evidence is unavailable"))?;
    if evidence.run_id != id
        || evidence.request_fingerprint != run.fingerprint
        || !evidence.prelaunch.same_identity(&evidence.ingestion)
        || evidence.runner.job != run.job_name
        || run.job_uid.as_deref() != Some(evidence.runner.job_uid.as_str())
        || evidence.runner.job_uid.is_empty()
        || evidence.runner.pod_uid.is_empty()
        || evidence.runner.pod_owner_job_uid != evidence.runner.job_uid
        || evidence.prelaunch.deployments.len() != 1
        || evidence.prelaunch.deployments[0].deployment != run.request.deployment
    {
        return Err(conflict(
            "execution does not match the authorized candidate",
        ));
    }
    let spec = run
        .job_spec
        .as_ref()
        .ok_or_else(|| conflict("prepared Job is unavailable"))?;
    let job_spec: JobSpec =
        serde_json::from_value(spec.clone()).map_err(|_| conflict("prepared Job is invalid"))?;
    let job = Job {
        spec: Some(job_spec),
        ..Default::default()
    };
    let target = binding::target_from_job(&run, &job, "candidate")
        .map_err(|_| conflict("runner Job does not target the candidate"))?;
    let (route, options) = promotion_route(&run, &job)?;
    binding::check_snapshot(
        &run,
        &target,
        &evidence.prelaunch,
        &evidence.prelaunch.route,
    )
    .map_err(|_| conflict("runner endpoint does not match the observed route"))?;
    let deployment = &evidence.prelaunch.deployments[0];

    let artifact = match run.artifacts.as_deref() {
        Some([one]) => one,
        _ => {
            return Err(conflict(
                "run requires exactly one immutable result artifact",
            ))
        }
    };
    let artifacts = state.benchmark_artifacts.as_ref().ok_or_else(unavailable)?;
    let bytes = artifacts
        .read(id, artifact)
        .await
        .map_err(|error| match error {
            thalamus_runs::Error::ArtifactIntegrity => {
                conflict("result artifact failed integrity verification")
            }
            _ => unavailable(),
        })?;
    match &run.request.kind {
        RunKind::Validation => {
            let summary: Value = serde_json::from_slice(&bytes)
                .map_err(|_| conflict("functional summary is invalid"))?;
            if summary.get("suite").and_then(Value::as_str) != Some("candidate")
                || validated_contract(&summary, id, &run.request, &deployment.backend).is_none()
                || (route == PromotionRoute::AgenticCandidate
                    && options.as_ref().is_none_or(|options| {
                        !agentic_summary_matches_options(
                            &summary,
                            options,
                            run.request.workload.model_budget_tokens,
                        )
                    }))
            {
                return Err(conflict(
                    "functional summary did not pass for the observed backend",
                ));
            }
        }
        RunKind::Benchmark => {
            let summary = parse_benchmark_result(&run, &bytes)
                .map_err(|_| conflict("benchmark summary is invalid"))?;
            // Failed requests at saturation are valid benchmark observations;
            // require at least one real successful measurement, not a perfect SLO.
            if !summary.points.iter().any(|point| {
                point.successes > 0
                    && (point.ttft_seconds.is_some()
                        || point.itl_seconds.is_some()
                        || point.throughput.requests_per_second > 0.0)
            }) {
                return Err(conflict("benchmark has no successful measurements"));
            }
        }
        RunKind::Debug => return Err(conflict("debug runs cannot be promoted")),
    }
    observe_again(state, &run, &evidence, &target, route).await?;
    Ok(Proof {
        run,
        evidence,
        target,
        route,
    })
}

async fn observe_again(
    state: &AppState,
    run: &Run,
    evidence: &VerifiedEvidence,
    target: &binding::Target,
    route: PromotionRoute,
) -> Result<(), ApiError> {
    let observer = state.runtime_observer.as_ref().ok_or_else(unavailable)?;
    let runtime = state.config.runtime.as_ref().ok_or_else(unavailable)?;
    if observer.config() != runtime {
        return Err(unavailable());
    }
    let public_route = &runtime
        .routes
        .get(&run.request.service)
        .ok_or_else(unavailable)?
        .public;
    let current = match (&run.request.kind, route) {
        (RunKind::Validation, PromotionRoute::Candidate) => {
            observer
                .observe_candidate(&run.request.service, &run.request.deployment)
                .await
        }
        (RunKind::Validation, PromotionRoute::AgenticCandidate) => {
            observer
                .observe_agentic_candidate(&run.request.service, &run.request.deployment)
                .await
        }
        (RunKind::Benchmark, PromotionRoute::Benchmark) => {
            observer
                .observe_benchmark(&run.request.service, &run.request.deployment)
                .await
        }
        _ => return Err(conflict("run route is not an authorized candidate route")),
    }
    .map_err(|_| unavailable())?;
    check_refreshed_route(run, evidence, target, route, &current, public_route)
}

fn check_refreshed_route(
    run: &Run,
    evidence: &VerifiedEvidence,
    target: &binding::Target,
    route: PromotionRoute,
    current: &RouteSnapshot,
    public_route: &str,
) -> Result<(), ApiError> {
    let agentic = match (&run.request.kind, route) {
        (RunKind::Validation, PromotionRoute::Candidate)
        | (RunKind::Benchmark, PromotionRoute::Benchmark) => false,
        (RunKind::Validation, PromotionRoute::AgenticCandidate) => true,
        _ => return Err(conflict("run route is not an authorized candidate route")),
    };
    let matches_route = |snapshot: &RouteSnapshot| {
        admission::matches_route(
            snapshot,
            &run.request.deployment,
            &run.request.kind,
            "candidate",
            public_route,
            agentic,
        )
    };
    if target.choice != binding::RouteChoice::Candidate
        || !matches_route(&evidence.prelaunch)
        || !matches_route(current)
    {
        return Err(conflict("run route is not the derived candidate route"));
    }
    // The agentic observer discovers the outer route and independently joins
    // its complete chain to the authorized candidate. Never derive that outer
    // name from the request or accept a public-route observation instead.
    if !evidence.prelaunch.same_identity(current) {
        return Err(conflict("candidate route or runtime changed since the run"));
    }
    binding::check_snapshot(run, target, current, &current.route)
        .map_err(|_| conflict("runner endpoint does not match the observed route"))
}
// Kubernetes adds API defaults to persisted specs. Require every rendered
// field, and preserve sequence order and cardinality; defaulted map keys may
// exist in the live resource but cannot replace a rendered value.
fn contains_rendered(expected: &Value, actual: &Value) -> bool {
    match (expected, actual) {
        (Value::Object(expected), Value::Object(actual)) => expected.iter().all(|(key, value)| {
            actual
                .get(key)
                .is_some_and(|other| contains_rendered(value, other))
        }),
        (Value::Array(expected), Value::Array(actual)) => {
            expected.len() == actual.len()
                && expected
                    .iter()
                    .zip(actual)
                    .all(|(a, b)| contains_rendered(a, b))
        }
        _ => expected == actual,
    }
}

fn same_engine(expected: &Value, observed: &Value) -> bool {
    [
        "image",
        "command",
        "args",
        "env",
        "resources",
        "securityContext",
        "volumeMounts",
    ]
    .iter()
    .all(|key| expected.get(*key) == observed.get(*key))
}

// Routing names and revisions deliberately differ in the newly generated
// candidate. The model manifest and actual pod template must not.
#[derive(Debug, PartialEq, Eq)]
struct RenderedIdentity {
    kind: String,
    pod: Value,
    model: Option<Value>,
    replicas: Option<Value>,
    strategy: Option<Value>,
    manifest: Value,
}

fn rendered_workload<'a>(
    document: &'a Value,
    namespace: &str,
    name: &str,
) -> Result<(&'a Value, &'static str), ApiError> {
    let fail = || conflict("rendered workload identity is missing or ambiguous");
    let items = document
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(fail)?;
    let mut matching = items.iter().filter(|item| {
        item.pointer("/metadata/name").and_then(Value::as_str) == Some(name)
            && item.pointer("/metadata/namespace").and_then(Value::as_str) == Some(namespace)
            && matches!(
                item.get("kind").and_then(Value::as_str),
                Some("Deployment" | "LLMInferenceService")
            )
    });
    let workload = matching.next().ok_or_else(fail)?;
    if matching.next().is_some() {
        return Err(fail());
    }
    let template = match workload.get("kind").and_then(Value::as_str) {
        Some("Deployment") => "/spec/template/spec",
        Some("LLMInferenceService") => "/spec/template",
        _ => return Err(fail()),
    };
    Ok((workload, template))
}

fn rendered_identity(
    document: &Value,
    namespace: &str,
    workload: &Value,
    template: &str,
) -> Result<RenderedIdentity, ApiError> {
    let fail = || conflict("rendered immutable model manifest is missing or invalid");
    let manifest_name = workload
        .pointer("/metadata/annotations/thalamus.ai~1model-manifest")
        .and_then(Value::as_str)
        .ok_or_else(fail)?;
    let mut manifests = document
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(fail)?
        .iter()
        .filter(|item| {
            item.get("kind").and_then(Value::as_str) == Some("ConfigMap")
                && item.pointer("/metadata/name").and_then(Value::as_str) == Some(manifest_name)
                && item.pointer("/metadata/namespace").and_then(Value::as_str) == Some(namespace)
        });
    let manifest = manifests.next().ok_or_else(fail)?;
    if manifests.next().is_some()
        || manifest.get("immutable").and_then(Value::as_bool) != Some(true)
    {
        return Err(fail());
    }
    let manifest: Value = serde_json::from_str(
        manifest
            .pointer("/data/manifest.json")
            .and_then(Value::as_str)
            .ok_or_else(fail)?,
    )
    .map_err(|_| fail())?;
    let kind = workload
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(fail)?;
    Ok(RenderedIdentity {
        kind: kind.to_owned(),
        pod: workload.pointer(template).cloned().ok_or_else(fail)?,
        model: workload.pointer("/spec/model").cloned(),
        replicas: workload.pointer("/spec/replicas").cloned(),
        strategy: workload
            .pointer(if kind == "Deployment" {
                "/spec/strategy"
            } else {
                "/spec/rolloutStrategy"
            })
            .cloned(),
        manifest,
    })
}

// The store saves render hashes/diffs, not the full rendered document. Render
// the transformed candidate in an isolated worktree and require the exact
// validated preview before comparing it to the execution's source render.
fn verify_candidate_render(
    repo: &GitOpsRepo,
    preview_config: &PreviewConfig,
    change: &Change,
    candidate: &str,
    namespace: &str,
    tested: &RenderedIdentity,
) -> Result<(), ApiError> {
    let fail = || conflict("candidate render differs from the tested configuration");
    let path = preview_config
        .worktree_root
        .join(format!("run-candidate-{}", Uuid::new_v4()));
    let mut tree = repo
        .create_worktree(&change.request.base_sha, &path)
        .map_err(|_| unavailable())?;
    let result = tree.preview(&change.request.patches, &preview_config.render);
    let cleanup = repo.discard_worktree(tree);
    let preview = match (result, cleanup) {
        (Ok(preview), Ok(())) => preview,
        _ => return Err(unavailable()),
    };
    let base_hash = format!("{:x}", Sha256::digest(preview.base_render.as_bytes()));
    let proposed_hash = format!("{:x}", Sha256::digest(preview.proposed_render.as_bytes()));
    if change.base_render_sha256.as_deref() != Some(base_hash.as_str())
        || change.proposed_render_sha256.as_deref() != Some(proposed_hash.as_str())
        || change.data_diff.as_deref() != Some(preview.data_diff.as_str())
        || change.rendered_diff.as_deref() != Some(preview.rendered_diff.as_str())
    {
        return Err(fail());
    }
    let yaml: serde_yaml::Value =
        serde_yaml::from_str(&preview.proposed_render).map_err(|_| unavailable())?;
    let document = serde_json::to_value(yaml).map_err(|_| unavailable())?;
    let (workload, template) = rendered_workload(&document, namespace, candidate)?;
    if rendered_identity(&document, namespace, workload, template)? != *tested {
        return Err(fail());
    }
    Ok(())
}

// Flux's lastAppliedRevision identifies a source commit but does not prove
// that a live workload has not drifted since reconciliation. Render that
// commit with the operator's CI command and compare its workload with the
// independently fetched live resource and the actual observed engine Pod.
// Raw resource values are kept only in memory and never persisted or returned.
async fn verify_rendered_execution(
    state: &AppState,
    repo: &GitOpsRepo,
    config: &workflows::GitOpsConfig,
    source_commit: &str,
    source_file: &Path,
    observed: &DeploymentSnapshot,
) -> Result<RenderedIdentity, ApiError> {
    let fail = || conflict("rendered tested configuration differs from the observed workload");
    let runtime = state.config.runtime.as_ref().ok_or_else(unavailable)?;
    let observer = state.runtime_observer.as_ref().ok_or_else(unavailable)?;
    let preview = workflows::preview_config(config, source_file)?;
    let worktree = config
        .worktree_root
        .join(format!("run-evidence-{}", Uuid::new_v4()));
    let rendered = repo
        .render_ref(source_commit, &worktree, &preview.render)
        .map_err(|_| unavailable())?;
    let yaml: serde_yaml::Value = serde_yaml::from_str(&rendered).map_err(|_| unavailable())?;
    let document = serde_json::to_value(yaml).map_err(|_| unavailable())?;
    let (expected, template) =
        rendered_workload(&document, &runtime.namespace, &observed.deployment)?;
    let (group, version, kind, plural) = match (
        expected.get("apiVersion").and_then(Value::as_str),
        expected.get("kind").and_then(Value::as_str),
    ) {
        (Some("apps/v1"), Some("Deployment")) => ("apps", "v1", "Deployment", "deployments"),
        (Some(api), Some("LLMInferenceService"))
            if api.strip_prefix("serving.kserve.io/") == Some(runtime.serving_version.as_str()) =>
        {
            (
                "serving.kserve.io",
                runtime.serving_version.as_str(),
                "LLMInferenceService",
                "llminferenceservices",
            )
        }
        _ => return Err(fail()),
    };
    let live = observer
        .get_resource(
            &runtime.namespace,
            group,
            version,
            kind,
            plural,
            &observed.deployment,
        )
        .await
        .map_err(|_| unavailable())?;
    let expected_spec = expected.get("spec").ok_or_else(fail)?;
    let live_spec = live.get("spec").ok_or_else(fail)?;
    if live.get("apiVersion") != expected.get("apiVersion")
        || live.get("kind") != expected.get("kind")
        || live.pointer("/metadata/name") != expected.pointer("/metadata/name")
        || live.pointer("/metadata/namespace") != expected.pointer("/metadata/namespace")
        || !expected
            .pointer("/metadata/annotations")
            .zip(live.pointer("/metadata/annotations"))
            .is_some_and(|(a, b)| contains_rendered(a, b))
        || !contains_rendered(expected_spec, live_spec)
    {
        return Err(fail());
    }
    // Extra node selectors change the hardware that was actually tested.
    let selector = format!("{template}/nodeSelector");
    if expected.pointer(&selector) != live.pointer(&selector) {
        return Err(fail());
    }
    let containers = expected
        .pointer(&format!("{template}/containers"))
        .and_then(Value::as_array)
        .ok_or_else(fail)?;
    let mut engines = containers
        .iter()
        .filter(|item| item.get("name").and_then(Value::as_str) == Some("main"));
    let engine = engines.next().ok_or_else(fail)?;
    if engines.next().is_some() {
        return Err(fail());
    }
    if !same_engine(engine, &observed.engine_spec) {
        return Err(fail());
    }
    let identity = rendered_identity(&document, &runtime.namespace, expected, template)?;
    if identity.manifest.get("image") != observed.engine_spec.get("image")
        || identity.manifest.get("modelPath").and_then(Value::as_str) != Some("/opt/thalamus/model")
        || identity.manifest.get("modelDigest").and_then(Value::as_str)
            != Some(observed.runtime.model_content.as_str())
        || identity
            .manifest
            .get("tokenizerDigest")
            .and_then(Value::as_str)
            != Some(observed.runtime.tokenizer_content.as_str())
    {
        return Err(fail());
    }
    Ok(identity)
}

// The Flux observer attests the source revision; use its commit as an object
// ID only in the independently configured and origin-bound GitOps repository.
pub(crate) fn flux_commit(revision: &str) -> Option<&str> {
    let (reference, hash) = revision.rsplit_once('@')?;
    let (algorithm, commit) = hash.split_once(':')?;
    if reference.is_empty()
        || reference.trim() != reference
        || reference.chars().any(char::is_control)
        || !matches!((algorithm, commit.len()), ("sha1", 40) | ("sha256", 64))
        || !commit
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    Some(commit)
}

fn committed_yaml(
    config: &workflows::GitOpsConfig,
    commit: &str,
    path: &Path,
) -> Result<Value, ApiError> {
    let file = path.to_str().ok_or_else(unavailable)?;
    let listing = Command::new("git")
        .arg("--git-dir")
        .arg(&config.repository_dir)
        .args(["ls-tree", "-z", commit, "--", file])
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .output()
        .map_err(|_| unavailable())?;
    if !listing.status.success() {
        return Err(unavailable());
    }
    let entry = listing.stdout.strip_suffix(&[0]).ok_or_else(unavailable)?;
    let tab = entry
        .iter()
        .position(|b| *b == b'\t')
        .ok_or_else(unavailable)?;
    let (meta, listed) = (&entry[..tab], &entry[tab + 1..]);
    let metadata = std::str::from_utf8(meta).map_err(|_| unavailable())?;
    let mut parts = metadata.split(' ');
    let mode = parts.next();
    let kind = parts.next();
    let object = parts.next().ok_or_else(unavailable)?;
    if parts.next().is_some()
        || !matches!(mode, Some("100644" | "100755"))
        || kind != Some("blob")
        || !workflows::valid_sha(object)
        || listed != file.as_bytes()
    {
        return Err(unavailable());
    }
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(&config.repository_dir)
        .args(["cat-file", "blob", object])
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .output()
        .map_err(|_| unavailable())?;
    if !output.status.success() {
        return Err(unavailable());
    }
    let yaml: serde_yaml::Value =
        serde_yaml::from_slice(&output.stdout).map_err(|_| unavailable())?;
    serde_json::to_value(yaml).map_err(|_| unavailable())
}

fn required_text<'a>(object: &'a Value, key: &str) -> Option<&'a str> {
    object.get(key)?.as_str().filter(|value| !value.is_empty())
}

// Keep exactly the same revision definition as the gated rollout transformation:
// remove revision/role/weight and hash the canonical deployment plus its pinned
// artifact and hardware. This also detects inconsistent source catalog data.
fn deployment_revision(deployment: &Value, artifact: &Value, hardware: &Value) -> Option<String> {
    let mut deployment = deployment.clone();
    let object = deployment.as_object_mut()?;
    for key in ["revision", "role", "weight"] {
        object.remove(key);
    }
    let bytes = serde_json::to_vec(
        &json!({"deployment": deployment, "artifact": artifact, "hardware": hardware}),
    )
    .ok()?;
    Some(format!("{:x}", Sha256::digest(bytes)))
}

fn unique_deployment<'a>(doc: &'a Value, name: &str) -> Option<&'a Value> {
    let mut found = doc
        .get("deployments")?
        .as_array()?
        .iter()
        .filter(|deployment| required_text(deployment, "name") == Some(name));
    let result = found.next()?;
    if found.next().is_some() {
        return None;
    }
    Some(result)
}

fn build_patch(
    source: &Value,
    base: &Value,
    source_artifacts: &Value,
    base_artifacts: &Value,
    source_hardware: &Value,
    base_hardware: &Value,
    observed: &DeploymentSnapshot,
) -> Result<Vec<PatchInput>, ApiError> {
    let fail = || conflict("committed catalog does not match the observed tested configuration");
    let deployment = unique_deployment(source, &observed.deployment).ok_or_else(fail)?;
    let artifact_name = required_text(deployment, "artifact").ok_or_else(fail)?;
    let hardware_name = required_text(deployment, "hardware").ok_or_else(fail)?;
    let artifact = source_artifacts.get(artifact_name).ok_or_else(fail)?;
    let hardware = source_hardware.get(hardware_name).ok_or_else(fail)?;
    if base_artifacts.get(artifact_name) != Some(artifact)
        || base_hardware.get(hardware_name) != Some(hardware)
        || deployment.get("packagedModel").and_then(Value::as_bool) != Some(true)
        || deployment
            .get("rolloutStrategy")
            .and_then(Value::as_str)
            .unwrap_or("RollingUpdate")
            != "RollingUpdate"
        || required_text(deployment, "revision") != Some(observed.revision.as_str())
        || deployment_revision(deployment, artifact, hardware).as_deref()
            != Some(observed.revision.as_str())
        || required_text(artifact, "modelDigest") != Some(observed.runtime.model_content.as_str())
        || required_text(artifact, "tokenizerDigest")
            != Some(observed.runtime.tokenizer_content.as_str())
    {
        return Err(fail());
    }
    let image = required_text(deployment, "image").ok_or_else(fail)?;
    let (repository, digest) = image.rsplit_once('@').ok_or_else(fail)?;
    if repository.is_empty()
        || repository
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
        || digest.strip_prefix("sha256:").is_none_or(|d| {
            d.len() != 64
                || !d
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
        || observed
            .runtime
            .image_digests
            .get("container/main")
            .map(String::as_str)
            != Some(digest)
        || required_text(artifact, "uri").and_then(|uri| uri.strip_prefix("oci://")) != Some(image)
    {
        return Err(fail());
    }

    // Service-level settings and the two selected dependencies cannot be silently
    // changed when carrying a tested deployment onto a different base revision.
    let source_other = source.as_object().ok_or_else(fail)?;
    let base_other = base.as_object().ok_or_else(fail)?;
    if !source_other
        .iter()
        .filter(|(key, _)| key.as_str() != "deployments")
        .eq(base_other
            .iter()
            .filter(|(key, _)| key.as_str() != "deployments"))
    {
        return Err(fail());
    }
    let deployments = base
        .get("deployments")
        .and_then(Value::as_array)
        .ok_or_else(fail)?;
    let mut stable = deployments.iter().enumerate().filter(|(_, item)| {
        item.get("role").and_then(Value::as_str).unwrap_or("stable") == "stable"
            && item.get("weight").and_then(Value::as_i64).unwrap_or(1) > 0
    });
    let (index, before) = stable.next().ok_or_else(fail)?;
    if stable.next().is_some() {
        return Err(conflict(
            "gated rollout requires exactly one public stable deployment",
        ));
    }
    let name = required_text(before, "name").ok_or_else(fail)?.to_owned();
    if unique_deployment(base, &name).is_none() {
        return Err(fail());
    }
    let mut desired = deployment.clone();
    let object = desired.as_object_mut().ok_or_else(fail)?;
    object.insert("name".into(), Value::String(name));
    for key in ["role", "weight"] {
        match before.get(key) {
            Some(value) => {
                object.insert(key.into(), value.clone());
            }
            None => {
                object.remove(key);
            }
        }
    }
    let revision = deployment_revision(&desired, artifact, hardware).ok_or_else(fail)?;
    desired
        .as_object_mut()
        .ok_or_else(fail)?
        .insert("revision".into(), Value::String(revision));
    if &desired == before {
        return Err(conflict(
            "tested configuration is already the stable deployment",
        ));
    }
    Ok(vec![PatchInput {
        path: vec!["deployments".into(), index.to_string()],
        operation: PatchOperation::Set(desired),
    }])
}

#[utoipa::path(
    post, path = "/api/v1/runs/{id}/promote",
    params(("id" = Uuid, Path, description = "Successful validation or benchmark run")),
    responses(
        (status = 200, body = RunPromotionView),
        (status = 401, description = "Authentication required", body = crate::ErrorBody),
        (status = 403, description = "Platform administrator required", body = crate::ErrorBody),
        (status = 404, description = "Run not found", body = crate::ErrorBody),
        (status = 409, description = "Execution evidence, catalog or rollout policy does not match", body = crate::ErrorBody),
        (status = 503, description = "Promotion dependencies unavailable", body = crate::ErrorBody)
    )
)]
pub(crate) async fn promote(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<Uuid>,
    Extension(caller): Extension<RequestPrincipal>,
) -> Result<Json<RunPromotionView>, ApiError> {
    let mut guard = state
        .identity
        .begin_platform_admin(caller.principal())
        .await?;
    let proof = load_proof(&state, &mut guard, id).await?;
    let config = workflows::gitops(&state)?;
    let policy = workflows::policy(config, &proof.run.request.service)?;
    let repo = workflows::open_repo(config, &policy.target_branch)?;
    repo.fetch().map_err(|_| unavailable())?;
    let key = format!("run-promotion:{id}");
    let actor = guard.actor_id().to_string();
    // Serialize by run rather than caller, so two authorized admins cannot
    // create two PRs for the same execution. Only an identity-audited promotion
    // is considered an existing operation: arbitrary caller-chosen change keys
    // are not evidence that a run was promoted.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('thalamus.run-promotion'), hashtext($1))")
        .bind(id.to_string())
        .execute(&mut **guard.transaction())
        .await?;
    let existing = sqlx::query(
        "SELECT c.id,c.request->>'base_sha' AS base_sha,c.fingerprint FROM thalamus_changes c \
         WHERE c.idempotency_key=$1 AND EXISTS (SELECT 1 FROM identity_audit a \
         WHERE a.action='run.promote.propose' AND a.target=($2 || '/' || c.id::text)) FOR UPDATE OF c",
    ).bind(&key).bind(id.to_string()).fetch_all(&mut **guard.transaction()).await?;
    let existing = match existing.as_slice() {
        [] => None,
        [row] => Some((
            row.try_get::<Uuid, _>("id")?,
            row.try_get::<Option<String>, _>("base_sha")?
                .ok_or_else(|| conflict("existing promotion has an invalid base"))?,
            row.try_get::<String, _>("fingerprint")?,
        )),
        _ => return Err(conflict("multiple promotions exist for this run")),
    };
    let base = match &existing {
        Some((_, base, _)) if workflows::valid_sha(base) => base.clone(),
        Some(_) => return Err(conflict("existing promotion has an invalid base")),
        None => repo
            .resolve_base(&policy.target_branch)
            .map_err(|_| unavailable())?,
    };
    let observed = &proof.evidence.prelaunch.deployments[0];
    let source_commit = flux_commit(&observed.flux_revision)
        .ok_or_else(|| conflict("observed Flux revision is not immutable"))?;
    let source_file =
        workflows::catalog_service_file(config, source_commit, &proof.run.request.service)?;
    let base_file = workflows::catalog_service_file(config, &base, &proof.run.request.service)?;
    let source = committed_yaml(config, source_commit, &source_file)?;
    let base_doc = committed_yaml(config, &base, &base_file)?;
    let source_artifacts =
        committed_yaml(config, source_commit, Path::new("catalog/artifacts.yaml"))?;
    let base_artifacts = committed_yaml(config, &base, Path::new("catalog/artifacts.yaml"))?;
    let source_hardware =
        committed_yaml(config, source_commit, Path::new("catalog/hardware.yaml"))?;
    let base_hardware = committed_yaml(config, &base, Path::new("catalog/hardware.yaml"))?;
    let tested =
        verify_rendered_execution(&state, &repo, config, source_commit, &source_file, observed)
            .await?;
    let input = build_patch(
        &source,
        &base_doc,
        &source_artifacts,
        &base_artifacts,
        &source_hardware,
        &base_hardware,
        observed,
    )?;
    let patches = workflows::patches(&base_file, input)?;
    workflows::verify_base_service(config, &base, &base_file, &proof.run.request.service)?;
    let proposal = Proposal {
        service: proof.run.request.service.clone(),
        base_sha: base,
        target_branch: policy.target_branch.clone(),
        title: format!(
            "Tested configuration for {} (run {id})",
            proof.run.request.service
        ),
        patches,
        validation_tier: policy.validation_tier.clone(),
        rollout: policy.rollout.clone(),
        revert_commit: None,
    };
    let store = ChangeStore::new(state.db.clone());
    observe_again(
        &state,
        &proof.run,
        &proof.evidence,
        &proof.target,
        proof.route,
    )
    .await?;
    let change = if let Some((existing_id, _, fingerprint)) = existing {
        let value = serde_json::to_value(&proposal).map_err(|_| unavailable())?;
        let bytes = serde_json::to_vec(&value).map_err(|_| unavailable())?;
        if format!("{:x}", Sha256::digest(bytes)) != fingerprint {
            return Err(conflict(
                "existing promotion does not match the verified run",
            ));
        }
        store
            .get_in_transaction(guard.transaction(), existing_id)
            .await
            .map_err(workflows::change_error)?
    } else {
        store
            .propose_in_transaction(guard.transaction(), &actor, &key, proposal)
            .await
            .map_err(workflows::change_error)?
    };
    guard
        .audit(None, "run.promote.propose", &format!("{id}/{}", change.id))
        .await?;
    if let Some(url) = change.pr_url.clone() {
        guard.commit().await?;
        return Ok(Json(RunPromotionView {
            change_id: change.id,
            pr_url: url,
        }));
    }
    let preview = workflows::preview_config(config, &base_file)?;
    let validated = match store
        .validate_in_transaction(guard.transaction(), change.id, &actor, &repo, &preview)
        .await
    {
        Ok(validated) => validated,
        Err(error) => {
            // Preserve validation invalidation and its audit even on render failure.
            guard
                .audit(None, "run.promote.validate", &format!("{id}/{}", change.id))
                .await?;
            guard.commit().await?;
            return Err(workflows::change_error(error));
        }
    };
    let candidate: Option<String> =
        sqlx::query_scalar("SELECT candidate_deployment FROM thalamus_rollouts WHERE change_id=$1")
            .bind(change.id)
            .fetch_one(&mut **guard.transaction())
            .await?;
    let candidate = candidate.ok_or_else(|| conflict("gated candidate was not prepared"))?;
    let namespace = &state
        .config
        .runtime
        .as_ref()
        .ok_or_else(unavailable)?
        .namespace;
    verify_candidate_render(&repo, &preview, &validated, &candidate, namespace, &tested)?;
    guard
        .audit(None, "run.promote.validate", &format!("{id}/{}", change.id))
        .await?;
    guard.commit().await?;

    // Commit the deterministic change before remote publication, so recovery after
    // a push/PR succeeds with the same branch and change ID. Reauthorize and
    // re-observe under the second transaction before any forge side effects.
    let mut guard = state
        .identity
        .begin_platform_admin(caller.principal())
        .await?;
    if guard.actor_id().to_string() != actor {
        return Err(conflict("promotion principal changed before publication"));
    }
    let current = load_proof(&state, &mut guard, id).await?;
    if current.run.fingerprint != proof.run.fingerprint
        || current.run.artifacts != proof.run.artifacts
        || current.evidence != proof.evidence
    {
        return Err(conflict("run evidence changed before publication"));
    }
    let change = store
        .get_in_transaction(guard.transaction(), change.id)
        .await
        .map_err(workflows::change_error)?;
    if serde_json::to_value(&change.request).map_err(|_| unavailable())?
        != serde_json::to_value(&validated.request).map_err(|_| unavailable())?
        || change.base_render_sha256 != validated.base_render_sha256
        || change.proposed_render_sha256 != validated.proposed_render_sha256
        || change.data_diff != validated.data_diff
        || change.rendered_diff != validated.rendered_diff
    {
        return Err(conflict("validated candidate changed before publication"));
    }
    workflows::check_current_policy(&change, config)?;
    let file = workflows::change_file(&change)?;
    workflows::verify_base_service(
        config,
        &change.request.base_sha,
        &file,
        &change.request.service,
    )?;
    let github = config.github.as_ref().ok_or_else(unavailable)?;
    let repository = forge::RepoRef::parse(forge::ForgeKind::Github, &github.repository)
        .map_err(|_| unavailable())?;
    let forge = workflows::github_client(github)?;
    let repo = workflows::open_repo(config, &change.request.target_branch)?;
    let preview = workflows::preview_config(config, &file)?;
    let worker = WorkerConfig {
        worktree_root: preview.worktree_root,
        render: preview.render,
        commit_author: CommitAuthor {
            name: github.commit_author_name.clone(),
            email: github.commit_author_email.clone(),
        },
        repository,
        forge_actor: github.actor.clone(),
    };
    observe_again(
        &state,
        &current.run,
        &current.evidence,
        &current.target,
        current.route,
    )
    .await?;
    let published = store
        .publish_in_transaction(
            guard.transaction(),
            change.id,
            &actor,
            &repo,
            forge.as_ref(),
            &worker,
        )
        .await
        .map_err(workflows::change_error)?;
    guard
        .audit(None, "run.promote.publish", &format!("{id}/{}", change.id))
        .await?;
    guard.commit().await?;
    let url = published.pr_url.ok_or_else(unavailable)?;

    Ok(Json(RunPromotionView {
        change_id: change.id,
        pr_url: url,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::collections::BTreeMap;
    use thalamus_core::{DesiredIdentity, RuntimeIdentity};
    use thalamus_runs::{EvidenceIdentity, RunConditions, RunRequest, Workload};
    use thalamus_runtime::{RouteListener, RunnerObservation};

    fn hash(c: char) -> String {
        format!("sha256:{}", c.to_string().repeat(64))
    }

    fn fixture() -> (Value, Value, Value, Value, DeploymentSnapshot) {
        let image = format!("registry.example/engine@{}", hash('a'));
        let artifacts = json!({"model": {
            "uri": format!("oci://{image}"),
            "modelDigest": hash('b'), "tokenizerDigest": hash('c')
        }});
        let hardware = json!({"gpu": {"nodeSelector": {"accelerator": "gpu"},
            "gpuCount": 1, "cpu": "4", "memoryGi": 32, "hostMemoryGi": 64}});
        let mut candidate = json!({
            "name": "tested", "role": "candidate", "weight": 1,
            "artifact": "model", "hardware": "gpu", "image": image,
            "packagedModel": true, "rolloutStrategy": "RollingUpdate",
            "contextLength": 8192, "revision": "placeholder"
        });
        let revision =
            deployment_revision(&candidate, &artifacts["model"], &hardware["gpu"]).unwrap();
        candidate["revision"] = json!(revision);
        let source = json!({"name": "service", "namespace": "serving", "deployments": [candidate]});
        let base = json!({"name": "service", "namespace": "serving", "deployments": [{
            "name": "stable", "role": "stable", "weight": 17,
            "artifact": "model", "hardware": "gpu", "image": "registry.example/old",
            "packagedModel": false, "revision": "old"
        }]});
        let target = DeploymentSnapshot {
            deployment: "tested".into(),
            revision,
            flux_revision: format!("main@sha1:{}", "d".repeat(40)),
            backend: "service/tested".into(),
            desired: DesiredIdentity {
                rendered_resources: hash('d'),
                resolved_dependencies: hash('e'),
            },
            rollout_configuration: hash('f'),
            runtime: RuntimeIdentity {
                image_digests: BTreeMap::from([("container/main".into(), hash('a'))]),
                model_content: hash('b'),
                tokenizer_content: hash('c'),
                effective_configuration: hash('f'),
                component_versions: BTreeMap::from([("engine".into(), "v1".into())]),
            },
            engine_spec: json!({}),
            nodes: vec!["node".into()],
            underlying_model: None,
        };
        (source, base, artifacts, hardware, target)
    }

    fn functional_config() -> Value {
        json!({
            "base_url": "https://agentic.example/v1",
            "model": "service",
            "suite": "candidate",
            "backend_header": "x-thalamus-backend",
            "expected_backend": "service/tested",
            "forbidden_model": "other",
            "context_prompt": "hello",
            "context_tokens": 32,
            "tool_parser": null,
            "reasoning": false,
            "responses": false,
            "responses_continuation": false,
            "anthropic": false,
            "timeout_seconds": 30
        })
    }

    fn promotion_fixture(
        kind: RunKind,
        config: &Value,
        route: &str,
        paths: &[&str],
    ) -> (Run, Job, VerifiedEvidence) {
        let now = Utc::now();
        let id = Uuid::from_u128(2);
        let request = RunRequest {
            project_id: Uuid::from_u128(1),
            kind,
            service: "service".into(),
            deployment: "tested".into(),
            runner: "runner".into(),
            workload: Workload {
                name: "functional".into(),
                seed: 1,
                concurrency: 1,
                duration_seconds: 30,
                warmup_seconds: 0,
                max_requests: 20,
                model_budget_tokens: 100,
                tool_budget_calls: 1,
            },
            // These caller assertions cannot establish the route or its chain.
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
        let run = Run {
            id,
            team: Uuid::from_u128(3).to_string(),
            actor: "actor".into(),
            idempotency_key: "key".into(),
            fingerprint: request.fingerprint().unwrap(),
            request,
            job_name: thalamus_runs::job_name(id),
            job_uid: Some("job-uid".into()),
            ownership_nonce: Uuid::from_u128(4),
            job_spec: None,
            job_namespace: Some("serving".into()),
            state: "succeeded".into(),
            artifacts: None,
            error: None,
            created_at: now,
            updated_at: now,
        };
        let field = if run.request.kind == RunKind::Benchmark {
            "THALAMUS_BENCHMARK_CONFIG"
        } else {
            "THALAMUS_VALIDATION_CONFIG"
        };
        let job = Job {
            spec: Some(
                serde_json::from_value(json!({"template":{"spec":{"containers":[{
                    "name":"runner", "env":[
                        {"name":"THALAMUS_RUN_ID","value":id.to_string()},
                        {"name":"THALAMUS_RUN_REQUEST","value":serde_json::to_string(&run.request).unwrap()},
                        {"name":field,"value":config.to_string()}
                    ]
                }]}}}))
                .unwrap(),
            ),
            ..Default::default()
        };
        let agentic = route.starts_with("agentic-cand-");
        let mut deployment = fixture().4;
        if agentic {
            deployment.underlying_model = Some(thalamus_runtime::UnderlyingModelIdentity {
                rollout_configuration: deployment.rollout_configuration.clone(),
                runtime: deployment.runtime.clone(),
            });
        }
        let snapshot = RouteSnapshot {
            observed_at: now,
            service: "service".into(),
            route: route.into(),
            agentic,
            hostnames: vec!["agentic.example".into()],
            paths: paths.iter().map(|path| (*path).into()).collect(),
            ports: vec![443],
            listeners: vec![RouteListener {
                hostname: "agentic.example".into(),
                port: 443,
                protocol: "HTTPS".into(),
            }],
            weights: BTreeMap::from([("tested".into(), 1)]),
            deployments: vec![deployment],
        };
        let evidence = VerifiedEvidence {
            run_id: id,
            request_fingerprint: run.fingerprint.clone(),
            prelaunch: snapshot.clone(),
            ingestion: snapshot,
            runner: RunnerObservation {
                job: run.job_name.clone(),
                job_uid: "job-uid".into(),
                pod: "runner-pod".into(),
                pod_uid: "pod-uid".into(),
                pod_owner_job_uid: "job-uid".into(),
                image: format!("registry/runner@{}", hash('a')),
                image_id: hash('a'),
            },
        };
        (run, job, evidence)
    }

    #[test]
    fn promotion_selects_agentic_only_from_the_persisted_job_options() {
        let config = functional_config();
        let (run, job, _) = promotion_fixture(
            RunKind::Validation,
            &config,
            "tested-candidate",
            &["/v1/chat/completions"],
        );
        assert_eq!(
            promotion_route(&run, &job).unwrap().0,
            PromotionRoute::Candidate
        );
        let mut forged_run = run.clone();
        forged_run.request.evidence.test_definition = "another-claim".into();
        assert!(promotion_route(&forged_run, &job).is_err());
        let mut ambiguous = job.clone();
        let env = ambiguous
            .spec
            .as_mut()
            .unwrap()
            .template
            .spec
            .as_mut()
            .unwrap()
            .containers[0]
            .env
            .as_mut()
            .unwrap();
        let duplicate = env
            .iter()
            .find(|entry| entry.name == "THALAMUS_VALIDATION_CONFIG")
            .unwrap()
            .clone();
        env.push(duplicate);
        assert!(promotion_route(&run, &ambiguous).is_err());
        for key in ["responses", "responses_continuation", "anthropic"] {
            let mut agentic = config.clone();
            agentic[key] = json!(true);
            let (run, job, _) = promotion_fixture(
                RunKind::Validation,
                &agentic,
                "agentic-cand-0123456789abcdef0123456789abcdef",
                &["/v1/chat/completions", "/v1/responses", "/v1/messages"],
            );
            assert_eq!(
                promotion_route(&run, &job).unwrap().0,
                PromotionRoute::AgenticCandidate,
                "{key}"
            );
        }
        let mut invalid = config.clone();
        invalid["responses"] = json!("true");
        let (run, job, _) = promotion_fixture(
            RunKind::Validation,
            &invalid,
            "tested-candidate",
            &["/v1/chat/completions"],
        );
        assert!(promotion_route(&run, &job).is_err());

        let mut public = config;
        public["suite"] = json!("public");
        let (run, job, _) = promotion_fixture(
            RunKind::Validation,
            &public,
            "service-public",
            &["/v1/chat/completions"],
        );
        assert!(binding::target_from_job(&run, &job, "candidate").is_err());
    }

    #[test]
    fn agentic_summary_requires_the_enabled_job_protocol_checks() {
        let mut options = functional_config();
        options["responses"] = json!(true);
        options["anthropic"] = json!(true);
        options["responses_continuation"] = json!(true);
        let mut checks = vec![
            json!({"name":"context_limit","status":"passed",
                "detail":"accepted 64 prompt tokens; quality under load not measured"}),
            json!({"name":"responses_continuation","status":"passed"}),
        ];
        for prefix in ["responses", "anthropic"] {
            for suffix in ["", "_authentication", "_authorization", "_streaming"] {
                checks.push(json!({"name":format!("{prefix}{suffix}"),"status":"passed"}));
            }
            checks.push(json!({"name":format!("{prefix}_tool_loop"),"status":"unsupported"}));
            checks.push(
                json!({"name":format!("{prefix}_context_limit"),"status":"passed",
                "detail":"accepted 64 prompt tokens; quality under load not measured"}),
            );
        }
        let summary = json!({"tool_parser":null,"checks":checks});
        assert!(agentic_summary_matches_options(&summary, &options, 64));

        let mut missing = summary.clone();
        missing["checks"]
            .as_array_mut()
            .unwrap()
            .retain(|check| check["name"] != "responses_continuation");
        assert!(!agentic_summary_matches_options(&missing, &options, 64));
        let mut unsupported = summary.clone();
        let check = unsupported["checks"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|check| check["name"] == "anthropic_authentication")
            .unwrap();
        check["status"] = json!("unsupported");
        assert!(!agentic_summary_matches_options(&unsupported, &options, 64));
        let mut duplicate = summary.clone();
        let check = duplicate["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["name"] == "responses")
            .unwrap()
            .clone();
        duplicate["checks"].as_array_mut().unwrap().push(check);
        assert!(!agentic_summary_matches_options(&duplicate, &options, 64));
        let mut short = summary.clone();
        let check = short["checks"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|check| check["name"] == "responses_context_limit")
            .unwrap();
        check["detail"] = json!("accepted 16 prompt tokens; quality under load not measured");
        assert!(!agentic_summary_matches_options(&short, &options, 64));
        let mut wrong_parser = summary.clone();
        wrong_parser["tool_parser"] = json!("hermes");
        assert!(!agentic_summary_matches_options(
            &wrong_parser,
            &options,
            64
        ));
        let mut tool_options = options.clone();
        tool_options["tool_parser"] = json!("hermes");
        let mut tools = summary.clone();
        tools["tool_parser"] = json!("hermes");
        for check in tools["checks"].as_array_mut().unwrap() {
            if matches!(
                check["name"].as_str(),
                Some("responses_tool_loop" | "anthropic_tool_loop")
            ) {
                check["status"] = json!("passed");
            }
        }
        assert!(agentic_summary_matches_options(&tools, &tool_options, 64));
        let check = tools["checks"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|check| check["name"] == "responses_tool_loop")
            .unwrap();
        check["status"] = json!("unsupported");
        assert!(!agentic_summary_matches_options(&tools, &tool_options, 64));

        let mut continuation_only = options.clone();
        continuation_only["responses"] = json!(false);
        continuation_only["anthropic"] = json!(false);
        let mut continuation_summary = summary;
        continuation_summary["checks"]
            .as_array_mut()
            .unwrap()
            .retain(|check| {
                matches!(
                    check["name"].as_str(),
                    Some("context_limit" | "responses_continuation")
                )
            });
        assert!(agentic_summary_matches_options(
            &continuation_summary,
            &continuation_only,
            64
        ));
    }

    #[test]
    fn refreshed_agentic_candidate_must_match_recorded_chain_and_job() {
        let mut config = functional_config();
        config["responses"] = json!(true);
        let (run, job, evidence) = promotion_fixture(
            RunKind::Validation,
            &config,
            "agentic-cand-0123456789abcdef0123456789abcdef",
            &["/v1/chat/completions", "/v1/responses"],
        );
        let target = binding::target_from_job(&run, &job, "candidate").unwrap();
        let route = promotion_route(&run, &job).unwrap().0;
        let mut current = evidence.prelaunch.clone();
        current.observed_at += chrono::Duration::seconds(1);
        assert!(
            check_refreshed_route(&run, &evidence, &target, route, &current, "service-public")
                .is_ok()
        );

        let mut changed = current.clone();
        changed.route = "service-public".into();
        assert!(
            check_refreshed_route(&run, &evidence, &target, route, &changed, "service-public")
                .is_err()
        );
        let mut wrong_route = evidence.clone();
        wrong_route.prelaunch.route = "service-public".into();
        assert!(check_refreshed_route(
            &run,
            &wrong_route,
            &target,
            route,
            &wrong_route.prelaunch,
            "service-public",
        )
        .is_err());
        let mut wrong_kind = evidence.clone();
        wrong_kind.prelaunch.agentic = false;
        assert!(check_refreshed_route(
            &run,
            &wrong_kind,
            &target,
            route,
            &wrong_kind.prelaunch,
            "service-public",
        )
        .is_err());
        changed = current.clone();
        changed.deployments[0].desired.resolved_dependencies = hash('9');
        assert!(
            check_refreshed_route(&run, &evidence, &target, route, &changed, "service-public")
                .is_err()
        );
        changed = current.clone();
        changed.deployments[0]
            .runtime
            .image_digests
            .insert("agentic-api".into(), hash('9'));
        assert!(
            check_refreshed_route(&run, &evidence, &target, route, &changed, "service-public")
                .is_err()
        );
        let mut with_model = evidence.clone();
        let deployment = &mut with_model.prelaunch.deployments[0];
        deployment.underlying_model = Some(thalamus_runtime::UnderlyingModelIdentity {
            rollout_configuration: deployment.rollout_configuration.clone(),
            runtime: deployment.runtime.clone(),
        });
        with_model.ingestion = with_model.prelaunch.clone();
        let mut changed_model = with_model.prelaunch.clone();
        assert!(check_refreshed_route(
            &run,
            &with_model,
            &target,
            route,
            &changed_model,
            "service-public"
        )
        .is_ok());
        changed_model.deployments[0]
            .underlying_model
            .as_mut()
            .unwrap()
            .runtime
            .model_content = hash('9');
        assert!(check_refreshed_route(
            &run,
            &with_model,
            &target,
            route,
            &changed_model,
            "service-public"
        )
        .is_err());
        changed = current.clone();
        changed.paths.retain(|path| path != "/v1/responses");
        assert!(
            check_refreshed_route(&run, &evidence, &target, route, &changed, "service-public")
                .is_err()
        );
        let mut missing_path = evidence.clone();
        missing_path
            .prelaunch
            .paths
            .retain(|path| path != "/v1/responses");
        assert!(check_refreshed_route(
            &run,
            &missing_path,
            &target,
            route,
            &missing_path.prelaunch,
            "service-public",
        )
        .is_err());

        let mut wrong_endpoint = config.clone();
        wrong_endpoint["base_url"] = json!("https://other.example/v1");
        let wrong_target = binding::target_from_config(
            &RunKind::Validation,
            "service",
            &wrong_endpoint,
            "candidate",
        )
        .unwrap();
        assert!(check_refreshed_route(
            &run,
            &evidence,
            &wrong_target,
            route,
            &current,
            "service-public"
        )
        .is_err());
        let mut wrong_backend = config.clone();
        wrong_backend["expected_backend"] = json!("service/other");
        let wrong_target = binding::target_from_config(
            &RunKind::Validation,
            "service",
            &wrong_backend,
            "candidate",
        )
        .unwrap();
        assert!(check_refreshed_route(
            &run,
            &evidence,
            &wrong_target,
            route,
            &current,
            "service-public"
        )
        .is_err());
        let mut public = config;
        public["suite"] = json!("public");
        let public_target =
            binding::target_from_config(&RunKind::Validation, "service", &public, "public")
                .unwrap();
        assert!(check_refreshed_route(
            &run,
            &evidence,
            &public_target,
            route,
            &current,
            "service-public"
        )
        .is_err());
        let mut other_service = run.clone();
        other_service.request.service = "other".into();
        assert!(check_refreshed_route(
            &other_service,
            &evidence,
            &target,
            route,
            &current,
            "service-public"
        )
        .is_err());
    }

    #[test]
    fn direct_and_benchmark_promotions_keep_their_derived_routes() {
        let (run, job, mut evidence) = promotion_fixture(
            RunKind::Validation,
            &functional_config(),
            "tested-candidate",
            &["/v1/chat/completions"],
        );
        let target = binding::target_from_job(&run, &job, "candidate").unwrap();
        let route = promotion_route(&run, &job).unwrap().0;
        assert!(check_refreshed_route(
            &run,
            &evidence,
            &target,
            route,
            &evidence.prelaunch,
            "service-public"
        )
        .is_ok());
        evidence.prelaunch.route = "service-public".into();
        assert!(check_refreshed_route(
            &run,
            &evidence,
            &target,
            route,
            &evidence.prelaunch,
            "service-public"
        )
        .is_err());

        let config = json!({"base_url":"https://agentic.example/v1","model":"service"});
        let (run, job, mut evidence) = promotion_fixture(
            RunKind::Benchmark,
            &config,
            "tested-benchmark",
            &["/v1/completions"],
        );
        let target = binding::target_from_job(&run, &job, "candidate").unwrap();
        let route = promotion_route(&run, &job).unwrap().0;
        assert_eq!(route, PromotionRoute::Benchmark);
        assert!(check_refreshed_route(
            &run,
            &evidence,
            &target,
            route,
            &evidence.prelaunch,
            "service-public"
        )
        .is_ok());
        evidence.prelaunch.route = "tested-candidate".into();
        assert!(check_refreshed_route(
            &run,
            &evidence,
            &target,
            route,
            &evidence.prelaunch,
            "service-public"
        )
        .is_err());
    }

    #[test]
    fn pinned_patch_carries_the_observed_configuration_to_the_single_stable_deployment() {
        let (source, base, artifacts, hardware, target) = fixture();
        let patches = build_patch(
            &source, &base, &artifacts, &artifacts, &hardware, &hardware, &target,
        )
        .unwrap();
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].path.len(), 2);
        assert_eq!(patches[0].path[0], "deployments");
        assert_eq!(patches[0].path[1], "0");
        let PatchOperation::Set(value) = &patches[0].operation else {
            panic!("expected set")
        };
        assert_eq!(value["name"], "stable");
        assert_eq!(value["role"], "stable");
        assert_eq!(value["weight"], 17);
        assert_eq!(value["image"], source["deployments"][0]["image"]);
        assert_eq!(value["contextLength"], 8192);
        assert_eq!(value["artifact"], "model");
        assert_eq!(
            value["revision"].as_str(),
            deployment_revision(value, &artifacts["model"], &hardware["gpu"]).as_deref()
        );
    }

    #[test]
    fn reject_unobserved_or_changed_identities_and_dependencies() {
        let (source, base, artifacts, hardware, target) = fixture();
        let mut changed = target.clone();
        changed
            .runtime
            .image_digests
            .insert("container/main".into(), hash('9'));
        assert!(build_patch(
            &source, &base, &artifacts, &artifacts, &hardware, &hardware, &changed
        )
        .is_err());
        changed = target.clone();
        changed.runtime.tokenizer_content = hash('9');
        assert!(build_patch(
            &source, &base, &artifacts, &artifacts, &hardware, &hardware, &changed
        )
        .is_err());
        changed = target.clone();
        changed.revision = "9".repeat(64);
        assert!(build_patch(
            &source, &base, &artifacts, &artifacts, &hardware, &hardware, &changed
        )
        .is_err());
        let mut other_artifacts = artifacts.clone();
        other_artifacts["model"]["modelDigest"] = json!(hash('9'));
        assert!(build_patch(
            &source,
            &base,
            &artifacts,
            &other_artifacts,
            &hardware,
            &hardware,
            &target
        )
        .is_err());
        let mut other_hardware = hardware.clone();
        other_hardware["gpu"]["cpu"] = json!("8");
        assert!(build_patch(
            &source,
            &base,
            &artifacts,
            &artifacts,
            &hardware,
            &other_hardware,
            &target
        )
        .is_err());
    }

    #[test]
    fn reject_ambiguous_or_unrelated_base_changes() {
        let (source, mut base, artifacts, hardware, target) = fixture();
        base["namespace"] = json!("different");
        assert!(
            build_patch(&source, &base, &artifacts, &artifacts, &hardware, &hardware, &target)
                .is_err()
        );
        base["namespace"] = json!("serving");
        let mut second = base["deployments"][0].clone();
        second["name"] = json!("other");
        base["deployments"].as_array_mut().unwrap().push(second);
        assert!(
            build_patch(&source, &base, &artifacts, &artifacts, &hardware, &hardware, &target)
                .is_err()
        );
    }

    #[test]
    fn candidate_render_preserves_tested_pod_and_immutable_manifest() {
        fn document(name: &str, max_len: &str, model_digest: &str) -> Value {
            let manifest_name = format!("{name}-manifest");
            let manifest = json!({"image":"registry/engine@sha256:abc", "modelPath":"/opt/thalamus/model",
                "modelDigest":model_digest, "tokenizerDigest":"tokenizer"});
            json!({"items": [
                {"kind":"LLMInferenceService","metadata":{"name":name,"namespace":"serving",
                    "annotations":{"thalamus.ai/model-manifest":manifest_name}},
                    "spec":{"replicas":1,"model":{"uri":"oci://registry/engine@sha256:abc"},
                        "rolloutStrategy":{"maxUnavailable":0},
                        "template":{"nodeSelector":{"accelerator":"gpu"},"containers":[{
                            "name":"main","args":["--max-model-len",max_len]}]}}},
                {"kind":"ConfigMap","metadata":{"name":manifest_name,"namespace":"serving"},
                    "immutable":true,"data":{"manifest.json":manifest.to_string()}}
            ]})
        }
        let source = document("tested", "4096", "model");
        let candidate = document("candidate", "4096", "model");
        let (before, before_template) = rendered_workload(&source, "serving", "tested").unwrap();
        let (after, after_template) =
            rendered_workload(&candidate, "serving", "candidate").unwrap();
        let tested = rendered_identity(&source, "serving", before, before_template).unwrap();
        assert_eq!(
            tested,
            rendered_identity(&candidate, "serving", after, after_template).unwrap()
        );
        let changed = document("candidate", "8192", "model");
        let (workload, template) = rendered_workload(&changed, "serving", "candidate").unwrap();
        assert_ne!(
            tested,
            rendered_identity(&changed, "serving", workload, template).unwrap()
        );
        let changed = document("candidate", "4096", "other");
        let (workload, template) = rendered_workload(&changed, "serving", "candidate").unwrap();
        assert_ne!(
            tested,
            rendered_identity(&changed, "serving", workload, template).unwrap()
        );
    }

    #[test]
    fn flux_revision_and_render_comparisons_fail_closed() {
        let commit = "a".repeat(40);
        assert_eq!(
            flux_commit(&format!("main@sha1:{commit}")),
            Some(commit.as_str())
        );
        assert!(flux_commit("main@sha1:1234").is_none());
        assert!(flux_commit(&format!("main@sha1:{}", "A".repeat(40))).is_none());
        assert!(contains_rendered(
            &json!({"spec":{"args":["a","b"]}}),
            &json!({"spec":{"args":["a","b"],"default":true}})
        ));
        assert!(!contains_rendered(
            &json!({"spec":{"args":["a","b"]}}),
            &json!({"spec":{"args":["a","b","c"]}})
        ));
        assert!(!contains_rendered(
            &json!({"spec":{"args":["a","b"]}}),
            &json!({"spec":{"args":["a","x"]}})
        ));
        let expected = json!({"image":"repo@sha256:abc", "args":["--max-model-len","4096"],
            "env":[{"name":"HOME","value":"/tmp"}]});
        assert!(same_engine(&expected, &expected));
        let mut actual = expected.clone();
        actual["args"][1] = json!("8192");
        assert!(!same_engine(&expected, &actual));
        actual = expected.clone();
        actual["env"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":"EXTRA","value":"1"}));
        assert!(!same_engine(&expected, &actual));
    }
}
