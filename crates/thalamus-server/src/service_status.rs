//! On-demand, independent observations for the five catalog status columns.
//!
//! The authorized catalog is an immutable Git snapshot. Kubernetes and run
//! records are read separately; a request, a desired image, or a previous
//! successful run is never by itself proof of the current serving revision.
use std::{sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use serde_json::Value;
use thalamus_access::{DefaultBranchHead, Policy};
use thalamus_core::StatusValue;
use thalamus_runs::{parse_benchmark_result, Run, RunRequest, Store};
use thalamus_runtime::{
    records::Records, DeploymentSnapshot, LiveObserver, RouteSnapshot, RuntimeConfig,
};
use uuid::Uuid;

use crate::{
    catalog::{CatalogDeployment, CatalogService, ServiceStatus, StatusView},
    AppState,
};

#[path = "service_status_pool.rs"]
mod pool;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn cell(
    value: StatusValue,
    revision: Option<&str>,
    at: Option<DateTime<Utc>>,
    reason: &'static str,
) -> StatusView {
    StatusView {
        state: value,
        revision: revision.map(str::to_owned),
        observed_at: at,
        reason: Some(reason),
    }
}

fn unknown(revision: Option<&str>, reason: &'static str) -> StatusView {
    cell(StatusValue::Unknown, revision, None, reason)
}

fn accepted(revision: &str, head: Option<&DefaultBranchHead>, at: DateTime<Utc>) -> StatusView {
    match head {
        None => unknown(Some(revision), "default branch is unavailable"),
        Some(head) if head.commit == revision => cell(
            StatusValue::Yes,
            Some(revision),
            Some(at),
            "catalog commit is the default branch head",
        ),
        Some(_) => cell(
            StatusValue::Stale,
            Some(revision),
            Some(at),
            "catalog commit has been superseded on the default branch",
        ),
    }
}

// A service is the conjunction of its deployments. A known failure is
// decisive; otherwise an unavailable or superseded deployment is not green.
fn aggregate(cells: Vec<StatusView>, revision: &str) -> StatusView {
    if cells.is_empty() {
        return unknown(Some(revision), "service has no observed deployments");
    }
    for state in [StatusValue::No, StatusValue::Stale, StatusValue::Unknown] {
        if let Some(found) = cells.iter().find(|c| c.state == state) {
            return found.clone();
        }
    }
    let at = cells.iter().filter_map(|c| c.observed_at).min();
    cell(
        StatusValue::Yes,
        Some(revision),
        at,
        "all service deployments have matching observations",
    )
}

fn labels(v: &Value, prefix: &str) -> Option<(String, String)> {
    let labels = v.pointer("/metadata/labels")?.as_object()?;
    let name = labels.get(&format!("{prefix}/name"))?.as_str()?;
    let namespace = labels.get(&format!("{prefix}/namespace"))?.as_str()?;
    if name.is_empty() || namespace.is_empty() {
        return None;
    }
    Some((namespace.to_owned(), name.to_owned()))
}

fn in_flux_inventory(
    kustomization: &Value,
    namespace: &str,
    name: &str,
    group: &str,
    kind: &str,
) -> Option<bool> {
    let entries = kustomization
        .pointer("/status/inventory/entries")?
        .as_array()?;
    let id = format!("{namespace}_{name}_{group}_{kind}");
    Some(
        entries
            .iter()
            .any(|entry| entry.get("id").and_then(Value::as_str) == Some(id.as_str())),
    )
}

// Flux v1 uses branch@sha1:<object-id>; older controllers used branch/<id>.
fn flux_commit(revision: &str) -> Option<&str> {
    let candidate = if let Some((_, id)) = revision.rsplit_once("@sha1:") {
        id
    } else if let Some((_, id)) = revision.rsplit_once("@sha256:") {
        id
    } else if let Some((_, id)) = revision.rsplit_once('/') {
        id
    } else if let Some(id) = revision
        .strip_prefix("sha1:")
        .or_else(|| revision.strip_prefix("sha256:"))
    {
        id
    } else {
        revision
    };
    (matches!(candidate.len(), 40 | 64)
        && candidate
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    .then_some(candidate)
}

fn readiness(object: &Value) -> StatusValue {
    let Some(generation) = object
        .pointer("/metadata/generation")
        .and_then(Value::as_i64)
    else {
        return StatusValue::Unknown;
    };
    let Some(conditions) = object
        .pointer("/status/conditions")
        .and_then(Value::as_array)
    else {
        return StatusValue::Unknown;
    };
    let mut matches = conditions
        .iter()
        .filter(|condition| condition.get("type").and_then(Value::as_str) == Some("Ready"));
    let Some(condition) = matches.next() else {
        return StatusValue::Unknown;
    };
    if matches.next().is_some() {
        return StatusValue::Unknown;
    }
    let observed = condition
        .get("observedGeneration")
        .and_then(Value::as_i64)
        .or_else(|| {
            object
                .pointer("/status/observedGeneration")
                .and_then(Value::as_i64)
        });
    match observed {
        Some(value) if value < generation => return StatusValue::Stale,
        Some(value) if value == generation => {}
        _ => return StatusValue::Unknown,
    }
    match condition.get("status").and_then(Value::as_str) {
        Some("True") => StatusValue::Yes,
        Some("False") => StatusValue::No,
        _ => StatusValue::Unknown,
    }
}

fn flux_applied(
    revision: &str,
    source: &Value,
    kustomization: &Value,
    helm: Option<&Value>,
    at: DateTime<Utc>,
) -> StatusView {
    if source.pointer("/spec/suspend").and_then(Value::as_bool) == Some(true)
        || kustomization
            .pointer("/spec/suspend")
            .and_then(Value::as_bool)
            == Some(true)
        || helm.is_some_and(|v| v.pointer("/spec/suspend").and_then(Value::as_bool) == Some(true))
    {
        return cell(
            StatusValue::Stale,
            Some(revision),
            Some(at),
            "Flux reconciliation is suspended",
        );
    }
    let artifact = source
        .pointer("/status/artifact/revision")
        .and_then(Value::as_str);
    let applied = kustomization
        .pointer("/status/lastAppliedRevision")
        .and_then(Value::as_str);
    match (artifact, applied) {
        (Some(artifact), Some(applied)) => {
            if flux_commit(artifact).is_none() || flux_commit(applied).is_none() {
                return unknown(Some(revision), "Flux revision is unavailable");
            }
            if flux_commit(artifact) != Some(revision)
                || flux_commit(applied) != Some(revision)
                || artifact != applied
            {
                return cell(
                    StatusValue::Stale,
                    Some(revision),
                    Some(at),
                    "Flux is observing or applying another revision",
                );
            }
            match kustomization
                .pointer("/status/lastAttemptedRevision")
                .and_then(Value::as_str)
            {
                Some(attempted) if attempted == applied => {}
                Some(_) => {
                    return cell(
                        StatusValue::Stale,
                        Some(revision),
                        Some(at),
                        "Flux is attempting a different revision",
                    )
                }
                None => return unknown(Some(revision), "Flux attempted revision is unavailable"),
            }
        }
        _ => {
            return unknown(
                Some(revision),
                "Flux source or applied revision is unavailable",
            )
        }
    }
    for object in [Some(source), Some(kustomization), helm]
        .into_iter()
        .flatten()
    {
        match readiness(object) {
            StatusValue::Yes => {}
            StatusValue::No => {
                return cell(
                    StatusValue::No,
                    Some(revision),
                    Some(at),
                    "a Flux reconciler is not ready",
                )
            }
            StatusValue::Stale => {
                return cell(
                    StatusValue::Stale,
                    Some(revision),
                    Some(at),
                    "Flux has not observed the current generation",
                )
            }
            StatusValue::Unknown => {
                return unknown(Some(revision), "Flux readiness is unavailable")
            }
        }
    }
    cell(
        StatusValue::Yes,
        Some(revision),
        Some(at),
        "Flux source and reconcilers applied this commit",
    )
}

fn digest(value: &str) -> Option<&str> {
    let index = value.rfind("sha256:")?;
    let result = &value[index + "sha256:".len()..];
    (result.len() == 64
        && result
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    .then_some(result)
}

fn main_container<'a>(object: &'a Value, pointer: &str) -> Option<&'a Value> {
    let mut matches = object
        .pointer(pointer)?
        .as_array()?
        .iter()
        .filter(|container| container.get("name").and_then(Value::as_str) == Some("main"));
    let result = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    Some(result)
}

fn declared_revision(deployment: &CatalogDeployment) -> Option<&str> {
    let revision = deployment.revision.as_deref()?;
    (revision.len() == 64
        && revision
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    .then_some(revision)
}

fn serving_revision_mismatch(
    deployment: &CatalogDeployment,
    serving: &Value,
    revision: &str,
    at: DateTime<Utc>,
) -> Option<StatusView> {
    let Some(expected) = declared_revision(deployment) else {
        return Some(unknown(
            Some(revision),
            "catalog deployment revision is unavailable",
        ));
    };
    match serving
        .pointer("/metadata/annotations/thalamus.ai~1revision")
        .and_then(Value::as_str)
    {
        Some(actual) if actual == expected => None,
        Some(_) => Some(cell(
            StatusValue::Stale,
            Some(revision),
            Some(at),
            "serving deployment revision differs from the catalog",
        )),
        None => Some(unknown(
            Some(revision),
            "serving deployment revision is unavailable",
        )),
    }
}

fn running_from_pods(
    revision: &str,
    deployment: &CatalogDeployment,
    serving: &Value,
    selector: Option<&Value>,
    pods: Option<&[Value]>,
    at: DateTime<Utc>,
) -> StatusView {
    if let Some(status) = serving_revision_mismatch(deployment, serving, revision, at) {
        return status;
    }
    let raw = deployment.pooled || deployment.rollout_strategy == "Recreate";
    let serving_image_path = if raw {
        "/spec/template/spec/containers"
    } else {
        "/spec/template/containers"
    };
    let desired_image = match deployment
        .image
        .as_deref()
        .filter(|image| !image.is_empty())
    {
        Some(image) => image,
        None => return unknown(Some(revision), "catalog serving image is unavailable"),
    };
    let expected_digest = desired_image
        .rsplit_once("@sha256:")
        .filter(|(repository, _)| !repository.is_empty())
        .and_then(|_| digest(desired_image));
    let current_image = main_container(serving, serving_image_path)
        .and_then(|v| v.get("image"))
        .and_then(Value::as_str);
    match current_image {
        Some(image) if image == desired_image => {}
        Some(_) => {
            return cell(
                StatusValue::Stale,
                Some(revision),
                Some(at),
                "serving image differs from the catalog revision",
            )
        }
        None => return unknown(Some(revision), "serving image is unavailable"),
    }
    let live_replicas = serving
        .pointer("/spec/replicas")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    if !deployment.pooled && live_replicas != u64::from(deployment.replicas) {
        return cell(
            StatusValue::Stale,
            Some(revision),
            Some(at),
            "serving replica count differs from the catalog revision",
        );
    }
    if live_replicas == 0 {
        return cell(
            StatusValue::No,
            Some(revision),
            Some(at),
            "serving deployment has no active replicas",
        );
    }
    let state = if raw {
        let generation = serving
            .pointer("/metadata/generation")
            .and_then(Value::as_i64);
        let observed = serving
            .pointer("/status/observedGeneration")
            .and_then(Value::as_i64);
        match (generation, observed) {
            (Some(generation), Some(observed)) if observed < generation => StatusValue::Stale,
            (Some(generation), Some(observed)) if observed == generation => {
                if serving
                    .pointer("/status/availableReplicas")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    >= live_replicas
                {
                    StatusValue::Yes
                } else {
                    StatusValue::No
                }
            }
            _ => StatusValue::Unknown,
        }
    } else {
        readiness(serving)
    };
    match state {
        StatusValue::No => {
            return cell(
                StatusValue::No,
                Some(revision),
                Some(at),
                "serving resource is not ready",
            )
        }
        StatusValue::Stale => {
            return cell(
                StatusValue::Stale,
                Some(revision),
                Some(at),
                "serving controller has not observed this generation",
            )
        }
        StatusValue::Unknown => return unknown(Some(revision), "serving readiness is unavailable"),
        StatusValue::Yes => {}
    }
    let Some(selector) = selector
        .and_then(Value::as_object)
        .filter(|v| !v.is_empty())
    else {
        return unknown(Some(revision), "serving pod selector is unavailable");
    };
    if selector.values().any(|v| v.as_str().is_none()) {
        return unknown(Some(revision), "serving pod selector is unsupported");
    }
    let Some(pods) = pods else {
        return unknown(Some(revision), "serving pods are unavailable");
    };
    let selected: Vec<_> = pods
        .iter()
        .filter(|pod| {
            if pod
                .pointer("/metadata/deletionTimestamp")
                .is_some_and(|v| !v.is_null())
            {
                return false;
            }
            let Some(labels) = pod.pointer("/metadata/labels").and_then(Value::as_object) else {
                return false;
            };
            selector
                .iter()
                .all(|(key, value)| labels.get(key) == Some(value))
        })
        .collect();
    if (selected.len() as u64) < live_replicas {
        return cell(
            StatusValue::No,
            Some(revision),
            Some(at),
            "not all serving replicas are present",
        );
    }
    for pod in selected {
        let Some(conditions) = pod.pointer("/status/conditions").and_then(Value::as_array) else {
            return unknown(Some(revision), "serving pod readiness is unavailable");
        };
        let mut ready = conditions
            .iter()
            .filter(|c| c.get("type").and_then(Value::as_str) == Some("Ready"));
        let state = ready
            .next()
            .and_then(|c| c.get("status"))
            .and_then(Value::as_str);
        if ready.next().is_some() {
            return unknown(Some(revision), "serving pod readiness is ambiguous");
        }
        match state {
            Some("True") => {}
            Some("False") => {
                return cell(
                    StatusValue::No,
                    Some(revision),
                    Some(at),
                    "a serving pod is not ready",
                )
            }
            _ => return unknown(Some(revision), "serving pod readiness is unavailable"),
        }
        let pod_image = main_container(pod, "/spec/containers")
            .and_then(|v| v.get("image"))
            .and_then(Value::as_str);
        let status = main_container(pod, "/status/containerStatuses");
        let image_id = status
            .and_then(|s| s.get("imageID"))
            .and_then(Value::as_str)
            .and_then(digest);
        if status.and_then(|s| s.get("ready")).and_then(Value::as_bool) == Some(false) {
            return cell(
                StatusValue::No,
                Some(revision),
                Some(at),
                "a serving pod is not ready",
            );
        }
        if pod_image.is_some_and(|image| image != desired_image)
            || matches!((image_id, expected_digest), (Some(actual), Some(expected)) if actual != expected)
        {
            return cell(
                StatusValue::Stale,
                Some(revision),
                Some(at),
                "a serving pod has a different image identity",
            );
        }
        if pod_image.is_none()
            || image_id.is_none()
            || status.and_then(|s| s.get("ready")).and_then(Value::as_bool) != Some(true)
        {
            return unknown(Some(revision), "serving pod image identity is unavailable");
        }
    }
    if expected_digest.is_none() {
        return unknown(
            Some(revision),
            "catalog serving image is not pinned by digest",
        );
    }
    cell(
        StatusValue::Yes,
        Some(revision),
        Some(at),
        "serving replicas are ready with the rendered image digest",
    )
}

async fn get(
    observer: &LiveObserver,
    namespace: &str,
    group: &str,
    version: &str,
    kind: &str,
    plural: &str,
    name: &str,
) -> Option<Value> {
    tokio::time::timeout(
        READ_TIMEOUT,
        observer.get_resource(namespace, group, version, kind, plural, name),
    )
    .await
    .ok()?
    .ok()
}

async fn deployment_observation(
    observer: &LiveObserver,
    config: &RuntimeConfig,
    service: &CatalogService,
    deployment: &CatalogDeployment,
    revision: &str,
) -> (StatusView, StatusView) {
    if service.namespace != config.namespace {
        return (
            unknown(
                Some(revision),
                "runtime namespace does not match the catalog",
            ),
            unknown(
                Some(revision),
                "runtime namespace does not match the catalog",
            ),
        );
    }
    let raw = deployment.pooled || deployment.rollout_strategy == "Recreate";
    if !matches!(
        deployment.rollout_strategy.as_str(),
        "Recreate" | "RollingUpdate"
    ) {
        return (
            unknown(Some(revision), "serving strategy is unsupported"),
            unknown(Some(revision), "serving strategy is unsupported"),
        );
    }
    let (group, version, kind, plural) = if raw {
        ("apps", "v1", "Deployment", "deployments")
    } else {
        (
            "serving.kserve.io",
            config.serving_version.as_str(),
            "LLMInferenceService",
            "llminferenceservices",
        )
    };
    let Some(serving) = get(
        observer,
        &service.namespace,
        group,
        version,
        kind,
        plural,
        &deployment.routing.deployment,
    )
    .await
    else {
        return (
            unknown(Some(revision), "serving resource is unavailable"),
            unknown(Some(revision), "serving resource is unavailable"),
        );
    };
    if let Some(status) = serving_revision_mismatch(deployment, &serving, revision, Utc::now()) {
        return (status.clone(), status);
    }
    let helm_owner = labels(&serving, "helm.toolkit.fluxcd.io");
    let helm = match &helm_owner {
        Some((namespace, name)) => {
            get(
                observer,
                namespace,
                "helm.toolkit.fluxcd.io",
                &config.helm_version,
                "HelmRelease",
                "helmreleases",
                name,
            )
            .await
        }
        None => None,
    };
    let owner = if helm_owner.is_some() {
        helm.as_ref()
            .and_then(|v| labels(v, "kustomize.toolkit.fluxcd.io"))
    } else {
        labels(&serving, "kustomize.toolkit.fluxcd.io")
    };
    let kustomization = match owner {
        Some((namespace, name)) => {
            get(
                observer,
                &namespace,
                "kustomize.toolkit.fluxcd.io",
                &config.flux_version,
                "Kustomization",
                "kustomizations",
                &name,
            )
            .await
        }
        None => None,
    };
    let source = if let Some(kustomization) = &kustomization {
        let source_ref = kustomization.pointer("/spec/sourceRef");
        match source_ref {
            Some(reference)
                if reference.get("kind").and_then(Value::as_str) == Some("GitRepository") =>
            {
                let namespace = reference
                    .get("namespace")
                    .and_then(Value::as_str)
                    .or_else(|| {
                        kustomization
                            .pointer("/metadata/namespace")
                            .and_then(Value::as_str)
                    });
                let name = reference.get("name").and_then(Value::as_str);
                match (namespace, name) {
                    (Some(namespace), Some(name)) => {
                        get(
                            observer,
                            namespace,
                            "source.toolkit.fluxcd.io",
                            &config.source_version,
                            "GitRepository",
                            "gitrepositories",
                            name,
                        )
                        .await
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    } else {
        None
    };
    let now = Utc::now();
    let applied = match (&source, &kustomization) {
        (Some(source), Some(kustomization)) if helm_owner.is_none() || helm.is_some() => {
            let applied = flux_applied(revision, source, kustomization, helm.as_ref(), now);
            if applied.state != StatusValue::Yes {
                applied
            } else {
                let inventory = match &helm_owner {
                    Some((namespace, name)) => in_flux_inventory(
                        kustomization,
                        namespace,
                        name,
                        "helm.toolkit.fluxcd.io",
                        "HelmRelease",
                    ),
                    None => in_flux_inventory(
                        kustomization,
                        &service.namespace,
                        &deployment.routing.deployment,
                        group,
                        kind,
                    ),
                };
                match inventory {
                    Some(true) => applied,
                    Some(false) => cell(
                        StatusValue::No,
                        Some(revision),
                        Some(now),
                        "Flux inventory does not contain the serving resource",
                    ),
                    None => unknown(Some(revision), "Flux inventory is unavailable"),
                }
            }
        }
        _ => unknown(
            Some(revision),
            "Flux ownership or source chain is unavailable",
        ),
    };
    // Read the pod selector from the upstream workload Service, rather than
    // guessing KServe's pod labels. Raw llm-d uses the Deployment selector.
    let service_selector = if raw {
        serving.pointer("/spec/selector/matchLabels").cloned()
    } else {
        get(
            observer,
            &service.namespace,
            "",
            "v1",
            "Service",
            "services",
            &format!("{}-kserve-workload-svc", deployment.routing.deployment),
        )
        .await
        .and_then(|v| v.pointer("/spec/selector").cloned())
    };
    let pods = tokio::time::timeout(READ_TIMEOUT, observer.list_pods(&service.namespace))
        .await
        .ok()
        .and_then(Result::ok);
    let mut running = running_from_pods(
        revision,
        deployment,
        &serving,
        service_selector.as_ref(),
        pods.as_deref(),
        Utc::now(),
    );
    // A ready workload only describes this catalog commit once the Flux chain
    // establishes which revision created the serving resource.
    if matches!(running.state, StatusValue::Yes | StatusValue::No)
        && applied.state != StatusValue::Yes
    {
        running = if applied.state == StatusValue::Stale {
            cell(
                StatusValue::Stale,
                Some(revision),
                applied.observed_at,
                "serving resource is associated with a superseded revision",
            )
        } else {
            unknown(
                Some(revision),
                "serving revision cannot be associated with the catalog commit",
            )
        };
    }
    // Observe the pool independently of workload readiness once the Flux
    // revision is established. A known failure remains decisive, followed by
    // stale and unknown observations, just as for deployment aggregation.
    if applied.state == StatusValue::Yes {
        let pool = pool::observe(
            &pool::Observation {
                observer,
                config,
                service,
                deployment,
                serving: &serving,
                selector: service_selector.as_ref(),
                pods: pods.as_deref(),
                kustomization: kustomization.as_ref(),
                helm_owner: helm_owner.as_ref(),
            },
            revision,
        )
        .await;
        let decisive = [StatusValue::No, StatusValue::Stale, StatusValue::Unknown]
            .into_iter()
            .find(|state| running.state == *state || pool.state == *state);
        running = match decisive {
            Some(state) if running.state == state => running,
            Some(_) => pool,
            None => cell(
                StatusValue::Yes,
                Some(revision),
                running.observed_at.min(pool.observed_at),
                "serving replicas, inference pool and endpoint picker are ready",
            ),
        };
    }
    (applied, running)
}

fn observed_deployment<'a>(
    snapshot: &'a RouteSnapshot,
    name: &str,
) -> Option<&'a DeploymentSnapshot> {
    let mut matches = snapshot
        .deployments
        .iter()
        .filter(|target| target.deployment == name);
    let target = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    Some(target)
}

fn matched_deployment<'a>(
    snapshot: &'a RouteSnapshot,
    name: &str,
    revision: &str,
) -> Option<&'a DeploymentSnapshot> {
    let target = observed_deployment(snapshot, name)?;
    (flux_commit(&target.flux_revision) == Some(revision)).then_some(target)
}

fn engine_argument<'a>(args: &'a [Value], name: &str) -> Option<Option<&'a str>> {
    let mut matches = args
        .iter()
        .enumerate()
        .filter(|(_, value)| value.as_str() == Some(name));
    let Some((index, _)) = matches.next() else {
        return Some(None);
    };
    if matches.next().is_some() {
        return None;
    }
    let value = args.get(index + 1)?.as_str()?;
    if value.is_empty() || value.starts_with("--") {
        return None;
    }
    Some(Some(value))
}

fn validation_result(summary: &Value, run: &Run, target: &DeploymentSnapshot) -> Option<bool> {
    if summary.get("run_id")?.as_str()? != run.id.to_string()
        || serde_json::from_value::<RunRequest>(summary.get("request")?.clone()).ok()?
            != run.request
        || !matches!(summary.get("suite")?.as_str()?, "candidate" | "public")
        || summary.get("runner_version")?.as_str()?.trim().is_empty()
    {
        return None;
    }
    let backends = summary.get("answering_backends")?.as_array()?;
    if backends.len() != 1 || backends[0].as_str()? != target.backend {
        return None;
    }
    let checks = summary.get("checks")?.as_array()?;
    let passed = summary.get("passed")?.as_bool()?;
    if checks.is_empty()
        || checks.iter().any(|c| {
            !matches!(
                c.get("status").and_then(Value::as_str),
                Some("passed" | "failed" | "unsupported")
            )
        })
    {
        return None;
    }
    let has_failed = checks
        .iter()
        .any(|c| c.get("status").and_then(Value::as_str) == Some("failed"));
    if passed == has_failed {
        return None;
    }
    let args = target.engine_spec.get("args")?.as_array()?;
    let tool_parser = engine_argument(args, "--tool-call-parser")?;
    let reasoning = engine_argument(args, "--reasoning-parser")?;
    let tested_tool_parser = match summary.get("tool_parser")? {
        Value::Null => None,
        Value::String(parser) => Some(parser.as_str()),
        _ => return None,
    };
    if tested_tool_parser != tool_parser {
        return None;
    }
    if passed {
        let check = |required: &str| {
            let mut matching = checks
                .iter()
                .filter(|c| c.get("name").and_then(Value::as_str) == Some(required));
            matching
                .next()
                .is_some_and(|c| c.get("status").and_then(Value::as_str) == Some("passed"))
                && matching.next().is_none()
        };
        if [
            "authentication",
            "authorization",
            "chat",
            "streaming",
            "context_limit",
        ]
        .into_iter()
        .any(|name| !check(name))
            || (tool_parser.is_some() && !check("tool_loop"))
            || (reasoning.is_some() && !check("reasoning"))
        {
            return None;
        }
    }
    Some(passed)
}

// A benchmark may survive an unrelated Git commit only when the rendered and
// observed runtime identities, backend and execution conditions remain equal.
// Functional validation is specific to the exact reconciled revision.
fn evidence_matches(
    target: &DeploymentSnapshot,
    recorded: &DeploymentSnapshot,
    kind: &str,
) -> bool {
    if kind != "benchmark" {
        return target == recorded;
    }
    target.deployment == recorded.deployment
        && target.revision == recorded.revision
        && target.backend == recorded.backend
        && target.desired == recorded.desired
        && target.rollout_configuration == recorded.rollout_configuration
        && target.runtime == recorded.runtime
        && target.underlying_model == recorded.underlying_model
        && target.engine_spec == recorded.engine_spec
        && target.nodes == recorded.nodes
}

fn result_status(
    run: &Run,
    target: &DeploymentSnapshot,
    recorded: &DeploymentSnapshot,
    failed: bool,
    bytes: &[u8],
    revision: &str,
    kind: &str,
) -> StatusView {
    if !evidence_matches(target, recorded, kind) {
        return cell(
            StatusValue::Stale,
            Some(revision),
            Some(run.updated_at),
            "latest run observed a different serving revision or runtime",
        );
    }
    if kind == "validation" {
        let summary: Value = match serde_json::from_slice(bytes) {
            Ok(summary) => summary,
            Err(_) => return unknown(Some(revision), "validation result is invalid"),
        };
        match validation_result(&summary, run, target) {
            Some(true) if !failed => cell(
                StatusValue::Yes,
                Some(revision),
                Some(run.updated_at),
                "latest matching validation passed",
            ),
            Some(false) if failed => cell(
                StatusValue::No,
                Some(revision),
                Some(run.updated_at),
                "latest matching validation failed",
            ),
            _ => unknown(
                Some(revision),
                "validation result is incomplete or inconsistent",
            ),
        }
    } else if !failed {
        match parse_benchmark_result(run, bytes) {
            Ok(result) if !result.points.is_empty() => cell(
                StatusValue::Yes,
                Some(revision),
                Some(run.updated_at),
                "a matching benchmark was measured",
            ),
            _ => unknown(
                Some(revision),
                "benchmark result contains no verified measurement",
            ),
        }
    } else {
        unknown(
            Some(revision),
            "latest benchmark did not produce a verified measurement",
        )
    }
}

async fn run_observation(
    state: &AppState,
    config: &RuntimeConfig,
    service: &CatalogService,
    deployment: &CatalogDeployment,
    current: Option<&RouteSnapshot>,
    revision: &str,
    kind: &str,
) -> StatusView {
    let Some(route) = config.routes.get(&service.name) else {
        return unknown(Some(revision), "run evidence scope is unavailable");
    };
    let Some(snapshot) = current else {
        return unknown(Some(revision), "current runtime identity is unavailable");
    };
    let Some(target) = matched_deployment(snapshot, &deployment.routing.deployment, revision)
    else {
        return cell(
            StatusValue::Stale,
            Some(revision),
            Some(snapshot.observed_at),
            "current runtime does not match the catalog revision",
        );
    };
    let Some(expected) = declared_revision(deployment) else {
        return unknown(Some(revision), "catalog deployment revision is unavailable");
    };
    if target.revision != expected {
        return cell(
            StatusValue::Stale,
            Some(revision),
            Some(snapshot.observed_at),
            "current deployment revision differs from the catalog",
        );
    }
    let team = route.team_id.to_string();
    let query = "SELECT id FROM thalamus_runs WHERE team=$1 AND request->>'project_id'=$2 AND request->>'service'=$3 AND request->>'deployment'=$4 AND request->>'kind'=$5 AND ($5 <> 'validation' OR request->'workload'->>'name'='functional') ORDER BY created_at DESC,id DESC LIMIT 1";
    let id = sqlx::query_scalar::<_, Uuid>(query)
        .bind(&team)
        .bind(route.project_id.to_string())
        .bind(&service.name)
        .bind(&deployment.routing.deployment)
        .bind(kind)
        .fetch_optional(&state.db)
        .await;
    let id = match id {
        Ok(Some(id)) => id,
        Ok(None) => return unknown(Some(revision), "no matching run has been recorded"),
        Err(_) => return unknown(Some(revision), "run records are unavailable"),
    };
    let run = match Store::new(state.db.clone()).get(&team, id).await {
        Ok(run) => run,
        Err(_) => return unknown(Some(revision), "run record is unavailable"),
    };
    let records = Records::new(state.db.clone());
    let evidence = if run.state == "succeeded" {
        match records.get_verified(id).await {
            Ok(Some(evidence)) => Some((evidence.ingestion, false)),
            _ => None,
        }
    } else if run.state == "failed" {
        match records.get_failure(id).await {
            Ok(Some(evidence)) => Some((evidence.ingestion, true)),
            _ => None,
        }
    } else {
        None
    };
    let Some((at_run, failed)) = evidence else {
        return unknown(
            Some(revision),
            "latest run has no verified execution evidence",
        );
    };
    let Some(recorded) = at_run
        .deployments
        .iter()
        .find(|item| item.deployment == deployment.routing.deployment)
    else {
        return unknown(Some(revision), "run did not observe this deployment");
    };
    if !evidence_matches(target, recorded, kind) {
        return cell(
            StatusValue::Stale,
            Some(revision),
            Some(run.updated_at),
            "latest run observed a different serving revision or runtime",
        );
    }
    let Some(artifacts) = state.benchmark_artifacts.as_ref() else {
        return unknown(Some(revision), "run artifact store is unavailable");
    };
    let Some(refs) = run.artifacts.as_ref().filter(|refs| refs.len() == 1) else {
        return unknown(Some(revision), "run result artifact is unavailable");
    };
    let bytes = match artifacts.read(id, &refs[0]).await {
        Ok(bytes) => bytes,
        Err(_) => return unknown(Some(revision), "run result artifact is unavailable"),
    };
    let status = result_status(&run, target, recorded, failed, &bytes, revision, kind);
    // A newer pending or failed run must not be hidden by a successful result
    // selected before the artifact read completed.
    match sqlx::query_scalar::<_, Uuid>(query)
        .bind(&team)
        .bind(route.project_id.to_string())
        .bind(&service.name)
        .bind(&deployment.routing.deployment)
        .bind(kind)
        .fetch_optional(&state.db)
        .await
    {
        Ok(Some(latest)) if latest == id => status,
        Ok(_) => unknown(Some(revision), "a newer run superseded this observation"),
        Err(_) => unknown(Some(revision), "run records are unavailable"),
    }
}

fn status_route<'a>(
    deployment: &CatalogDeployment,
    public: Option<&'a RouteSnapshot>,
    candidate: Option<&'a RouteSnapshot>,
) -> Option<&'a RouteSnapshot> {
    match deployment.role.as_deref().unwrap_or("stable") {
        "stable" => public,
        "candidate" => candidate,
        _ => None,
    }
}

/// Collect only for services already authorized by the caller's committed policy.
pub(crate) async fn collect(
    state: &AppState,
    policy: Arc<Policy>,
    services: &[CatalogService],
) -> Vec<ServiceStatus> {
    if services.is_empty() {
        return Vec::new();
    }
    let revision_policy = policy.clone();
    let revision = tokio::task::spawn_blocking(move || {
        revision_policy.committed_revision().ok().map(str::to_owned)
    })
    .await
    .ok()
    .flatten();
    let Some(revision) = revision else {
        return services.iter().map(|_| ServiceStatus::unknown()).collect();
    };
    let branch = tokio::time::timeout(
        READ_TIMEOUT,
        tokio::task::spawn_blocking(move || policy.default_branch_head().ok()),
    )
    .await
    .ok()
    .and_then(Result::ok)
    .flatten();
    let accepted = accepted(&revision, branch.as_ref(), Utc::now());
    let mut result = Vec::with_capacity(services.len());
    for service in services {
        let (Some(observer), Some(config)) = (
            state.runtime_observer.as_deref(),
            state.config.runtime.as_ref(),
        ) else {
            result.push(ServiceStatus {
                accepted: accepted.clone(),
                applied: unknown(Some(&revision), "runtime observer is not configured"),
                running: unknown(Some(&revision), "runtime observer is not configured"),
                functional: unknown(Some(&revision), "runtime observer is not configured"),
                measured: unknown(Some(&revision), "runtime observer is not configured"),
            });
            continue;
        };
        let public = if let Some(routes) = config.routes.get(&service.name) {
            tokio::time::timeout(
                READ_TIMEOUT,
                observer.observe_route(&service.name, &routes.public),
            )
            .await
            .ok()
            .and_then(Result::ok)
        } else {
            None
        };
        let mut applied = Vec::new();
        let mut running = Vec::new();
        let mut functional = Vec::new();
        let mut measured = Vec::new();
        for deployment in &service.deployments {
            let (a, r) =
                deployment_observation(observer, config, service, deployment, &revision).await;
            // These are committed, authorized deployments. A derived candidate
            // route may describe its candidate only; it cannot stand in for
            // observing the configured public route for a stable deployment.
            let candidate = if deployment.role.as_deref() == Some("candidate") {
                tokio::time::timeout(
                    READ_TIMEOUT,
                    observer.observe_candidate(&service.name, &deployment.routing.deployment),
                )
                .await
                .ok()
                .and_then(Result::ok)
            } else {
                None
            };
            let current = status_route(deployment, public.as_ref(), candidate.as_ref());
            applied.push(a);
            running.push(r);
            functional.push(
                run_observation(
                    state,
                    config,
                    service,
                    deployment,
                    current,
                    &revision,
                    "validation",
                )
                .await,
            );
            measured.push(
                run_observation(
                    state,
                    config,
                    service,
                    deployment,
                    current,
                    &revision,
                    "benchmark",
                )
                .await,
            );
        }
        result.push(ServiceStatus {
            accepted: accepted.clone(),
            applied: aggregate(applied, &revision),
            running: aggregate(running, &revision),
            functional: aggregate(functional, &revision),
            measured: aggregate(measured, &revision),
        });
    }
    result
}

#[cfg(test)]
#[path = "service_status_pool_tests.rs"]
mod pool_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;
    use thalamus_core::{DesiredIdentity, RuntimeIdentity, ServiceDeployment};
    use thalamus_runs::{EvidenceIdentity, RunConditions, RunKind, Workload, INFERENCE_PERF_IMAGE};

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
    const OTHER: &str = "abcdef0123456789abcdef0123456789abcdef01";
    const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn at() -> DateTime<Utc> {
        DateTime::from_timestamp(1_720_000_000, 0).unwrap()
    }

    fn deployment() -> CatalogDeployment {
        CatalogDeployment {
            routing: ServiceDeployment {
                deployment: "model-a".into(),
                weight: 1,
            },
            artifact: "weights".into(),
            hardware: "gpu".into(),
            image: Some(format!("registry.invalid/vllm@sha256:{DIGEST}")),
            revision: Some(DIGEST.into()),
            role: None,
            replicas: 1,
            packaged_model: true,
            rollout_strategy: "RollingUpdate".into(),
            pooled: false,
        }
    }

    fn resources() -> (Value, Value, Value, Value, Vec<Value>) {
        let image = deployment().image.unwrap();
        let revision = format!("main@sha1:{COMMIT}");
        let condition = json!([{"type":"Ready","status":"True","observedGeneration":1}]);
        let source = json!({
            "apiVersion":"source.toolkit.fluxcd.io/v1", "kind":"GitRepository",
            "metadata":{"name":"catalog","namespace":"flux-system","generation":1},
            "spec":{"ref":{"branch":"main"}},
            "status":{"artifact":{"revision":revision},"conditions":condition}
        });
        let kustomization = json!({
            "apiVersion":"kustomize.toolkit.fluxcd.io/v1", "kind":"Kustomization",
            "metadata":{"name":"serving","namespace":"flux-system","generation":1},
            "spec":{"sourceRef":{"kind":"GitRepository","name":"catalog"}},
            "status":{"lastAppliedRevision":revision,"lastAttemptedRevision":revision,"conditions":condition,
                "inventory":{"entries":[{"id":"inference_model-a_serving.kserve.io_LLMInferenceService","v":"v1alpha2"}]}}
        });
        let serving = json!({
            "apiVersion":"serving.kserve.io/v1alpha2", "kind":"LLMInferenceService",
            "metadata":{"name":"model-a","namespace":"inference","generation":1,
                "annotations":{"thalamus.ai/revision":DIGEST},"labels":{
                "kustomize.toolkit.fluxcd.io/name":"serving",
                "kustomize.toolkit.fluxcd.io/namespace":"flux-system"
            }},
            "spec":{"replicas":1,"template":{"containers":[{"name":"main","image":image}]}},
            "status":{"conditions":condition}
        });
        let selector = json!({"app.kubernetes.io/name":"model-a","kserve.io/component":"workload"});
        let pods = vec![json!({
            "apiVersion":"v1","kind":"Pod",
            "metadata":{"name":"model-a-1","namespace":"inference","labels":selector},
            "spec":{"containers":[{"name":"main","image":image}]},
            "status":{"conditions":[{"type":"Ready","status":"True"}],
                "containerStatuses":[{"name":"main","ready":true,"imageID":format!("containerd://sha256:{DIGEST}")}]}
        })];
        (source, kustomization, serving, selector, pods)
    }

    fn target() -> DeploymentSnapshot {
        DeploymentSnapshot {
            deployment: "model-a".into(),
            revision: DIGEST.into(),
            flux_revision: format!("main@sha1:{COMMIT}"),
            backend: format!("example/model-a/{DIGEST}"),
            desired: DesiredIdentity {
                rendered_resources: format!("sha256:{}", "b".repeat(64)),
                resolved_dependencies: format!("sha256:{}", "c".repeat(64)),
            },
            rollout_configuration: format!("sha256:{}", "9".repeat(64)),
            runtime: RuntimeIdentity {
                image_digests: BTreeMap::from([(
                    "container/main".into(),
                    format!("sha256:{DIGEST}"),
                )]),
                model_content: format!("sha256:{}", "d".repeat(64)),
                tokenizer_content: format!("sha256:{}", "e".repeat(64)),
                effective_configuration: format!("sha256:{}", "f".repeat(64)),
                component_versions: BTreeMap::from([("engine".into(), format!("sha256:{DIGEST}"))]),
            },
            underlying_model: None,
            engine_spec: json!({"args":["--served-model-name","example"]}),
            nodes: vec!["gpu-node".into()],
        }
    }

    fn run(kind: RunKind) -> Run {
        let id = Uuid::from_u128(1);
        let request = RunRequest {
            project_id: Uuid::from_u128(2),
            kind: kind.clone(),
            service: "example".into(),
            deployment: "model-a".into(),
            runner: "runner".into(),
            workload: Workload {
                name: if kind == RunKind::Validation {
                    "functional"
                } else {
                    "random"
                }
                .into(),
                seed: 42,
                concurrency: 8,
                duration_seconds: 60,
                warmup_seconds: 1,
                max_requests: 100,
                model_budget_tokens: 10_000,
                tool_budget_calls: 0,
            },
            evidence: EvidenceIdentity {
                desired_configuration: format!("sha256:{}", "b".repeat(64)),
                observed_runtime: format!("sha256:{}", "f".repeat(64)),
                test_definition: "test".into(),
                conditions: RunConditions {
                    hardware: "gpu".into(),
                    nodes: vec!["gpu-node".into()],
                    replicas: 1,
                    cache_state: "warm".into(),
                    competing_traffic: "low".into(),
                    observed_at: at(),
                },
            },
        };
        Run {
            id,
            team: "team".into(),
            actor: "actor".into(),
            idempotency_key: "key".into(),
            fingerprint: request.fingerprint().unwrap(),
            request,
            job_name: format!("thalamus-run-{id}"),
            job_uid: Some("job-uid".into()),
            ownership_nonce: Uuid::from_u128(3),
            job_spec: None,
            job_namespace: Some("inference".into()),
            state: "succeeded".into(),
            artifacts: None,
            error: None,
            created_at: at(),
            updated_at: at(),
        }
    }

    fn validation(run: &Run, passed: bool) -> Vec<u8> {
        let checks: Vec<_> = [
            "authentication",
            "authorization",
            "chat",
            "streaming",
            "context_limit",
        ]
        .iter()
        .map(|name| json!({"name":name,"status":"passed"}))
        .collect();
        let mut summary = json!({
            "run_id":run.id, "request":run.request, "suite":"candidate", "runner_version":"1", "tool_parser":null,
            "answering_backends":[format!("example/model-a/{DIGEST}")], "checks":checks, "passed":passed
        });
        if !passed {
            summary["checks"][2]["status"] = json!("failed");
        }
        serde_json::to_vec(&summary).unwrap()
    }

    fn benchmark(run: &mut Run) -> Vec<u8> {
        let image = format!("registry.invalid/runner@sha256:{DIGEST}");
        let tokenizer = json!({"repository":"org/tokenizer","revision":COMMIT});
        let spec = json!({
            "load":{"type":"constant","stages":[{"rate":2,"duration":3}]},
            "data":{"type":"random","input_len":128,"output_len":64},
            "goodput":{"ttft":0.5,"itl":0.1}
        });
        let config = json!({
            "base_url":"https://inference.invalid/v1","model":"example",
            "benchmark":{"tokenizer":tokenizer,"workloads":{"random":spec}}
        });
        run.job_spec = Some(json!({"template":{"spec":{"containers":[{
            "name":"runner","image":image,"env":[
                {"name":"THALAMUS_RUNNER_IMAGE","value":image},
                {"name":"THALAMUS_BENCHMARK_CONFIG","value":config.to_string()}
            ]
        }]}}}));
        serde_json::to_vec(&json!({
            "schema_version":1,"run_id":run.id,"request":run.request,
            "runner":{"name":"inference-perf","version":"0.7.0","adapter_version":"1",
                "image":image,"upstream_image":INFERENCE_PERF_IMAGE},
            "tokenizer":tokenizer,
            "definition":{"name":"random","seed":42,"warmup_seconds":1,"spec":spec},
            "points":[{
                "stage":{"type":"constant","rate":2,"duration":3},
                "successes":5,"failures":1,"achieved_request_rate":1.9,
                "ttft_seconds":{"p50":0.2,"p95":0.4},
                "itl_seconds":{"p50":0.02,"p95":0.04},
                "throughput":{"requests_per_second":1.5,"output_tokens_per_second":31.0},
                "goodput":{"requests_per_second":1.25}
            }]
        }))
        .unwrap()
    }

    #[test]
    fn independent_upstream_and_durable_fixtures_do_not_infer_pool_readiness() {
        let (source, kustomization, _, _, _) = resources();
        let target = target();
        let validation_run = run(RunKind::Validation);
        let mut benchmark_run = run(RunKind::Benchmark);
        let benchmark = benchmark(&mut benchmark_run);
        let status = ServiceStatus {
            accepted: accepted(
                COMMIT,
                Some(&DefaultBranchHead {
                    branch: "main".into(),
                    commit: COMMIT.into(),
                }),
                at(),
            ),
            applied: flux_applied(COMMIT, &source, &kustomization, None, at()),
            running: unknown(Some(COMMIT), "inference pool and picker are not observed"),
            functional: result_status(
                &validation_run,
                &target,
                &target,
                false,
                &validation(&validation_run, true),
                COMMIT,
                "validation",
            ),
            measured: result_status(
                &benchmark_run,
                &target,
                &target,
                false,
                &benchmark,
                COMMIT,
                "benchmark",
            ),
        };
        let view = serde_json::to_value(status).unwrap();
        for column in ["accepted", "applied", "functional", "measured"] {
            assert_eq!(view[column]["state"], "true", "{column}");
            assert_eq!(view[column]["revision"], COMMIT);
            assert!(!view[column]["observed_at"].is_null());
        }
        assert_eq!(view["running"]["state"], "unknown");
    }

    #[test]
    fn flux_revisions_failures_and_freshness_are_distinct() {
        let (source, mut kustomization, _, _, _) = resources();
        assert_eq!(
            in_flux_inventory(
                &kustomization,
                "inference",
                "model-a",
                "serving.kserve.io",
                "LLMInferenceService"
            ),
            Some(true)
        );
        assert_eq!(
            in_flux_inventory(
                &kustomization,
                "inference",
                "other",
                "serving.kserve.io",
                "LLMInferenceService"
            ),
            Some(false)
        );
        assert_eq!(accepted(COMMIT, None, at()).state, StatusValue::Unknown);
        assert_eq!(
            accepted(
                COMMIT,
                Some(&DefaultBranchHead {
                    branch: "main".into(),
                    commit: OTHER.into()
                }),
                at()
            )
            .state,
            StatusValue::Stale
        );
        kustomization["status"]["lastAppliedRevision"] = json!(format!("main@sha1:{OTHER}"));
        assert_eq!(
            flux_applied(COMMIT, &source, &kustomization, None, at()).state,
            StatusValue::Stale
        );
        kustomization["status"]["lastAppliedRevision"] = json!(format!("main@sha1:{COMMIT}"));
        kustomization["status"]["lastAttemptedRevision"] = json!(format!("main@sha1:{OTHER}"));
        assert_eq!(
            flux_applied(COMMIT, &source, &kustomization, None, at()).state,
            StatusValue::Stale
        );
        kustomization["status"]["lastAttemptedRevision"] = json!(format!("main@sha1:{COMMIT}"));
        kustomization["status"]["conditions"][0]["status"] = json!("False");
        assert_eq!(
            flux_applied(COMMIT, &source, &kustomization, None, at()).state,
            StatusValue::No
        );
        kustomization["status"]["conditions"][0]["observedGeneration"] = json!(0);
        assert_eq!(
            flux_applied(COMMIT, &source, &kustomization, None, at()).state,
            StatusValue::Stale
        );
        kustomization["status"]["conditions"] = json!([]);
        assert_eq!(
            flux_applied(COMMIT, &source, &kustomization, None, at()).state,
            StatusValue::Unknown
        );
    }

    #[test]
    fn running_requires_live_ready_pods_and_actual_matching_image_ids() {
        let (_, _, serving, selector, mut pods) = resources();
        assert_eq!(
            running_from_pods(
                COMMIT,
                &deployment(),
                &serving,
                Some(&selector),
                Some(&pods),
                at()
            )
            .state,
            StatusValue::Yes
        );
        let mut superseded = serving.clone();
        superseded["metadata"]["annotations"]["thalamus.ai/revision"] = json!("b".repeat(64));
        assert_eq!(
            running_from_pods(
                COMMIT,
                &deployment(),
                &superseded,
                Some(&selector),
                Some(&pods),
                at()
            )
            .state,
            StatusValue::Stale
        );
        let mut missing_revision = deployment();
        missing_revision.revision = None;
        assert_eq!(
            running_from_pods(
                COMMIT,
                &missing_revision,
                &serving,
                Some(&selector),
                Some(&pods),
                at()
            )
            .state,
            StatusValue::Unknown
        );
        let mut tagged = deployment();
        tagged.image = Some("registry.invalid/vllm:latest".into());
        let mut tagged_serving = serving.clone();
        tagged_serving["spec"]["template"]["containers"][0]["image"] =
            json!("registry.invalid/vllm:latest");
        tagged_serving["status"]["conditions"][0]["status"] = json!("False");
        assert_eq!(
            running_from_pods(
                COMMIT,
                &tagged,
                &tagged_serving,
                Some(&selector),
                None,
                at()
            )
            .state,
            StatusValue::No
        );
        pods[0]["status"]["conditions"] = json!([]);
        assert_eq!(
            running_from_pods(
                COMMIT,
                &deployment(),
                &serving,
                Some(&selector),
                Some(&pods),
                at()
            )
            .state,
            StatusValue::Unknown
        );
        pods[0]["status"]["conditions"] = json!([{"type":"Ready","status":"True"}]);
        pods[0]["status"]["conditions"][0]["status"] = json!("False");
        assert_eq!(
            running_from_pods(
                COMMIT,
                &deployment(),
                &serving,
                Some(&selector),
                Some(&pods),
                at()
            )
            .state,
            StatusValue::No
        );
        pods[0]["status"]["conditions"][0]["status"] = json!("True");
        pods[0]["status"]["containerStatuses"][0]["imageID"] =
            json!(format!("containerd://sha256:{}", "b".repeat(64)));
        assert_eq!(
            running_from_pods(
                COMMIT,
                &deployment(),
                &serving,
                Some(&selector),
                Some(&pods),
                at()
            )
            .state,
            StatusValue::Stale
        );
        pods[0]["status"]["containerStatuses"][0]["imageID"] = Value::Null;
        assert_eq!(
            running_from_pods(
                COMMIT,
                &deployment(),
                &serving,
                Some(&selector),
                Some(&pods),
                at()
            )
            .state,
            StatusValue::Unknown
        );
        assert_eq!(
            running_from_pods(COMMIT, &deployment(), &serving, Some(&selector), None, at()).state,
            StatusValue::Unknown
        );
    }

    #[test]
    fn run_results_are_not_reused_after_runtime_changes_or_without_proof() {
        let run = run(RunKind::Validation);
        let current = target();
        let mut older = current.clone();
        older.runtime.image_digests.insert(
            "container/main".into(),
            format!("sha256:{}", "b".repeat(64)),
        );
        let passed = validation(&run, true);
        assert_eq!(
            result_status(&run, &current, &older, false, &passed, COMMIT, "validation").state,
            StatusValue::Stale
        );
        assert_eq!(
            result_status(
                &run,
                &current,
                &current,
                true,
                &passed,
                COMMIT,
                "validation"
            )
            .state,
            StatusValue::Unknown
        );
        let failed = validation(&run, false);
        assert_eq!(
            result_status(
                &run,
                &current,
                &current,
                true,
                &failed,
                COMMIT,
                "validation"
            )
            .state,
            StatusValue::No
        );
        assert_eq!(
            result_status(&run, &current, &current, false, b"{}", COMMIT, "benchmark").state,
            StatusValue::Unknown
        );
        let independent = aggregate(
            vec![
                cell(StatusValue::Yes, Some(COMMIT), Some(at()), "ready"),
                unknown(Some(COMMIT), "missing"),
            ],
            COMMIT,
        );
        assert_eq!(independent.state, StatusValue::Unknown);
    }

    #[test]
    fn enabled_engine_features_need_matching_successful_probes() {
        let run = run(RunKind::Validation);
        let mut current = target();
        current.engine_spec["args"] = json!([
            "--tool-call-parser",
            "hermes",
            "--reasoning-parser",
            "deepseek"
        ]);
        let mut summary: Value = serde_json::from_slice(&validation(&run, true)).unwrap();
        assert_eq!(validation_result(&summary, &run, &current), None);
        summary["tool_parser"] = json!("hermes");
        summary["checks"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":"tool_loop","status":"passed"}));
        assert_eq!(validation_result(&summary, &run, &current), None);
        summary["checks"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":"reasoning","status":"passed"}));
        assert_eq!(validation_result(&summary, &run, &current), Some(true));
    }

    #[test]
    fn identical_runtime_can_reuse_a_benchmark_across_git_commits() {
        let current = target();
        let mut previous = current.clone();
        previous.flux_revision = format!("main@sha1:{OTHER}");
        let validation_run = run(RunKind::Validation);
        assert_eq!(
            result_status(
                &validation_run,
                &current,
                &previous,
                false,
                &validation(&validation_run, true),
                COMMIT,
                "validation"
            )
            .state,
            StatusValue::Stale
        );
        let mut benchmark_run = run(RunKind::Benchmark);
        let bytes = benchmark(&mut benchmark_run);
        assert_eq!(
            result_status(
                &benchmark_run,
                &current,
                &previous,
                false,
                &bytes,
                COMMIT,
                "benchmark"
            )
            .state,
            StatusValue::Yes
        );
        previous.runtime.model_content = format!("sha256:{}", "1".repeat(64));
        assert_eq!(
            result_status(
                &benchmark_run,
                &current,
                &previous,
                false,
                &bytes,
                COMMIT,
                "benchmark"
            )
            .state,
            StatusValue::Stale
        );
    }

    #[test]
    fn helm_release_and_raw_workload_are_checked_independently() {
        let (source, mut kustomization, _, selector, pods) = resources();
        let mut helm = json!({
            "apiVersion":"helm.toolkit.fluxcd.io/v2","kind":"HelmRelease",
            "metadata":{"name":"inference","namespace":"flux-system","generation":3,"labels":{
                "kustomize.toolkit.fluxcd.io/name":"serving",
                "kustomize.toolkit.fluxcd.io/namespace":"flux-system"
            }},
            "status":{"conditions":[{"type":"Ready","status":"True","observedGeneration":3}]}
        });
        kustomization["status"]["inventory"]["entries"]
            .as_array_mut()
            .unwrap()
            .push(
                json!({"id":"flux-system_inference_helm.toolkit.fluxcd.io_HelmRelease","v":"v2"}),
            );
        assert_eq!(
            labels(&helm, "kustomize.toolkit.fluxcd.io"),
            Some(("flux-system".into(), "serving".into()))
        );
        assert_eq!(
            in_flux_inventory(
                &kustomization,
                "flux-system",
                "inference",
                "helm.toolkit.fluxcd.io",
                "HelmRelease"
            ),
            Some(true)
        );
        assert_eq!(
            flux_applied(COMMIT, &source, &kustomization, Some(&helm), at()).state,
            StatusValue::Yes
        );
        helm["status"]["conditions"][0]["status"] = json!("False");
        assert_eq!(
            flux_applied(COMMIT, &source, &kustomization, Some(&helm), at()).state,
            StatusValue::No
        );

        let mut deployment = deployment();
        deployment.rollout_strategy = "Recreate".into();
        let raw = json!({
            "apiVersion":"apps/v1","kind":"Deployment",
            "metadata":{"name":"model-a","namespace":"inference","generation":2,"annotations":{"thalamus.ai/revision":DIGEST}},
            "spec":{"replicas":1,"selector":{"matchLabels":selector},
                "template":{"spec":{"containers":[{"name":"main","image":deployment.image}]}}},
            "status":{"observedGeneration":2,"availableReplicas":1}
        });
        assert_eq!(
            running_from_pods(
                COMMIT,
                &deployment,
                &raw,
                Some(&selector),
                Some(&pods),
                at()
            )
            .state,
            StatusValue::Yes
        );
        assert_eq!(
            running_from_pods(COMMIT, &deployment, &raw, Some(&selector), None, at()).state,
            StatusValue::Unknown
        );
        // KEDA owns replicas in a shared pool, so compare with the live
        // Deployment's desired replica count instead of the catalog default.
        deployment.pooled = true;
        deployment.rollout_strategy = "RollingUpdate".into();
        let mut scaled = raw.clone();
        scaled["spec"]["replicas"] = json!(2);
        scaled["status"]["availableReplicas"] = json!(2);
        let mut scaled_pods = pods.clone();
        let mut second = scaled_pods[0].clone();
        second["metadata"]["name"] = json!("model-a-2");
        scaled_pods.push(second);
        assert_eq!(
            running_from_pods(
                COMMIT,
                &deployment,
                &scaled,
                Some(&selector),
                Some(&scaled_pods),
                at()
            )
            .state,
            StatusValue::Yes
        );
    }

    #[test]
    fn candidate_observation_cannot_replace_the_public_route() {
        let route = |name: &str| RouteSnapshot {
            observed_at: at(),
            service: "example".into(),
            route: name.into(),
            agentic: false,
            hostnames: vec!["inference.example.invalid".into()],
            paths: vec!["/v1/chat/completions".into()],
            ports: vec![443],
            listeners: vec![],
            weights: BTreeMap::from([("model-a".into(), 1)]),
            deployments: vec![target()],
        };
        let public = route("example");
        let candidate = route("model-a-candidate");
        let mut deployment = deployment();
        assert!(status_route(&deployment, None, Some(&candidate)).is_none());
        assert_eq!(
            status_route(&deployment, Some(&public), Some(&candidate))
                .unwrap()
                .route,
            "example"
        );
        deployment.role = Some("candidate".into());
        assert!(status_route(&deployment, Some(&public), None).is_none());
        assert_eq!(
            status_route(&deployment, Some(&public), Some(&candidate))
                .unwrap()
                .route,
            "model-a-candidate"
        );
        deployment.role = Some("other".into());
        assert!(status_route(&deployment, Some(&public), Some(&candidate)).is_none());
    }
}
