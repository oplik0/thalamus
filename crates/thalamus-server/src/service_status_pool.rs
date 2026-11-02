//! Independent, fail-closed observations of the inference pool and its picker.
use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;

use chrono::Utc;
use serde_json::Value;
use thalamus_core::StatusValue;
use thalamus_runtime::{LiveObserver, RuntimeConfig};

use crate::catalog::{CatalogDeployment, CatalogService, StatusView};

use super::{cell, declared_revision, digest, get, in_flux_inventory, unknown, READ_TIMEOUT};

struct Fault(StatusValue, &'static str);
type Check<T> = Result<T, Fault>;

pub(super) struct Observation<'a> {
    pub(super) observer: &'a LiveObserver,
    pub(super) config: &'a RuntimeConfig,
    pub(super) service: &'a CatalogService,
    pub(super) deployment: &'a CatalogDeployment,
    pub(super) serving: &'a Value,
    pub(super) selector: Option<&'a Value>,
    pub(super) pods: Option<&'a [Value]>,
    pub(super) kustomization: Option<&'a Value>,
    pub(super) helm_owner: Option<&'a (String, String)>,
}

fn missing(reason: &'static str) -> Fault {
    Fault(StatusValue::Unknown, reason)
}

fn stale(reason: &'static str) -> Fault {
    Fault(StatusValue::Stale, reason)
}

fn unready(reason: &'static str) -> Fault {
    Fault(StatusValue::No, reason)
}

fn rank(state: StatusValue) -> u8 {
    match state {
        StatusValue::Yes => 0,
        StatusValue::Unknown => 1,
        StatusValue::Stale => 2,
        StatusValue::No => 3,
    }
}

fn combine(first: Check<()>, second: Check<()>) -> Check<()> {
    match (first, second) {
        (Ok(()), result) | (result, Ok(())) => result,
        (Err(first), Err(second)) => {
            if rank(first.0) >= rank(second.0) {
                Err(first)
            } else {
                Err(second)
            }
        }
    }
}

fn text<'a>(value: &'a Value, pointer: &str, reason: &'static str) -> Check<&'a str> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| missing(reason))
}

fn optional_text<'a>(value: &'a Value, key: &str, reason: &'static str) -> Check<Option<&'a str>> {
    match value.get(key) {
        None => Ok(None),
        Some(v) => v
            .as_str()
            .filter(|s| !s.is_empty())
            .map(Some)
            .ok_or_else(|| missing(reason)),
    }
}

fn number(value: &Value) -> Option<u64> {
    value.as_u64().filter(|n| (1..=65535).contains(n))
}

fn alive(value: &Value, reason: &'static str) -> Check<()> {
    match value.pointer("/metadata/deletionTimestamp") {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(_)) => Err(stale(reason)),
        Some(_) => Err(missing(reason)),
    }
}

fn identity<'a>(
    value: &'a Value,
    namespace: &str,
    name: &str,
    kind: &str,
    reason: &'static str,
) -> Check<&'a str> {
    let actual_ns = text(value, "/metadata/namespace", reason)?;
    let actual_name = text(value, "/metadata/name", reason)?;
    let uid = text(value, "/metadata/uid", reason)?;
    if actual_ns != namespace || actual_name != name {
        return Err(stale(reason));
    }
    if let Some(actual) = optional_text(value, "kind", reason)? {
        if actual != kind {
            return Err(stale(reason));
        }
    }
    alive(value, reason)?;
    Ok(uid)
}

fn owner<'a>(value: &'a Value, reason: &'static str) -> Check<&'a Value> {
    let refs = value
        .pointer("/metadata/ownerReferences")
        .and_then(Value::as_array)
        .ok_or_else(|| missing(reason))?;
    // A malformed reference could itself be a controller, so do not discard it.
    if refs
        .iter()
        .any(|r| !r.is_object() || r.get("controller").is_some_and(|v| !v.is_boolean()))
    {
        return Err(missing(reason));
    }
    let mut matches = refs
        .iter()
        .filter(|r| r.get("controller").and_then(Value::as_bool) == Some(true));
    let result = matches.next().ok_or_else(|| missing(reason))?;
    if matches.next().is_some() {
        return Err(missing(reason));
    }
    Ok(result)
}

fn owner_identity<'a>(
    reference: &'a Value,
    api_version: &str,
    kind: &str,
    reason: &'static str,
) -> Check<(&'a str, &'a str)> {
    let api = text(reference, "/apiVersion", reason)?;
    let actual_kind = text(reference, "/kind", reason)?;
    let name = text(reference, "/name", reason)?;
    let uid = text(reference, "/uid", reason)?;
    if api != api_version || actual_kind != kind {
        return Err(stale(reason));
    }
    Ok((name, uid))
}

fn owner_is(
    value: &Value,
    api_version: &str,
    kind: &str,
    name: &str,
    uid: &str,
    reason: &'static str,
) -> Check<()> {
    let (actual_name, actual_uid) =
        owner_identity(owner(value, reason)?, api_version, kind, reason)?;
    if actual_name != name || actual_uid != uid {
        return Err(stale(reason));
    }
    Ok(())
}

fn flux_pair(value: &Value, prefix: &str, reason: &'static str) -> Check<Option<(String, String)>> {
    let Some(map) = value.pointer("/metadata/labels") else {
        return Ok(None);
    };
    let map = map.as_object().ok_or_else(|| missing(reason))?;
    let name = map.get(&format!("{prefix}/name"));
    let namespace = map.get(&format!("{prefix}/namespace"));
    match (namespace, name) {
        (None, None) => Ok(None),
        (Some(ns), Some(name)) => {
            let ns = ns
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| missing(reason))?;
            let name = name
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| missing(reason))?;
            Ok(Some((ns.to_owned(), name.to_owned())))
        }
        _ => Err(missing(reason)),
    }
}

fn flux_binding(
    observation: &Observation<'_>,
    backend: &Value,
    pool: &Value,
    raw: bool,
    backend_name: &str,
    pool_name: &str,
) -> Check<()> {
    const REASON: &str = "inference pool Flux ownership is unavailable or ambiguous";
    const CHANGED: &str = "inference pool Flux ownership differs from serving";
    let serving = observation.serving;
    let helm_owner = observation.helm_owner;
    let namespace = observation.service.namespace.as_str();
    let kustomization = observation.kustomization.ok_or_else(|| missing(REASON))?;
    let flux_ns = text(kustomization, "/metadata/namespace", REASON)?;
    let flux_name = text(kustomization, "/metadata/name", REASON)?;
    let flux = (flux_ns.to_owned(), flux_name.to_owned());
    let serving_helm = flux_pair(serving, "helm.toolkit.fluxcd.io", REASON)?;
    if serving_helm.as_ref() != helm_owner {
        return Err(stale(CHANGED));
    }
    if let Some(helm) = helm_owner {
        for object in [Some(backend), raw.then_some(pool)].into_iter().flatten() {
            let pair = flux_pair(object, "helm.toolkit.fluxcd.io", REASON)?
                .ok_or_else(|| missing(REASON))?;
            if &pair != helm {
                return Err(stale(CHANGED));
            }
        }
        if in_flux_inventory(
            kustomization,
            &helm.0,
            &helm.1,
            "helm.toolkit.fluxcd.io",
            "HelmRelease",
        ) != Some(true)
        {
            return Err(missing(REASON));
        }
    } else {
        let serving_flux = flux_pair(serving, "kustomize.toolkit.fluxcd.io", REASON)?
            .ok_or_else(|| missing(REASON))?;
        if serving_flux != flux {
            return Err(stale(CHANGED));
        }
        for object in [Some(backend), raw.then_some(pool)].into_iter().flatten() {
            if flux_pair(object, "helm.toolkit.fluxcd.io", REASON)?.is_some() {
                return Err(stale(CHANGED));
            }
            let pair = flux_pair(object, "kustomize.toolkit.fluxcd.io", REASON)?
                .ok_or_else(|| missing(REASON))?;
            if pair != flux {
                return Err(stale(CHANGED));
            }
        }
        if in_flux_inventory(
            kustomization,
            namespace,
            backend_name,
            "agentgateway.dev",
            "AgentgatewayBackend",
        ) != Some(true)
            || (raw
                && in_flux_inventory(
                    kustomization,
                    namespace,
                    pool_name,
                    "inference.networking.k8s.io",
                    "InferencePool",
                ) != Some(true))
        {
            return Err(missing(REASON));
        }
    }
    Ok(())
}

// Directly rendered raw objects belong to the same release or inventory as
// the serving Deployment. Controller-created KServe objects use owner UIDs.
fn flux_object(
    observation: &Observation<'_>,
    object: &Value,
    name: &str,
    group: &str,
    kind: &str,
    unavailable: &'static str,
    changed: &'static str,
) -> Check<()> {
    let serving = observation.serving;
    let helm_owner = observation.helm_owner;
    let namespace = observation.service.namespace.as_str();
    let kustomization = observation
        .kustomization
        .ok_or_else(|| missing(unavailable))?;
    if let Some(helm) = helm_owner {
        let serving_owner = flux_pair(serving, "helm.toolkit.fluxcd.io", unavailable)?
            .ok_or_else(|| missing(unavailable))?;
        let object_owner = flux_pair(object, "helm.toolkit.fluxcd.io", unavailable)?
            .ok_or_else(|| missing(unavailable))?;
        if &serving_owner != helm || &object_owner != helm {
            return Err(stale(changed));
        }
        if in_flux_inventory(
            kustomization,
            &helm.0,
            &helm.1,
            "helm.toolkit.fluxcd.io",
            "HelmRelease",
        ) != Some(true)
        {
            return Err(missing(unavailable));
        }
    } else {
        if flux_pair(object, "helm.toolkit.fluxcd.io", unavailable)?.is_some() {
            return Err(stale(changed));
        }
        let serving_owner = flux_pair(serving, "kustomize.toolkit.fluxcd.io", unavailable)?
            .ok_or_else(|| missing(unavailable))?;
        let object_owner = flux_pair(object, "kustomize.toolkit.fluxcd.io", unavailable)?
            .ok_or_else(|| missing(unavailable))?;
        let flux_namespace = text(kustomization, "/metadata/namespace", unavailable)?;
        let flux_name = text(kustomization, "/metadata/name", unavailable)?;
        if object_owner != serving_owner
            || serving_owner.0 != flux_namespace
            || serving_owner.1 != flux_name
        {
            return Err(stale(changed));
        }
        if in_flux_inventory(kustomization, namespace, name, group, kind) != Some(true) {
            return Err(missing(unavailable));
        }
    }
    Ok(())
}

#[derive(PartialEq, Eq)]
struct Parent<'a> {
    group: &'a str,
    kind: &'a str,
    namespace: &'a str,
    name: &'a str,
    section: Option<&'a str>,
    port: Option<u64>,
}

fn parent<'a>(value: &'a Value, namespace: &'a str, reason: &'static str) -> Check<Parent<'a>> {
    let group = match value.get("group") {
        None => "gateway.networking.k8s.io",
        Some(v) => v.as_str().ok_or_else(|| missing(reason))?,
    };
    let kind = optional_text(value, "kind", reason)?.unwrap_or("Gateway");
    let namespace = optional_text(value, "namespace", reason)?.unwrap_or(namespace);
    let name = text(value, "/name", reason)?;
    let section = optional_text(value, "sectionName", reason)?;
    let port = match value.get("port") {
        None => None,
        Some(v) => Some(number(v).ok_or_else(|| missing(reason))?),
    };
    Ok(Parent {
        group,
        kind,
        namespace,
        name,
        section,
        port,
    })
}

fn gateway_parent<'a>(
    route: &'a Value,
    namespace: &'a str,
    reason: &'static str,
) -> Check<Parent<'a>> {
    let parents = route
        .pointer("/spec/parentRefs")
        .and_then(Value::as_array)
        .ok_or_else(|| missing(reason))?;
    if parents.len() != 1 {
        return Err(missing(reason));
    }
    let parent = parent(&parents[0], namespace, reason)?;
    if parent.group != "gateway.networking.k8s.io" || parent.kind != "Gateway" {
        return Err(stale(reason));
    }
    Ok(parent)
}

fn controller_name<'a>(value: &'a Value, reason: &'static str) -> Check<&'a str> {
    let name = text(value, "/controllerName", reason)?;
    let Some((domain, path)) = name.split_once('/') else {
        return Err(missing(reason));
    };
    let valid_domain = !domain.is_empty()
        && domain.split('.').all(|part| {
            !part.is_empty()
                && part.len() <= 63
                && part
                    .as_bytes()
                    .first()
                    .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                && part
                    .as_bytes()
                    .last()
                    .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                && part
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        });
    let valid_path = !path.is_empty()
        && path
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"/-._~%!$&'()*+,;=:".contains(&c));
    if name.len() > 253 || !valid_domain || !valid_path {
        return Err(missing(reason));
    }
    Ok(name)
}

fn parent_status<'a>(
    object: &'a Value,
    namespace: &str,
    expected: &Parent<'_>,
    include_listener: bool,
    reason: &'static str,
) -> Check<&'a Value> {
    let statuses = object
        .pointer("/status/parents")
        .and_then(Value::as_array)
        .ok_or_else(|| missing(reason))?;
    let mut found = None;
    for status in statuses {
        let reference = status.get("parentRef").ok_or_else(|| missing(reason))?;
        let actual = parent(reference, namespace, reason)?;
        if actual.group == expected.group
            && actual.kind == expected.kind
            && actual.name == expected.name
            && actual.namespace == expected.namespace
            && (!include_listener
                || (actual.section == expected.section && actual.port == expected.port))
            && found.replace(status).is_some()
        {
            return Err(missing(reason));
        }
    }
    found.ok_or_else(|| missing(reason))
}

fn conditions(
    object: &Value,
    status: &Value,
    unavailable: &'static str,
    superseded: &'static str,
    not_ready: &'static str,
) -> Check<()> {
    let generation = object
        .pointer("/metadata/generation")
        .and_then(Value::as_i64)
        .filter(|n| *n > 0)
        .ok_or_else(|| missing(unavailable))?;
    let conditions = status
        .get("conditions")
        .and_then(Value::as_array)
        .ok_or_else(|| missing(unavailable))?;
    let mut result = if conditions.iter().any(|condition| {
        condition
            .get("type")
            .and_then(Value::as_str)
            .is_none_or(|s| s.is_empty())
    }) {
        StatusValue::Unknown
    } else {
        StatusValue::Yes
    };
    for ty in ["Accepted", "ResolvedRefs"] {
        let mut found = conditions
            .iter()
            .filter(|condition| condition.get("type").and_then(Value::as_str) == Some(ty));
        let first = found.next();
        let state = match (first, found.next()) {
            (Some(condition), None) => {
                match condition.get("observedGeneration").and_then(Value::as_i64) {
                    Some(n) if n < generation => StatusValue::Stale,
                    Some(n) if n == generation => {
                        match condition.get("status").and_then(Value::as_str) {
                            Some("True")
                                if condition.get("reason").and_then(Value::as_str)
                                    != Some("PartiallyValid") =>
                            {
                                StatusValue::Yes
                            }
                            Some("False") => StatusValue::No,
                            _ => StatusValue::Unknown,
                        }
                    }
                    _ => StatusValue::Unknown,
                }
            }
            _ => StatusValue::Unknown,
        };
        if rank(state) > rank(result) {
            result = state;
        }
    }
    match result {
        StatusValue::Yes => Ok(()),
        StatusValue::No => Err(unready(not_ready)),
        StatusValue::Stale => Err(stale(superseded)),
        StatusValue::Unknown => Err(missing(unavailable)),
    }
}

fn benchmark_ref(route: &Value, pool_name: &str, namespace: &str) -> Check<()> {
    const MISSING: &str = "benchmark route pool reference is unavailable or ambiguous";
    const DIFFERENT: &str = "benchmark route refers to a different inference pool";
    let rules = route
        .pointer("/spec/rules")
        .and_then(Value::as_array)
        .ok_or_else(|| missing(MISSING))?;
    if rules.len() != 1 {
        return Err(missing(MISSING));
    }
    let refs = rules[0]
        .get("backendRefs")
        .and_then(Value::as_array)
        .ok_or_else(|| missing(MISSING))?;
    if refs.len() != 1 {
        return Err(missing(MISSING));
    }
    let reference = &refs[0];
    let group = text(reference, "/group", MISSING)?;
    let kind = text(reference, "/kind", MISSING)?;
    let name = text(reference, "/name", MISSING)?;
    let ns = optional_text(reference, "namespace", MISSING)?.unwrap_or(namespace);
    if group != "inference.networking.k8s.io"
        || kind != "InferencePool"
        || name != pool_name
        || ns != namespace
    {
        return Err(stale(DIFFERENT));
    }
    if let Some(weight) = reference.get("weight") {
        match weight.as_u64() {
            Some(0) => return Err(stale(DIFFERENT)),
            Some(_) => {}
            None => return Err(missing(MISSING)),
        }
    }
    Ok(())
}

fn selector<'a>(
    value: &'a Value,
    nested: bool,
    reason: &'static str,
) -> Check<&'a serde_json::Map<String, Value>> {
    let map = value.as_object().ok_or_else(|| missing(reason))?;
    let labels = if nested && map.contains_key("matchLabels") {
        if map.len() != 1 {
            return Err(missing(reason));
        }
        map.get("matchLabels")
            .and_then(Value::as_object)
            .ok_or_else(|| missing(reason))?
    } else {
        map
    };
    if labels.is_empty()
        || labels
            .iter()
            .any(|(k, v)| k.is_empty() || v.as_str().is_none_or(|s| s.is_empty()))
    {
        return Err(missing(reason));
    }
    Ok(labels)
}

fn selected(
    pod: &Value,
    selector: &serde_json::Map<String, Value>,
    reason: &'static str,
) -> Check<bool> {
    let labels = match pod.pointer("/metadata/labels") {
        None => return Ok(false),
        Some(v) => v.as_object().ok_or_else(|| missing(reason))?,
    };
    Ok(selector
        .iter()
        .all(|(key, value)| labels.get(key) == Some(value)))
}

fn pod_ready(pod: &Value, reason: &'static str, not_ready: &'static str) -> Check<()> {
    alive(pod, reason)?;
    match pod.pointer("/status/phase").and_then(Value::as_str) {
        Some("Running") => {}
        Some("Pending" | "Failed" | "Succeeded") => return Err(unready(not_ready)),
        _ => return Err(missing(reason)),
    }
    let conditions = pod
        .pointer("/status/conditions")
        .and_then(Value::as_array)
        .ok_or_else(|| missing(reason))?;
    for ty in ["Initialized", "Ready"] {
        let mut found = conditions
            .iter()
            .filter(|c| c.get("type").and_then(Value::as_str) == Some(ty));
        let condition = found.next().ok_or_else(|| missing(reason))?;
        if found.next().is_some() {
            return Err(missing(reason));
        }
        match condition.get("status").and_then(Value::as_str) {
            Some("True") => {}
            Some("False") => return Err(unready(not_ready)),
            _ => return Err(missing(reason)),
        }
    }
    Ok(())
}

fn rollout(
    deployment: &Value,
    reason: &'static str,
    superseded: &'static str,
    not_ready: &'static str,
) -> Check<()> {
    let generation = deployment
        .pointer("/metadata/generation")
        .and_then(Value::as_i64)
        .filter(|n| *n > 0)
        .ok_or_else(|| missing(reason))?;
    match deployment
        .pointer("/status/observedGeneration")
        .and_then(Value::as_i64)
    {
        Some(n) if n < generation => return Err(stale(superseded)),
        Some(n) if n == generation => {}
        _ => return Err(missing(reason)),
    }
    let desired = match deployment.pointer("/spec/replicas") {
        None => 1,
        Some(v) => v.as_u64().ok_or_else(|| missing(reason))?,
    };
    if desired == 0 {
        return Err(unready(not_ready));
    }
    for field in [
        "replicas",
        "updatedReplicas",
        "readyReplicas",
        "availableReplicas",
    ] {
        let value = deployment
            .pointer(&format!("/status/{field}"))
            .and_then(Value::as_u64)
            .ok_or_else(|| missing(reason))?;
        if value != desired {
            return Err(unready(not_ready));
        }
    }
    match deployment.pointer("/status/unavailableReplicas") {
        None => {}
        Some(v) if v.as_u64() == Some(0) => {}
        Some(v) if v.as_u64().is_some() => return Err(unready(not_ready)),
        _ => return Err(missing(reason)),
    }
    Ok(())
}

async fn cached_resource<'a>(
    observer: &LiveObserver,
    namespace: &str,
    kind: &str,
    plural: &str,
    name: &str,
    cache: &'a mut BTreeMap<String, Value>,
    reason: &'static str,
) -> Check<&'a Value> {
    if !cache.contains_key(name) {
        let resource = get(observer, namespace, "apps", "v1", kind, plural, name)
            .await
            .ok_or_else(|| missing(reason))?;
        cache.insert(name.to_owned(), resource);
    }
    cache.get(name).ok_or_else(|| missing(reason))
}

async fn pod_workload<'a>(
    observer: &LiveObserver,
    namespace: &str,
    pod: &Value,
    replicasets: &'a mut BTreeMap<String, Value>,
    deployments: &'a mut BTreeMap<String, Value>,
    reason: &'static str,
    changed: &'static str,
) -> Check<(&'a Value, &'a Value)> {
    let (rs_name, rs_uid) = owner_identity(owner(pod, reason)?, "apps/v1", "ReplicaSet", reason)
        .map_err(|e| {
            if e.0 == StatusValue::Stale {
                stale(changed)
            } else {
                e
            }
        })?;
    let rs = cached_resource(
        observer,
        namespace,
        "ReplicaSet",
        "replicasets",
        rs_name,
        replicasets,
        reason,
    )
    .await?;
    let observed_uid = identity(rs, namespace, rs_name, "ReplicaSet", reason)?;
    if observed_uid != rs_uid {
        return Err(stale(changed));
    }
    let (deploy_name, deploy_uid) =
        owner_identity(owner(rs, reason)?, "apps/v1", "Deployment", reason).map_err(|e| {
            if e.0 == StatusValue::Stale {
                stale(changed)
            } else {
                e
            }
        })?;
    let deployment = cached_resource(
        observer,
        namespace,
        "Deployment",
        "deployments",
        deploy_name,
        deployments,
        reason,
    )
    .await?;
    let observed_uid = identity(deployment, namespace, deploy_name, "Deployment", reason)?;
    if observed_uid != deploy_uid {
        return Err(stale(changed));
    }
    // Deployment controllers copy the pod spec into their ReplicaSet. An old
    // ReplicaSet can retain the same Deployment UID after a rollout.
    let rs_template = rs
        .pointer("/spec/template/spec")
        .filter(|v| v.is_object())
        .ok_or_else(|| missing(reason))?;
    let current_template = deployment
        .pointer("/spec/template/spec")
        .filter(|v| v.is_object())
        .ok_or_else(|| missing(reason))?;
    if rs_template != current_template {
        return Err(stale(changed));
    }
    Ok((rs, deployment))
}

fn container<'a>(
    object: &'a Value,
    path: &str,
    name: &str,
    reason: &'static str,
) -> Check<&'a Value> {
    let containers = object
        .pointer(path)
        .and_then(Value::as_array)
        .ok_or_else(|| missing(reason))?;
    let mut matches = containers
        .iter()
        .filter(|c| c.get("name").and_then(Value::as_str) == Some(name));
    let found = matches.next().ok_or_else(|| missing(reason))?;
    if matches.next().is_some() {
        return Err(missing(reason));
    }
    Ok(found)
}

fn model_configuration(pod: &Value, workload: &Value) -> Check<()> {
    const REASON: &str = "serving pod configuration is unavailable or ambiguous";
    let actual = container(pod, "/spec/containers", "main", REASON)?;
    let template = container(workload, "/spec/template/spec/containers", "main", REASON)?;
    text(actual, "/image", REASON)?;
    text(template, "/image", REASON)?;
    for field in ["command", "args"] {
        let expected = template.get(field);
        let observed = actual.get(field);
        for value in [expected, observed].into_iter().flatten() {
            if value
                .as_array()
                .is_none_or(|items| items.iter().any(|item| !item.is_string()))
            {
                return Err(missing(REASON));
            }
        }
        match (expected, observed) {
            (None, None) => {}
            (Some(_), None) => return Err(missing(REASON)),
            (None, Some(_)) => return Err(stale("serving pod configuration is stale")),
            (Some(expected), Some(observed)) if expected != observed => {
                return Err(stale("serving pod configuration is stale"))
            }
            _ => {}
        }
    }
    for field in ["image", "env", "securityContext", "resources"] {
        if let Some(expected) = template.get(field) {
            let observed = actual.get(field).ok_or_else(|| missing(REASON))?;
            if expected != observed {
                return Err(stale("serving pod configuration is stale"));
            }
        }
    }
    Ok(())
}

fn model_port(pod: &Value, target: u64) -> Check<()> {
    const REASON: &str = "inference pool model port evidence is unavailable or ambiguous";
    let main = container(pod, "/spec/containers", "main", REASON)?;
    let ports = main
        .get("ports")
        .and_then(Value::as_array)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| missing(REASON))?;
    let mut count = 0;
    for port in ports {
        let number = port
            .get("containerPort")
            .and_then(number)
            .ok_or_else(|| missing(REASON))?;
        let protocol = optional_text(port, "protocol", REASON)?.unwrap_or("TCP");
        if protocol == "TCP" && number == target {
            count += 1;
        }
    }
    match count {
        1 => Ok(()),
        0 => Err(stale(
            "inference pool target port differs from serving pods",
        )),
        _ => Err(missing(REASON)),
    }
}

fn picker_target<'a>(service_port: &Value, pod: &'a Value) -> Check<(u64, &'a Value)> {
    const REASON: &str = "picker target port is unavailable or ambiguous";
    let target = service_port
        .get("targetPort")
        .or_else(|| service_port.get("port"))
        .ok_or_else(|| missing(REASON))?;
    let target_number = number(target);
    let target_name = target.as_str().filter(|s| !s.is_empty());
    if target_number.is_none() && target_name.is_none() {
        return Err(missing(REASON));
    }
    let containers = pod
        .pointer("/spec/containers")
        .and_then(Value::as_array)
        .ok_or_else(|| missing(REASON))?;
    let mut found = None;
    for container in containers {
        let ports = match container.get("ports") {
            None => continue,
            Some(value) => value.as_array().ok_or_else(|| missing(REASON))?,
        };
        for port in ports {
            let protocol = optional_text(port, "protocol", REASON)?.unwrap_or("TCP");
            if protocol != "TCP" {
                continue;
            }
            let port_number = port
                .get("containerPort")
                .and_then(number)
                .ok_or_else(|| missing(REASON))?;
            if (target_number == Some(port_number)
                || target_name
                    .is_some_and(|name| port.get("name").and_then(Value::as_str) == Some(name)))
                && found.replace((port_number, container)).is_some()
            {
                return Err(missing(REASON));
            }
        }
    }
    found.ok_or_else(|| missing(REASON))
}

fn picker_flags(
    container: &Value,
    pool_name: &str,
    namespace: &str,
    target_port: u64,
) -> Check<()> {
    const REASON: &str = "picker pod pool arguments are unavailable or ambiguous";
    let mut words = Vec::new();
    for key in ["command", "args"] {
        if let Some(value) = container.get(key) {
            let args = value.as_array().ok_or_else(|| missing(REASON))?;
            for word in args {
                words.push(word.as_str().ok_or_else(|| missing(REASON))?);
            }
        }
    }
    let mut pool = None;
    let mut ns = None;
    let mut grpc = None;
    let mut i = 0;
    while i < words.len() {
        let word = words[i];
        let (slot, inline) = if word == "--pool-name" {
            (&mut pool, None)
        } else if word == "--pool-namespace" {
            (&mut ns, None)
        } else if word == "--grpc-port" {
            (&mut grpc, None)
        } else if let Some(value) = word.strip_prefix("--pool-name=") {
            (&mut pool, Some(value))
        } else if let Some(value) = word.strip_prefix("--pool-namespace=") {
            (&mut ns, Some(value))
        } else if let Some(value) = word.strip_prefix("--grpc-port=") {
            (&mut grpc, Some(value))
        } else {
            i += 1;
            continue;
        };
        let value = match inline {
            Some(value) => value,
            None => {
                i += 1;
                *words.get(i).ok_or_else(|| missing(REASON))?
            }
        };
        if value.is_empty() || value.starts_with("--") || slot.replace(value).is_some() {
            return Err(missing(REASON));
        }
        i += 1;
    }
    let pool = pool.ok_or_else(|| missing(REASON))?;
    let ns = ns.ok_or_else(|| missing(REASON))?;
    if pool != pool_name || ns != namespace {
        return Err(stale("picker pod refers to a different inference pool"));
    }
    let value = grpc.ok_or_else(|| missing(REASON))?;
    if value.starts_with('0') || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(missing(REASON));
    }
    let grpc_port = value
        .parse::<u16>()
        .ok()
        .filter(|port| *port > 0)
        .ok_or_else(|| missing(REASON))?;
    if u64::from(grpc_port) != target_port {
        return Err(stale("picker gRPC port differs from its Service target"));
    }
    Ok(())
}

fn picker_container<'a>(
    pod: &'a Value,
    rs: &Value,
    deployment: &Value,
    service_port: &Value,
    pool_name: &str,
    namespace: &str,
) -> Check<(u64, &'a str)> {
    const REASON: &str = "picker pod image or configuration is unavailable or ambiguous";
    let (port, actual) = picker_target(service_port, pod)?;
    let name = text(actual, "/name", REASON)?;
    let image = text(actual, "/image", REASON)?;
    picker_flags(actual, pool_name, namespace, port)?;
    for object in [rs, deployment] {
        let template = container(object, "/spec/template/spec/containers", name, REASON)?;
        let template_image = text(template, "/image", REASON)?;
        if template_image != image
            || template.get("command") != actual.get("command")
            || template.get("args") != actual.get("args")
        {
            return Err(stale("picker pod image or configuration is stale"));
        }
    }
    let status = container(pod, "/status/containerStatuses", name, REASON)?;
    match status.get("ready").and_then(Value::as_bool) {
        Some(true) => {}
        Some(false) => return Err(unready("picker pod is not ready")),
        None => return Err(missing(REASON)),
    }
    let image_id = text(status, "/imageID", REASON)?;
    let actual_digest = digest(image_id).ok_or_else(|| missing(REASON))?;
    if image.contains("@sha256:") && digest(image) != Some(actual_digest) {
        return Err(stale("picker pod image or configuration is stale"));
    }
    Ok((port, actual_digest))
}

fn endpoint_port(slice: &Value, service_port: &Value) -> Check<u64> {
    const REASON: &str = "picker EndpointSlice port is unavailable or ambiguous";
    let ports = slice
        .get("ports")
        .and_then(Value::as_array)
        .ok_or_else(|| missing(REASON))?;
    let service_name = optional_text(service_port, "name", REASON)?;
    let mut found = None;
    for port in ports {
        let name = optional_text(port, "name", REASON)?;
        let protocol = optional_text(port, "protocol", REASON)?.unwrap_or("TCP");
        if name == service_name && protocol == "TCP" {
            let n = port
                .get("port")
                .and_then(number)
                .ok_or_else(|| missing(REASON))?;
            if found.replace(n).is_some() {
                return Err(missing(REASON));
            }
        }
    }
    found.ok_or_else(|| missing(REASON))
}

fn pod_ips(pod: &Value) -> Check<BTreeSet<IpAddr>> {
    const REASON: &str = "picker pod IP is unavailable or ambiguous";
    let mut ips = BTreeSet::new();
    if let Some(value) = pod.pointer("/status/podIP") {
        let ip = value
            .as_str()
            .and_then(|s| s.parse::<IpAddr>().ok())
            .ok_or_else(|| missing(REASON))?;
        if ip.is_unspecified() || ip.is_multicast() {
            return Err(missing(REASON));
        }
        ips.insert(ip);
    }
    if let Some(value) = pod.pointer("/status/podIPs") {
        for entry in value.as_array().ok_or_else(|| missing(REASON))? {
            let ip = text(entry, "/ip", REASON)?
                .parse::<IpAddr>()
                .map_err(|_| missing(REASON))?;
            if ip.is_unspecified() || ip.is_multicast() {
                return Err(missing(REASON));
            }
            ips.insert(ip);
        }
    }
    if ips.is_empty() {
        return Err(missing(REASON));
    }
    Ok(ips)
}

fn endpoint_evidence(
    slices: &[&Value],
    service: &Value,
    service_port: &Value,
    namespace: &str,
    picker_pods: &BTreeMap<&str, (&Value, u64)>,
) -> Check<()> {
    const REASON: &str = "picker EndpointSlice evidence is unavailable or ambiguous";
    const TARGET: &str = "picker EndpointSlice target identity differs from selected pods";
    let service_name = text(service, "/metadata/name", REASON)?;
    let service_uid = text(service, "/metadata/uid", REASON)?;
    let mut reached = BTreeSet::new();
    let mut addresses = BTreeSet::new();
    let mut slice_ids = BTreeSet::new();
    let mut slice_names = BTreeSet::new();
    for slice in slices {
        let slice_name = text(slice, "/metadata/name", REASON)?;
        if !slice_names.insert(slice_name) {
            return Err(missing(REASON));
        }
        let uid = identity(slice, namespace, slice_name, "EndpointSlice", REASON)?;
        if !slice_ids.insert(uid) {
            return Err(missing(REASON));
        }
        if slice
            .pointer("/metadata/labels/endpointslice.kubernetes.io~1managed-by")
            .and_then(Value::as_str)
            != Some("endpointslice-controller.k8s.io")
        {
            return Err(missing(REASON));
        }
        owner_is(slice, "v1", "Service", service_name, service_uid, REASON).map_err(|e| {
            if e.0 == StatusValue::Stale {
                stale(TARGET)
            } else {
                e
            }
        })?;
        let family = text(slice, "/addressType", REASON)?;
        if !matches!(family, "IPv4" | "IPv6") {
            return Err(missing(REASON));
        }
        let endpoints = slice
            .get("endpoints")
            .and_then(Value::as_array)
            .ok_or_else(|| missing(REASON))?;
        if endpoints.is_empty() {
            continue;
        }
        let port = endpoint_port(slice, service_port)?;
        for endpoint in endpoints {
            let reference = endpoint.get("targetRef").ok_or_else(|| missing(REASON))?;
            let uid = text(reference, "/uid", REASON)?;
            let target_name = text(reference, "/name", REASON)?;
            let target_kind = text(reference, "/kind", REASON)?;
            let target_ns = optional_text(reference, "namespace", REASON)?.unwrap_or(namespace);
            let (pod, expected_port) = picker_pods.get(uid).ok_or_else(|| stale(TARGET))?;
            if target_kind != "Pod"
                || target_ns != namespace
                || text(pod, "/metadata/name", REASON)? != target_name
                || *expected_port != port
            {
                return Err(stale(TARGET));
            }
            let conditions = endpoint
                .get("conditions")
                .and_then(Value::as_object)
                .ok_or_else(|| missing(REASON))?;
            let ready = conditions
                .get("ready")
                .and_then(Value::as_bool)
                .ok_or_else(|| missing(REASON))?;
            let terminating = match conditions.get("terminating") {
                None => false,
                Some(v) => v.as_bool().ok_or_else(|| missing(REASON))?,
            };
            let serving = match conditions.get("serving") {
                None => ready,
                Some(v) => v.as_bool().ok_or_else(|| missing(REASON))?,
            };
            if ready && (terminating || !serving) {
                return Err(missing(REASON));
            }
            if !ready && serving {
                return Err(missing(REASON));
            }
            let ips = pod_ips(pod)?;
            let endpoint_addresses = endpoint
                .get("addresses")
                .and_then(Value::as_array)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| missing(REASON))?;
            for value in endpoint_addresses {
                let address = value
                    .as_str()
                    .and_then(|s| s.parse::<IpAddr>().ok())
                    .ok_or_else(|| missing(REASON))?;
                if (family == "IPv4") != address.is_ipv4() || !ips.contains(&address) {
                    return Err(stale(TARGET));
                }
                if !addresses.insert((address, port)) {
                    return Err(missing(REASON));
                }
            }
            if ready {
                reached.insert(uid);
            }
        }
    }
    if reached.is_empty() {
        return Err(unready("picker has no ready endpoints"));
    }
    if reached.len() != picker_pods.len() {
        return Err(missing(
            "picker EndpointSlices do not cover the selected ready pods",
        ));
    }
    Ok(())
}

async fn observe_inner(observation: &Observation<'_>) -> Check<()> {
    let observer = observation.observer;
    let config = observation.config;
    let service = observation.service;
    let deployment = observation.deployment;
    let serving = observation.serving;
    let serving_selector = observation.selector;
    let pods = observation.pods;
    let ns = service.namespace.as_str();
    let deployment_name = deployment.routing.deployment.as_str();
    let raw = deployment.pooled || deployment.rollout_strategy == "Recreate";
    let serving_kind = if raw {
        "Deployment"
    } else {
        "LLMInferenceService"
    };
    let serving_uid = identity(
        serving,
        ns,
        deployment_name,
        serving_kind,
        "serving resource identity is unavailable",
    )?;
    let backend_name = format!("{deployment_name}-backend");
    let backend = get(
        observer,
        ns,
        "agentgateway.dev",
        &config.backend_version,
        "AgentgatewayBackend",
        "agentgatewaybackends",
        &backend_name,
    )
    .await
    .ok_or_else(|| missing("inference backend is unavailable"))?;
    identity(
        &backend,
        ns,
        &backend_name,
        "AgentgatewayBackend",
        "inference backend identity is unavailable",
    )?;
    let expected_revision = declared_revision(deployment)
        .ok_or_else(|| missing("inference backend revision is unavailable"))?;
    for (pointer, expected) in [
        (
            "/metadata/annotations/thalamus.ai~1revision",
            expected_revision,
        ),
        (
            "/metadata/annotations/thalamus.ai~1deployment",
            deployment_name,
        ),
        (
            "/metadata/annotations/thalamus.ai~1serving-kind",
            if raw { "raw" } else { "kserve" },
        ),
        ("/spec/ai/provider/custom/model", service.name.as_str()),
    ] {
        if text(
            &backend,
            pointer,
            "inference backend identity is unavailable",
        )? != expected
        {
            return Err(stale(
                "inference backend identity differs from the serving revision",
            ));
        }
    }
    let provider = backend
        .pointer("/spec/ai/provider")
        .and_then(Value::as_object)
        .ok_or_else(|| missing("inference backend provider is unavailable or ambiguous"))?;
    if provider.len() != 1 || !provider.contains_key("custom") {
        return Err(missing(
            "inference backend provider is unavailable or ambiguous",
        ));
    }
    let pool_ref = backend
        .pointer("/spec/ai/provider/custom/backendRef")
        .ok_or_else(|| missing("inference backend pool reference is unavailable"))?;
    let pool_group = text(
        pool_ref,
        "/group",
        "inference backend pool reference is unavailable",
    )?;
    let pool_kind = text(
        pool_ref,
        "/kind",
        "inference backend pool reference is unavailable",
    )?;
    let pool_name = text(
        pool_ref,
        "/name",
        "inference backend pool reference is unavailable",
    )?;
    let pool_ns = optional_text(
        pool_ref,
        "namespace",
        "inference backend pool reference is unavailable",
    )?
    .unwrap_or(ns);
    if pool_group != "inference.networking.k8s.io" || pool_kind != "InferencePool" || pool_ns != ns
    {
        return Err(stale(
            "inference backend pool reference has a different identity",
        ));
    }
    let pool = get(
        observer,
        ns,
        "inference.networking.k8s.io",
        &config.pool_version,
        "InferencePool",
        "inferencepools",
        pool_name,
    )
    .await
    .ok_or_else(|| missing("inference pool is unavailable"))?;
    identity(
        &pool,
        ns,
        pool_name,
        "InferencePool",
        "inference pool identity is unavailable",
    )?;
    flux_binding(observation, &backend, &pool, raw, &backend_name, pool_name)?;
    if !raw {
        owner_is(
            &pool,
            &format!("serving.kserve.io/{}", config.serving_version),
            "LLMInferenceService",
            deployment_name,
            serving_uid,
            "inference pool controller identity is unavailable",
        )
        .map_err(|e| {
            if e.0 == StatusValue::Stale {
                stale("inference pool controller identity is stale")
            } else {
                e
            }
        })?;
    }

    let benchmark_name = format!("{deployment_name}-benchmark");
    let benchmark = get(
        observer,
        ns,
        "gateway.networking.k8s.io",
        &config.route_version,
        "HTTPRoute",
        "httproutes",
        &benchmark_name,
    )
    .await
    .ok_or_else(|| missing("benchmark route is unavailable"))?;
    identity(
        &benchmark,
        ns,
        &benchmark_name,
        "HTTPRoute",
        "benchmark route identity is unavailable",
    )?;
    benchmark_ref(&benchmark, pool_name, ns)?;
    let benchmark_parent = gateway_parent(
        &benchmark,
        ns,
        "benchmark route parent is unavailable or ambiguous",
    )?;
    let traffic_name = match deployment.role.as_deref().unwrap_or("stable") {
        "stable" => config
            .routes
            .get(&service.name)
            .map(|route| route.public.clone())
            .filter(|name| !name.is_empty())
            .ok_or_else(|| missing("traffic route is not configured"))?,
        "candidate" => format!("{deployment_name}-candidate"),
        _ => return Err(missing("traffic route role is unsupported")),
    };
    let traffic = get(
        observer,
        ns,
        "gateway.networking.k8s.io",
        &config.route_version,
        "HTTPRoute",
        "httproutes",
        &traffic_name,
    )
    .await
    .ok_or_else(|| missing("traffic route is unavailable"))?;
    identity(
        &traffic,
        ns,
        &traffic_name,
        "HTTPRoute",
        "traffic route identity is unavailable",
    )?;
    let traffic_parent = gateway_parent(
        &traffic,
        ns,
        "traffic route parent is unavailable or ambiguous",
    )?;
    if benchmark_parent != traffic_parent {
        return Err(stale(
            "benchmark route parent differs from the traffic route",
        ));
    }
    let traffic_status = parent_status(
        &traffic,
        ns,
        &traffic_parent,
        true,
        "traffic route parent status is unavailable or ambiguous",
    )?;
    let controller = controller_name(traffic_status, "traffic route controller is unavailable")?;
    let traffic_observation = conditions(
        &traffic,
        traffic_status,
        "traffic route parent conditions are unavailable or ambiguous",
        "traffic route parent has not observed this generation",
        "traffic route parent is not ready",
    );
    let pool_observation = (|| {
        let pool_status = parent_status(
            &pool,
            ns,
            &traffic_parent,
            false,
            "inference pool parent status is unavailable or ambiguous",
        )?;
        let pool_controller =
            controller_name(pool_status, "inference pool controller is unavailable")?;
        if pool_controller != controller {
            return Err(stale(
                "inference pool controller differs from traffic route",
            ));
        }
        conditions(
            &pool,
            pool_status,
            "inference pool parent conditions are unavailable or ambiguous",
            "inference pool parent has not observed this generation",
            "inference pool parent is not ready",
        )
    })();
    let parent_observation = combine(traffic_observation, pool_observation);
    if matches!(&parent_observation, Err(Fault(StatusValue::No, _))) {
        return parent_observation;
    }
    let mut replicasets = BTreeMap::new();
    let mut deployments = BTreeMap::new();
    let model_observation = async {
        let pool_selector = selector(
            pool.pointer("/spec/selector")
                .ok_or_else(|| missing("inference pool selector is unavailable"))?,
            true,
            "inference pool selector is unavailable or ambiguous",
        )?;
        let serving_selector = selector(
            serving_selector.ok_or_else(|| missing("serving pod selector is unavailable"))?,
            false,
            "serving pod selector is unavailable or ambiguous",
        )?;
        let pods = pods.ok_or_else(|| missing("serving and picker pods are unavailable"))?;
        let ports = pool
            .pointer("/spec/targetPorts")
            .and_then(Value::as_array)
            .ok_or_else(|| missing("inference pool target port is unavailable or ambiguous"))?;
        if ports.len() != 1 {
            return Err(missing(
                "inference pool target port is unavailable or ambiguous",
            ));
        }
        let target = ports[0]
            .get("number")
            .and_then(number)
            .ok_or_else(|| missing("inference pool target port is unavailable or ambiguous"))?;
        let mut model_uids = BTreeSet::new();
        let mut model_names = BTreeSet::new();
        for pod in pods {
            if !selected(pod, serving_selector, "serving pod labels are unavailable")? {
                continue;
            }
            match pod.pointer("/metadata/deletionTimestamp") {
                None | Some(Value::Null) => {}
                Some(Value::String(_)) => continue,
                _ => return Err(missing("serving pod identity is unavailable")),
            }
            let name = text(pod, "/metadata/name", "serving pod identity is unavailable")?;
            if !model_names.insert(name) {
                return Err(missing("serving pod identity is ambiguous"));
            }
            let uid = identity(pod, ns, name, "Pod", "serving pod identity is unavailable")?;
            if !model_uids.insert(uid) {
                return Err(missing("serving pod identity is ambiguous"));
            }
            if !selected(
                pod,
                pool_selector,
                "inference pool selector evidence is unavailable",
            )? {
                return Err(stale("inference pool does not select a serving pod"));
            }
            model_port(pod, target)?;
            let (_, workload) = pod_workload(
                observer,
                ns,
                pod,
                &mut replicasets,
                &mut deployments,
                "serving pod controller identity is unavailable",
                "serving pod controller identity is stale",
            )
            .await?;
            let workload_name = text(
                workload,
                "/metadata/name",
                "serving pod controller identity is unavailable",
            )?;
            let workload_uid = text(
                workload,
                "/metadata/uid",
                "serving pod controller identity is unavailable",
            )?;
            model_configuration(pod, workload)?;
            if raw {
                if workload_name != deployment_name || workload_uid != serving_uid {
                    return Err(stale("serving pod controller identity is stale"));
                }
                let previous = serving
                    .pointer("/metadata/generation")
                    .and_then(Value::as_i64)
                    .filter(|n| *n > 0)
                    .ok_or_else(|| missing("serving workload generation is unavailable"))?;
                let current = workload
                    .pointer("/metadata/generation")
                    .and_then(Value::as_i64)
                    .filter(|n| *n > 0)
                    .ok_or_else(|| missing("serving workload generation is unavailable"))?;
                if current != previous {
                    return Err(stale("serving workload generation is stale"));
                }
            } else {
                owner_is(
                    workload,
                    &format!("serving.kserve.io/{}", config.serving_version),
                    "LLMInferenceService",
                    deployment_name,
                    serving_uid,
                    "serving pod controller identity is unavailable",
                )
                .map_err(|e| {
                    if e.0 == StatusValue::Stale {
                        stale("serving pod controller identity is stale")
                    } else {
                        e
                    }
                })?;
                rollout(
                    workload,
                    "serving workload rollout is unavailable",
                    "serving workload generation is stale",
                    "serving workload is not ready",
                )?;
            }
        }
        if model_uids.is_empty() {
            return Err(missing("serving pods for inference pool are unavailable"));
        }

        // A broad pool selector also routes to Pods outside the serving selector.
        // Only the separately owned shared-pool filler is an allowed extra member.
        let filler_name = format!("{deployment_name}-filler");
        for pod in pods {
            if !selected(
                pod,
                pool_selector,
                "inference pool selected pod labels are unavailable",
            )? || selected(pod, serving_selector, "serving pod labels are unavailable")?
            {
                continue;
            }
            if !deployment.pooled || !raw {
                return Err(stale("inference pool selects an unrelated pod"));
            }
            let name = text(
                pod,
                "/metadata/name",
                "inference pool selected pod identity is unavailable",
            )?;
            let uid = identity(
                pod,
                ns,
                name,
                "Pod",
                "inference pool selected pod identity is unavailable",
            )?;
            if !model_names.insert(name) || !model_uids.insert(uid) {
                return Err(missing("inference pool selected pod identity is ambiguous"));
            }
            let (_, workload) = pod_workload(
                observer,
                ns,
                pod,
                &mut replicasets,
                &mut deployments,
                "inference pool selected pod controller identity is unavailable",
                "inference pool selected pod controller identity is stale",
            )
            .await?;
            let workload_name = text(
                workload,
                "/metadata/name",
                "inference pool selected pod controller identity is unavailable",
            )?;
            if workload_name != filler_name.as_str() {
                return Err(stale("inference pool selects an unrelated pod"));
            }
            let manifest = text(
                serving,
                "/metadata/annotations/thalamus.ai~1model-manifest",
                "inference pool filler identity is unavailable",
            )?;
            for (pointer, expected) in [
                (
                    "/metadata/annotations/thalamus.ai~1deployment",
                    deployment_name,
                ),
                (
                    "/metadata/annotations/thalamus.ai~1revision",
                    expected_revision,
                ),
                ("/metadata/annotations/thalamus.ai~1serving-kind", "raw"),
                (
                    "/metadata/annotations/thalamus.ai~1model-manifest",
                    manifest,
                ),
            ] {
                if text(
                    workload,
                    pointer,
                    "inference pool filler identity is unavailable",
                )? != expected
                {
                    return Err(stale("inference pool filler identity differs from serving"));
                }
            }
            flux_object(
                observation,
                workload,
                &filler_name,
                "apps",
                "Deployment",
                "inference pool filler Flux ownership is unavailable or ambiguous",
                "inference pool filler Flux ownership differs from serving",
            )?;
            let filler_selector = selector(
                workload
                    .pointer("/spec/selector")
                    .ok_or_else(|| missing("inference pool filler selector is unavailable"))?,
                true,
                "inference pool filler selector is unavailable or ambiguous",
            )?;
            if filler_selector
                .get("thalamus.ai/deployment")
                .and_then(Value::as_str)
                != Some(deployment_name)
                || filler_selector
                    .get("thalamus.ai/pool-role")
                    .and_then(Value::as_str)
                    != Some("filler")
                || !selected(
                    pod,
                    filler_selector,
                    "inference pool filler selector is unavailable",
                )?
            {
                return Err(stale("inference pool filler identity differs from serving"));
            }
            rollout(
                workload,
                "inference pool filler rollout is unavailable",
                "inference pool filler generation is stale",
                "inference pool filler is not ready",
            )?;
            pod_ready(
                pod,
                "inference pool filler pod readiness is unavailable or ambiguous",
                "inference pool filler pod is not ready",
            )?;
            model_port(pod, target)?;
            let image = deployment
                .image
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| missing("inference pool filler image is unavailable"))?;
            let expected_digest = image
                .rsplit_once("@sha256:")
                .filter(|(repository, _)| !repository.is_empty())
                .and_then(|_| digest(image))
                .ok_or_else(|| missing("inference pool filler image is unavailable"))?;
            let actual = container(
                pod,
                "/spec/containers",
                "main",
                "inference pool filler image is unavailable",
            )?;
            if text(
                actual,
                "/image",
                "inference pool filler image is unavailable",
            )? != image
            {
                return Err(stale("inference pool filler image differs from serving"));
            }
            let status = container(
                pod,
                "/status/containerStatuses",
                "main",
                "inference pool filler image is unavailable",
            )?;
            match status.get("ready").and_then(Value::as_bool) {
                Some(true) => {}
                Some(false) => return Err(unready("inference pool filler pod is not ready")),
                None => return Err(missing("inference pool filler image is unavailable")),
            }
            let image_id = text(
                status,
                "/imageID",
                "inference pool filler image is unavailable",
            )?;
            let actual_digest = digest(image_id)
                .ok_or_else(|| missing("inference pool filler image is unavailable"))?;
            if actual_digest != expected_digest {
                return Err(stale("inference pool filler image differs from serving"));
            }
            model_configuration(pod, workload)?;
            model_configuration(pod, serving)?;
        }
        Ok(())
    }
    .await;
    if matches!(&model_observation, Err(Fault(StatusValue::No, _))) {
        return combine(parent_observation, model_observation);
    }
    let picker_observation = async {
        let pods = pods.ok_or_else(|| missing("serving and picker pods are unavailable"))?;

        let reference = pool
            .pointer("/spec/endpointPickerRef")
            .ok_or_else(|| missing("inference pool picker reference is unavailable"))?;
        let picker_name = text(
            reference,
            "/name",
            "inference pool picker reference is unavailable",
        )?;
        let group = match reference.get("group") {
            None => "",
            Some(value) => value
                .as_str()
                .ok_or_else(|| missing("inference pool picker reference is unavailable"))?,
        };
        let kind = optional_text(
            reference,
            "kind",
            "inference pool picker reference is unavailable",
        )?
        .unwrap_or("Service");
        let picker_ns = optional_text(
            reference,
            "namespace",
            "inference pool picker reference is unavailable",
        )?
        .unwrap_or(ns);
        if !group.is_empty() || kind != "Service" || picker_ns != ns {
            return Err(stale(
                "inference pool picker reference has a different identity",
            ));
        }
        let picker_port = reference
            .pointer("/port/number")
            .and_then(number)
            .ok_or_else(|| missing("inference pool picker port is unavailable"))?;
        let picker = get(observer, ns, "", "v1", "Service", "services", picker_name)
            .await
            .ok_or_else(|| missing("picker Service is unavailable"))?;
        identity(
            &picker,
            ns,
            picker_name,
            "Service",
            "picker Service identity is unavailable",
        )?;
        if raw {
            flux_object(
                observation,
                &picker,
                picker_name,
                "",
                "Service",
                "picker Service Flux ownership is unavailable or ambiguous",
                "picker Service Flux ownership differs from serving",
            )?;
        } else {
            owner_is(
                &picker,
                &format!("serving.kserve.io/{}", config.serving_version),
                "LLMInferenceService",
                deployment_name,
                serving_uid,
                "picker Service controller identity is unavailable",
            )
            .map_err(|e| {
                if e.0 == StatusValue::Stale {
                    stale("picker Service controller identity is stale")
                } else {
                    e
                }
            })?;
        }
        match picker.pointer("/spec/type") {
            None => {}
            Some(v) if v.as_str() == Some("ExternalName") => {
                return Err(stale("picker Service is an ExternalName"))
            }
            Some(v) if v.as_str().is_some() => {}
            _ => return Err(missing("picker Service type is unavailable")),
        }
        if picker.pointer("/spec/externalName").is_some() {
            return Err(stale("picker Service is an ExternalName"));
        }
        match picker.pointer("/spec/publishNotReadyAddresses") {
            None | Some(Value::Bool(false)) => {}
            Some(Value::Bool(true)) => {
                return Err(unready("picker Service publishes unready endpoints"))
            }
            _ => return Err(missing("picker Service readiness policy is unavailable")),
        }
        let picker_selector = selector(
            picker
                .pointer("/spec/selector")
                .ok_or_else(|| missing("picker Service selector is unavailable"))?,
            false,
            "picker Service selector is unavailable or ambiguous",
        )?;
        let service_ports = picker
            .pointer("/spec/ports")
            .and_then(Value::as_array)
            .ok_or_else(|| missing("picker Service port is unavailable or ambiguous"))?;
        for port in service_ports {
            if port.get("port").and_then(number).is_none() {
                return Err(missing("picker Service port is unavailable or ambiguous"));
            }
            optional_text(
                port,
                "name",
                "picker Service port is unavailable or ambiguous",
            )?;
            optional_text(
                port,
                "protocol",
                "picker Service port is unavailable or ambiguous",
            )?;
        }
        let mut matches = service_ports
            .iter()
            .filter(|port| port.get("port").and_then(number) == Some(picker_port));
        let service_port = matches
            .next()
            .ok_or_else(|| missing("picker Service port is unavailable or ambiguous"))?;
        if matches.next().is_some()
            || optional_text(
                service_port,
                "protocol",
                "picker Service port is unavailable or ambiguous",
            )?
            .unwrap_or("TCP")
                != "TCP"
        {
            return Err(missing("picker Service port is unavailable or ambiguous"));
        }
        let mut picker_pods = BTreeMap::new();
        let mut picker_names = BTreeSet::new();
        let mut picker_image = None;
        for pod in pods {
            if !selected(pod, picker_selector, "picker pod labels are unavailable")? {
                continue;
            }
            let name = text(pod, "/metadata/name", "picker pod identity is unavailable")?;
            if !picker_names.insert(name) {
                return Err(missing("picker pod identity is ambiguous"));
            }
            let uid = identity(pod, ns, name, "Pod", "picker pod identity is unavailable")?;
            let (rs, workload) = pod_workload(
                observer,
                ns,
                pod,
                &mut replicasets,
                &mut deployments,
                "picker pod controller identity is unavailable",
                "picker pod controller identity is stale",
            )
            .await?;
            if raw {
                let name = text(
                    workload,
                    "/metadata/name",
                    "picker Deployment Flux ownership is unavailable or ambiguous",
                )?;
                flux_object(
                    observation,
                    workload,
                    name,
                    "apps",
                    "Deployment",
                    "picker Deployment Flux ownership is unavailable or ambiguous",
                    "picker Deployment Flux ownership differs from serving",
                )?;
            } else {
                owner_is(
                    workload,
                    &format!("serving.kserve.io/{}", config.serving_version),
                    "LLMInferenceService",
                    deployment_name,
                    serving_uid,
                    "picker Deployment controller identity is unavailable",
                )
                .map_err(|e| {
                    if e.0 == StatusValue::Stale {
                        stale("picker Deployment controller identity is stale")
                    } else {
                        e
                    }
                })?;
            }
            rollout(
                workload,
                "picker Deployment rollout is unavailable",
                "picker Deployment generation is stale",
                "picker Deployment is not ready",
            )?;
            pod_ready(
                pod,
                "picker pod readiness is unavailable or ambiguous",
                "picker pod is not ready",
            )?;
            let (port, image) = picker_container(pod, rs, workload, service_port, pool_name, ns)?;
            if picker_image.is_some_and(|previous| previous != image) {
                return Err(stale("picker pods have different image identities"));
            }
            picker_image = Some(image);
            if picker_pods.insert(uid, (pod, port)).is_some() {
                return Err(missing("picker pod identity is ambiguous"));
            }
        }
        let slices = tokio::time::timeout(READ_TIMEOUT, observer.list_endpoint_slices(ns))
            .await
            .ok()
            .and_then(Result::ok)
            .ok_or_else(|| missing("picker EndpointSlices are unavailable"))?;
        let slices: Vec<_> = slices
            .iter()
            .filter(|slice| {
                slice
                    .pointer("/metadata/labels/kubernetes.io~1service-name")
                    .and_then(Value::as_str)
                    == Some(picker_name)
            })
            .collect();
        if slices.is_empty() {
            return Err(missing("picker EndpointSlices are unavailable"));
        }
        endpoint_evidence(&slices, &picker, service_port, ns, &picker_pods)
    }
    .await;
    combine(
        parent_observation,
        combine(model_observation, picker_observation),
    )
}

pub(super) async fn observe(observation: &Observation<'_>, revision: &str) -> StatusView {
    match observe_inner(observation).await {
        Ok(()) => cell(
            StatusValue::Yes,
            Some(revision),
            Some(Utc::now()),
            "inference pool and endpoint picker are ready",
        ),
        Err(Fault(StatusValue::Unknown, reason)) => unknown(Some(revision), reason),
        Err(Fault(state, reason)) => cell(state, Some(revision), Some(Utc::now()), reason),
    }
}
