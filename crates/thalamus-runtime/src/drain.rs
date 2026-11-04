//! Point-in-time zero-filler evidence for a separately authorized in-place rollout.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

use crate::observe::{
    LiveResources, annotation, controller_owner, flux_revision, generation, invalid, lookup, name,
    namespace, rollout, selected, selector, string, uid,
};
use crate::{Result, RuntimeConfig};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FillerReplicaSet {
    pub name: String,
    pub uid: String,
    pub desired_replicas: u32,
    pub observed_replicas: u32,
    pub current: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FillerPod {
    pub name: String,
    pub uid: String,
    pub owner_uid: Option<String>,
    pub phase: Option<String>,
    pub terminating: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FillerScaler {
    pub kind: String,
    pub name: String,
    pub uid: String,
}

/// Evidence at a particular time, not a lock or a promise that a scaler cannot
/// subsequently be created. Re-observe immediately before the guarded merge.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FillerState {
    pub observed_at: DateTime<Utc>,
    pub service: String,
    pub deployment: String,
    pub serving_uid: String,
    pub pool: String,
    pub pool_uid: String,
    pub queue: String,
    pub filler: String,
    pub filler_uid: String,
    pub revision: String,
    pub flux_revision: String,
    pub desired_replicas: u32,
    pub observed_replicas: u32,
    pub replicasets: Vec<FillerReplicaSet>,
    pub pods: Vec<FillerPod>,
    pub scalers: Vec<FillerScaler>,
}
impl FillerState {
    /// Conservative instantaneous drain: no scaler, ReplicaSet or Pod can be
    /// inferred away from a zero count on the top-level Deployment alone.
    pub fn verified_zero(&self) -> bool {
        self.desired_replicas == 0
            && self.observed_replicas == 0
            && self
                .replicasets
                .iter()
                .all(|rs| rs.current && rs.desired_replicas == 0 && rs.observed_replicas == 0)
            && self.pods.is_empty()
            && self.scalers.is_empty()
    }
}
fn replicas(v: &Value, pointer: &str, default: u32) -> Result<u32> {
    match v.pointer(pointer) {
        None => Ok(default),
        Some(v) => v
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .ok_or_else(|| invalid(format!("invalid {pointer}"))),
    }
}
fn has_filler_labels(v: &Value, deployment: &str) -> bool {
    v.pointer("/metadata/labels")
        .and_then(Value::as_object)
        .is_some_and(|labels| {
            labels.get("thalamus.ai/deployment").and_then(Value::as_str) == Some(deployment)
                && labels.get("thalamus.ai/pool-role").and_then(Value::as_str) == Some("filler")
        })
}
fn owned_by(v: &Value, kind: &str, owner_uid: &str) -> bool {
    v.pointer("/metadata/ownerReferences")
        .and_then(Value::as_array)
        .is_some_and(|owners| {
            owners.iter().any(|owner| {
                owner.get("controller").and_then(Value::as_bool) == Some(true)
                    && owner.get("kind").and_then(Value::as_str) == Some(kind)
                    && owner.get("uid").and_then(Value::as_str) == Some(owner_uid)
            })
        })
}
fn queue(template: &Value) -> Result<&str> {
    template
        .pointer("/metadata/labels")
        .and_then(Value::as_object)
        .and_then(|m| m.get("kueue.x-k8s.io/queue-name"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid("shared-pool queue label missing"))
}

/// Pure equivalent of the live drain read. Resources must include a complete
/// namespace list of ReplicaSets, Pods, HPAs and KEDA ScaledObjects.
pub fn join_filler_state(
    config: &RuntimeConfig,
    service: &str,
    deployment: &str,
    r: &LiveResources,
    observed_at: DateTime<Utc>,
) -> Result<FillerState> {
    let configured = config
        .routes
        .get(service)
        .ok_or_else(|| invalid("unknown service"))?;
    if configured.service.name != service
        || configured.team_id.is_nil()
        || configured.project_id.is_nil()
    {
        return Err(invalid("invalid service ownership"));
    }
    let ns = config.namespace.as_str();
    let backend = lookup(&r.backends, ns, &format!("{deployment}-backend"))?;
    if annotation(backend, "thalamus.ai/deployment")? != deployment
        || annotation(backend, "thalamus.ai/serving-kind")? != "raw"
        || backend
            .pointer("/spec/ai/provider/custom/model")
            .and_then(Value::as_str)
            != Some(service)
    {
        return Err(invalid(
            "backend is not bound to this raw service deployment",
        ));
    }
    let revision = annotation(backend, "thalamus.ai/revision")?;
    if revision.len() != 64
        || !revision
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(invalid("invalid deployment revision"));
    }
    let header_sets = backend
        .pointer("/spec/policies/transformation/response/set")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("backend response identity missing"))?;
    if header_sets.len() != 1
        || !string(&header_sets[0], "/name")?.eq_ignore_ascii_case("x-thalamus-backend")
    {
        return Err(invalid("backend response identity is ambiguous"));
    }
    let header: String = serde_json::from_str(string(&header_sets[0], "/value")?)
        .map_err(|_| invalid("backend response identity is not a literal"))?;
    if header != format!("{service}/{deployment}/{revision}") {
        return Err(invalid("backend response identity differs from deployment"));
    }
    let reference = backend
        .pointer("/spec/ai/provider/custom/backendRef")
        .ok_or_else(|| invalid("pool reference missing"))?;
    if string(reference, "/group")? != "inference.networking.k8s.io"
        || string(reference, "/kind")? != "InferencePool"
        || reference
            .get("namespace")
            .and_then(Value::as_str)
            .is_some_and(|value| value != ns)
    {
        return Err(invalid("unsupported pool reference"));
    }
    let pool = lookup(&r.pools, ns, string(reference, "/name")?)?;
    let serving = lookup(&r.serving, ns, deployment)?;
    let filler_name = format!("{deployment}-filler");
    let filler = lookup(&r.serving, ns, &filler_name)?;
    if serving.get("kind").and_then(Value::as_str) != Some("Deployment")
        || filler.get("kind").and_then(Value::as_str) != Some("Deployment")
        || annotation(serving, "thalamus.ai/deployment")? != deployment
        || annotation(filler, "thalamus.ai/deployment")? != deployment
        || annotation(serving, "thalamus.ai/revision")? != revision
        || annotation(filler, "thalamus.ai/revision")? != revision
        || annotation(serving, "thalamus.ai/model-manifest")?
            != annotation(filler, "thalamus.ai/model-manifest")?
    {
        return Err(invalid("serving and filler workload identity disagree"));
    }
    rollout(serving, true)?;
    let (flux, deps) = flux_revision(r, serving, backend, pool, true)?;
    let (filler_flux, filler_deps) = flux_revision(r, filler, backend, pool, true)?;
    if filler_flux != flux || filler_deps != deps {
        return Err(invalid("filler Flux provenance differs from serving"));
    }
    let serving_template = serving
        .pointer("/spec/template")
        .ok_or_else(|| invalid("serving template missing"))?;
    let filler_template = filler
        .pointer("/spec/template")
        .ok_or_else(|| invalid("filler template missing"))?;
    let pool_selector = selector(pool)?;
    let normal_labels = serving_template
        .pointer("/metadata/labels")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("normal workload labels missing"))?;
    if normal_labels
        .get("thalamus.ai/deployment")
        .and_then(Value::as_str)
        != Some(deployment)
        || normal_labels
            .get("thalamus.ai/pool-role")
            .and_then(Value::as_str)
            != Some("normal")
    {
        return Err(invalid("normal workload is not bound to this pool member"));
    }
    if !selected(serving_template, &pool_selector)
        || !selected(filler_template, &pool_selector)
        || !has_filler_labels(filler_template, deployment)
    {
        return Err(invalid(
            "pool does not select both serving and filler templates",
        ));
    }
    let labels = filler
        .pointer("/spec/selector/matchLabels")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("filler selector missing"))?;
    if labels.len() != 2
        || labels.get("thalamus.ai/deployment").and_then(Value::as_str) != Some(deployment)
        || labels.get("thalamus.ai/pool-role").and_then(Value::as_str) != Some("filler")
        || filler
            .pointer("/spec/selector/matchExpressions")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty())
    {
        return Err(invalid("filler Deployment selector is ambiguous"));
    }
    let normal_queue = queue(serving_template)?;
    let filler_queue = queue(filler_template)?;
    if filler_queue != format!("{normal_queue}-filler") {
        return Err(invalid("filler queue does not match normal member queue"));
    }
    if filler
        .pointer("/status/observedGeneration")
        .and_then(Value::as_i64)
        != Some(generation(filler)?)
    {
        return Err(invalid("filler Deployment status is stale"));
    }
    let desired_replicas = replicas(filler, "/spec/replicas", 1)?;
    let observed_replicas = replicas(filler, "/status/replicas", 0)?;
    let filler_uid = uid(filler)?;
    let mut replica_sets = Vec::new();
    let mut rs_uids = BTreeSet::new();
    let mut rs_names = BTreeSet::new();
    for rs in &r.replicasets {
        let owned = owned_by(rs, "Deployment", filler_uid);
        if namespace(rs)? != ns || (!owned && !has_filler_labels(rs, deployment)) {
            continue;
        }
        let rs_uid = uid(rs)?;
        let rs_name = name(rs)?;
        if !rs_uids.insert(rs_uid.to_owned()) || !rs_names.insert(rs_name.to_owned()) {
            return Err(invalid("duplicate filler ReplicaSet"));
        }
        replica_sets.push(FillerReplicaSet {
            name: rs_name.to_owned(),
            uid: rs_uid.to_owned(),
            desired_replicas: replicas(rs, "/spec/replicas", 1)?,
            observed_replicas: replicas(rs, "/status/replicas", 0)?,
            current: owned
                && rs
                    .pointer("/status/observedGeneration")
                    .and_then(Value::as_i64)
                    == Some(generation(rs)?),
        });
    }
    replica_sets.sort_by(|a, b| (&a.name, &a.uid).cmp(&(&b.name, &b.uid)));
    let mut pods = Vec::new();
    for pod in &r.pods {
        if namespace(pod)? != ns {
            continue;
        }
        let owner = controller_owner(pod).ok();
        let owner_uid = owner
            .and_then(|owner| owner.get("uid"))
            .and_then(Value::as_str);
        if !has_filler_labels(pod, deployment)
            && !owner_uid.is_some_and(|uid| uid == filler_uid || rs_uids.contains(uid))
        {
            continue;
        }
        pods.push(FillerPod {
            name: name(pod)?.to_owned(),
            uid: uid(pod)?.to_owned(),
            owner_uid: owner_uid.map(str::to_owned),
            phase: pod
                .pointer("/status/phase")
                .and_then(Value::as_str)
                .map(str::to_owned),
            terminating: pod
                .pointer("/metadata/deletionTimestamp")
                .is_some_and(|value| !value.is_null()),
        });
    }
    pods.sort_by(|a, b| (&a.name, &a.uid).cmp(&(&b.name, &b.uid)));
    let mut scalers = Vec::new();
    for scaler in r.horizontal_autoscalers.iter().chain(&r.scaled_objects) {
        if namespace(scaler)? != ns {
            continue;
        }
        let target = string(scaler, "/spec/scaleTargetRef/name")?;
        if target != filler_name.as_str() && !rs_names.contains(target) {
            continue;
        }
        scalers.push(FillerScaler {
            kind: string(scaler, "/kind")?.to_owned(),
            name: name(scaler)?.to_owned(),
            uid: uid(scaler)?.to_owned(),
        });
    }
    scalers.sort_by(|a, b| (&a.kind, &a.name, &a.uid).cmp(&(&b.kind, &b.name, &b.uid)));
    Ok(FillerState {
        observed_at,
        service: service.to_owned(),
        deployment: deployment.to_owned(),
        serving_uid: uid(serving)?.to_owned(),
        pool: name(pool)?.to_owned(),
        pool_uid: uid(pool)?.to_owned(),
        queue: filler_queue.to_owned(),
        filler: filler_name,
        filler_uid: filler_uid.to_owned(),
        revision: revision.to_owned(),
        flux_revision: flux,
        desired_replicas,
        observed_replicas,
        replicasets: replica_sets,
        pods,
        scalers,
    })
}
