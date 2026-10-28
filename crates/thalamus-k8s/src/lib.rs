//! Namespaced read adapters and narrowly scoped worker writes.
//!
//! RBAC and admission must reserve `thalamus-` names for this controller. The
//! managed-by label is an ownership check, not an authentication mechanism.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use futures::Stream;
use k8s_openapi::api::{
    batch::v1::Job,
    coordination::v1::{Lease, LeaseSpec},
    core::v1::ConfigMap,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta};
use kube::{
    Api, Client, ResourceExt,
    api::{ApiResource, DynamicObject, GroupVersionKind, Patch, PatchParams, PostParams},
    runtime::watcher::{self, Event},
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const MANAGER: &str = "thalamus";
pub const MANAGED_BY: &str = "app.kubernetes.io/managed-by";
pub const NAME_PREFIX: &str = "thalamus-";
pub const CONFIG_MAP_MAX_BYTES: usize = 1024 * 1024;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Kubernetes API: {0}")]
    Kube(#[from] kube::Error),
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("object is not managed by thalamus")]
    NotOwned,
    #[error(
        "ConfigMap projection fields are not exclusively owned by thalamus apply; manual remediation is required"
    )]
    UnsafeProjectionOwnership,
    #[error("lease is held by another worker")]
    LeaseHeld,
}

/// The supported read-only CRD families. Versions are configured because
/// installed upstream APIs vary between clusters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadKind {
    GitRepository,
    OciRepository,
    Bucket,
    HelmRepository,
    HelmChart,
    HelmRelease,
    LlmInferenceService,
    InferencePool,
}

impl ReadKind {
    pub fn resource(self, version: &str) -> Result<ApiResource, Error> {
        if version.is_empty() || !version.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(Error::Invalid("invalid API version".into()));
        }
        let (group, kind, plural) = match self {
            Self::GitRepository => (
                "source.toolkit.fluxcd.io",
                "GitRepository",
                "gitrepositories",
            ),
            Self::OciRepository => (
                "source.toolkit.fluxcd.io",
                "OCIRepository",
                "ocirepositories",
            ),
            Self::Bucket => ("source.toolkit.fluxcd.io", "Bucket", "buckets"),
            Self::HelmRepository => (
                "source.toolkit.fluxcd.io",
                "HelmRepository",
                "helmrepositories",
            ),
            Self::HelmChart => ("source.toolkit.fluxcd.io", "HelmChart", "helmcharts"),
            Self::HelmRelease => ("helm.toolkit.fluxcd.io", "HelmRelease", "helmreleases"),
            Self::LlmInferenceService => (
                "serving.kserve.io",
                "LLMInferenceService",
                "llminferenceservices",
            ),
            Self::InferencePool => (
                "inference.networking.k8s.io",
                "InferencePool",
                "inferencepools",
            ),
        };
        Ok(ApiResource::from_gvk_with_plural(
            &GroupVersionKind::gvk(group, version, kind),
            plural,
        ))
    }
}

#[derive(Clone)]
pub struct K8s {
    client: Client,
    namespace: String,
}

impl K8s {
    pub fn new(client: Client, namespace: impl Into<String>) -> Result<Self, Error> {
        let namespace = namespace.into();
        if !dns_label(&namespace) {
            return Err(Error::Invalid("invalid namespace".into()));
        }
        Ok(Self { client, namespace })
    }

    fn dynamic(&self, kind: ReadKind, version: &str) -> Result<Api<DynamicObject>, Error> {
        Ok(Api::namespaced_with(
            self.client.clone(),
            &self.namespace,
            &kind.resource(version)?,
        ))
    }

    pub async fn get(
        &self,
        kind: ReadKind,
        version: &str,
        name: &str,
    ) -> Result<Option<DynamicObject>, Error> {
        Ok(self.dynamic(kind, version)?.get_opt(name).await?)
    }

    /// Drive this stream continuously and feed every event to a WatchState.
    /// On an error or end of stream invalidate the state and restart the watch
    /// to obtain a complete relist. Quiet streams eventually become stale;
    /// restart them periodically to refresh even when no resources change.
    pub fn watch(
        &self,
        kind: ReadKind,
        version: &str,
    ) -> Result<impl Stream<Item = Result<Event<DynamicObject>, watcher::Error>> + use<>, Error>
    {
        Ok(watcher::watcher(
            self.dynamic(kind, version)?,
            watcher::Config::default(),
        ))
    }

    /// Create an empty reserved ConfigMap, then apply data without forcing
    /// field ownership. Creation may succeed even if apply fails. A
    /// resourceVersion precondition guards the checked object against races.
    pub async fn apply_projection(
        &self,
        name: &str,
        data: BTreeMap<String, String>,
    ) -> Result<ConfigMap, Error> {
        self.apply_projection_with_labels(name, data, BTreeMap::new())
            .await
    }

    /// Apply a projection with additional selector labels owned by this manager.
    /// The mandatory ownership label cannot be supplied or overridden.
    pub async fn apply_projection_with_labels(
        &self,
        name: &str,
        data: BTreeMap<String, String>,
        labels: BTreeMap<String, String>,
    ) -> Result<ConfigMap, Error> {
        validate_projection_labels(&labels)?;
        let mut desired = projection(name, data)?;
        desired
            .metadata
            .labels
            .as_mut()
            .expect("projection labels")
            .extend(labels);
        let api: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.namespace);
        match api.get_opt(name).await? {
            None => {
                let empty = ConfigMap {
                    metadata: ObjectMeta {
                        name: desired.metadata.name.clone(),
                        labels: Some(owned_labels()),
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let created = api
                    .create(
                        &PostParams {
                            field_manager: Some(MANAGER.into()),
                            ..Default::default()
                        },
                        &empty,
                    )
                    .await?;
                desired.metadata.resource_version = Some(
                    created
                        .metadata
                        .resource_version
                        .ok_or_else(|| Error::Invalid("missing resourceVersion".into()))?,
                );
                Ok(api
                    .patch(name, &PatchParams::apply(MANAGER), &Patch::Apply(&desired))
                    .await?)
            }
            Some(existing) => {
                ensure_owned(&existing.metadata)?;
                ensure_projection_data_owned(&existing)?;
                ensure_projection_labels_owned(
                    &existing,
                    desired.metadata.labels.as_ref().expect("projection labels"),
                )?;
                desired.metadata.resource_version = Some(
                    existing
                        .metadata
                        .resource_version
                        .ok_or_else(|| Error::Invalid("missing resourceVersion".into()))?,
                );
                Ok(api
                    .patch(name, &PatchParams::apply(MANAGER), &Patch::Apply(&desired))
                    .await?)
            }
        }
    }

    /// Create only, with a stable run-derived name. A 409 must be resolved by
    /// reading and comparing the existing Job's run identity and specification.
    pub async fn create_job(&self, mut job: Job) -> Result<Job, Error> {
        let name = job
            .metadata
            .name
            .as_deref()
            .ok_or_else(|| Error::Invalid("Job requires a name".into()))?;
        validate_name(name)?;
        if job
            .metadata
            .namespace
            .as_deref()
            .is_some_and(|n| n != self.namespace)
        {
            return Err(Error::Invalid("Job namespace mismatch".into()));
        }
        if job
            .spec
            .as_ref()
            .and_then(|s| s.ttl_seconds_after_finished)
            .is_none_or(|v| v <= 0)
        {
            return Err(Error::Invalid("Job requires a positive TTL".into()));
        }
        set_owned(&mut job.metadata)?;
        let api: Api<Job> = Api::namespaced(self.client.clone(), &self.namespace);
        Ok(api.create(&PostParams::default(), &job).await?)
    }

    pub async fn get_job(&self, name: &str) -> Result<Option<Job>, Error> {
        validate_name(name)?;
        let api: Api<Job> = Api::namespaced(self.client.clone(), &self.namespace);
        let job = api.get_opt(name).await?;
        if let Some(j) = &job {
            ensure_owned(&j.metadata)?;
        }
        Ok(job)
    }

    /// Acquire or renew using resourceVersion compare-and-swap. Conflicts must
    /// be retried by re-reading. Time must come from a reasonably synced clock.
    pub async fn acquire_lease(
        &self,
        name: &str,
        holder: &str,
        duration_seconds: i32,
        now: DateTime<Utc>,
    ) -> Result<Lease, Error> {
        validate_name(name)?;
        if holder.is_empty() || duration_seconds <= 0 {
            return Err(Error::Invalid(
                "holder and positive duration required".into(),
            ));
        }
        let api: Api<Lease> = Api::namespaced(self.client.clone(), &self.namespace);
        match api.get_opt(name).await? {
            None => {
                let lease = Lease {
                    metadata: ObjectMeta {
                        name: Some(name.into()),
                        labels: Some(owned_labels()),
                        ..Default::default()
                    },
                    spec: Some(LeaseSpec {
                        holder_identity: Some(holder.into()),
                        lease_duration_seconds: Some(duration_seconds),
                        acquire_time: Some(MicroTime(now)),
                        renew_time: Some(MicroTime(now)),
                        lease_transitions: Some(0),
                    }),
                };
                Ok(api.create(&PostParams::default(), &lease).await?)
            }
            Some(mut lease) => {
                ensure_owned(&lease.metadata)?;
                if lease.metadata.resource_version.is_none() {
                    return Err(Error::Invalid("missing resourceVersion".into()));
                }
                update_lease(&mut lease, holder, duration_seconds, now)?;
                Ok(api.replace(name, &PostParams::default(), &lease).await?)
            }
        }
    }

    pub async fn release_lease(&self, name: &str, holder: &str) -> Result<Lease, Error> {
        validate_name(name)?;
        let api: Api<Lease> = Api::namespaced(self.client.clone(), &self.namespace);
        let mut lease = api.get(name).await?;
        ensure_owned(&lease.metadata)?;
        if lease.metadata.resource_version.is_none() {
            return Err(Error::Invalid("missing resourceVersion".into()));
        }
        let spec = lease.spec.as_mut().ok_or(Error::LeaseHeld)?;
        if holder.is_empty() || spec.holder_identity.as_deref() != Some(holder) {
            return Err(Error::LeaseHeld);
        }
        spec.holder_identity = None;
        spec.renew_time = None;
        Ok(api.replace(name, &PostParams::default(), &lease).await?)
    }
}

fn update_lease(
    lease: &mut Lease,
    holder: &str,
    duration: i32,
    now: DateTime<Utc>,
) -> Result<(), Error> {
    let spec = lease.spec.get_or_insert_with(LeaseSpec::default);
    let previous = spec.holder_identity.as_deref().filter(|s| !s.is_empty());
    if previous.is_some_and(|p| p != holder) {
        // Missing/invalid time or duration is unknown, never safe to steal.
        let expires = spec
            .renew_time
            .as_ref()
            .or(spec.acquire_time.as_ref())
            .and_then(|t| {
                let seconds = spec.lease_duration_seconds.filter(|d| *d > 0)?;
                t.0.checked_add_signed(chrono::Duration::seconds(i64::from(seconds)))
            });
        if expires.is_none_or(|t| now < t) {
            return Err(Error::LeaseHeld);
        }
    }
    if previous != Some(holder) {
        spec.acquire_time = Some(MicroTime(now));
        spec.lease_transitions = Some(
            spec.lease_transitions
                .unwrap_or(0)
                .checked_add(1)
                .ok_or_else(|| Error::Invalid("lease transitions overflow".into()))?,
        );
    }
    spec.holder_identity = Some(holder.into());
    spec.lease_duration_seconds = Some(duration);
    spec.renew_time = Some(MicroTime(now));
    Ok(())
}

fn dns_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value.as_bytes()[value.len() - 1].is_ascii_alphanumeric()
}
fn validate_name(name: &str) -> Result<(), Error> {
    if name.starts_with(NAME_PREFIX) && dns_label(name) && name.len() > NAME_PREFIX.len() {
        Ok(())
    } else {
        Err(Error::Invalid(
            "name must be a DNS label with reserved thalamus- prefix".into(),
        ))
    }
}
fn owned_labels() -> BTreeMap<String, String> {
    BTreeMap::from([(MANAGED_BY.into(), MANAGER.into())])
}
fn ensure_owned(meta: &ObjectMeta) -> Result<(), Error> {
    if meta
        .labels
        .as_ref()
        .and_then(|l| l.get(MANAGED_BY))
        .map(String::as_str)
        == Some(MANAGER)
    {
        Ok(())
    } else {
        Err(Error::NotOwned)
    }
}

// An apply omission deletes a key only when that apply manager is its sole
// owner. Legacy POSTs could leave an Update owner alongside the Apply owner.
fn ensure_projection_data_owned(config_map: &ConfigMap) -> Result<(), Error> {
    let Some(data) = config_map.data.as_ref().filter(|data| !data.is_empty()) else {
        return Ok(());
    };
    let mut owned = std::collections::BTreeSet::new();
    for entry in config_map
        .metadata
        .managed_fields
        .as_deref()
        .unwrap_or_default()
    {
        if entry.subresource.as_deref().is_some_and(|s| !s.is_empty()) {
            continue;
        }
        if entry.fields_type.as_deref() != Some("FieldsV1") {
            return Err(Error::UnsafeProjectionOwnership);
        }
        let fields = entry
            .fields_v1
            .as_ref()
            .and_then(|fields| fields.0.as_object())
            .ok_or(Error::UnsafeProjectionOwnership)?;
        let Some(data_fields) = fields.get("f:data") else {
            continue;
        };
        let data_fields = data_fields
            .as_object()
            .ok_or(Error::UnsafeProjectionOwnership)?;
        let is_apply_owner = entry.manager.as_deref() == Some(MANAGER)
            && entry.operation.as_deref() == Some("Apply");
        // An empty field set owns the entire field, not merely the map itself.
        if data_fields.is_empty() {
            if !is_apply_owner {
                return Err(Error::UnsafeProjectionOwnership);
            }
            owned.extend(data.keys().map(String::as_str));
            continue;
        }
        for field in data_fields.keys() {
            if field == "." {
                continue;
            }
            let key = field
                .strip_prefix("f:")
                .ok_or(Error::UnsafeProjectionOwnership)?;
            if data.contains_key(key) {
                if !is_apply_owner {
                    return Err(Error::UnsafeProjectionOwnership);
                }
                owned.insert(key);
            }
        }
    }
    if owned.len() != data.len() {
        return Err(Error::UnsafeProjectionOwnership);
    }
    Ok(())
}
fn label_part(value: &str, allow_empty: bool) -> bool {
    if value.is_empty() {
        return allow_empty;
    }
    value.len() <= 63
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value.as_bytes()[value.len() - 1].is_ascii_alphanumeric()
}

fn validate_projection_labels(labels: &BTreeMap<String, String>) -> Result<(), Error> {
    for (key, value) in labels {
        let valid_key = match key.split_once('/') {
            Some((prefix, name)) => {
                prefix.len() <= 253 && prefix.split('.').all(dns_label) && label_part(name, false)
            }
            None => label_part(key, false),
        };
        if key == MANAGED_BY || !valid_key || !label_part(value, true) {
            return Err(Error::Invalid(
                "invalid or reserved projection label".into(),
            ));
        }
    }
    Ok(())
}

// Reject shared or unknown ownership for requested labels and for labels that
// an apply omission would otherwise attempt to remove.
fn ensure_projection_labels_owned(
    config_map: &ConfigMap,
    desired: &BTreeMap<String, String>,
) -> Result<(), Error> {
    let Some(labels) = config_map.metadata.labels.as_ref() else {
        return Ok(());
    };
    let mut owners: BTreeMap<&str, (bool, bool)> = BTreeMap::new();
    for entry in config_map
        .metadata
        .managed_fields
        .as_deref()
        .unwrap_or_default()
    {
        if entry.subresource.as_deref().is_some_and(|s| !s.is_empty()) {
            continue;
        }
        if entry.fields_type.as_deref() != Some("FieldsV1") {
            return Err(Error::UnsafeProjectionOwnership);
        }
        let fields = entry
            .fields_v1
            .as_ref()
            .and_then(|f| f.0.as_object())
            .ok_or(Error::UnsafeProjectionOwnership)?;
        let Some(metadata) = fields.get("f:metadata") else {
            continue;
        };
        let metadata = metadata
            .as_object()
            .ok_or(Error::UnsafeProjectionOwnership)?;
        let Some(label_fields) = metadata.get("f:labels") else {
            continue;
        };
        let label_fields = label_fields
            .as_object()
            .ok_or(Error::UnsafeProjectionOwnership)?;
        let is_apply = entry.manager.as_deref() == Some(MANAGER)
            && entry.operation.as_deref() == Some("Apply");
        for key in labels.keys().filter(|key| key.as_str() != MANAGED_BY) {
            if label_fields.is_empty() || label_fields.contains_key(&format!("f:{key}")) {
                let owner = owners.entry(key).or_default();
                if is_apply {
                    owner.0 = true;
                } else {
                    owner.1 = true;
                }
            }
        }
    }
    for key in labels.keys().filter(|key| key.as_str() != MANAGED_BY) {
        let (ours, other) = owners.get(key.as_str()).copied().unwrap_or_default();
        if (desired.contains_key(key) || ours) && (!ours || other) {
            return Err(Error::UnsafeProjectionOwnership);
        }
    }
    Ok(())
}

fn set_owned(meta: &mut ObjectMeta) -> Result<(), Error> {
    let labels = meta.labels.get_or_insert_with(BTreeMap::new);
    if labels.get(MANAGED_BY).is_some_and(|v| v != MANAGER) {
        return Err(Error::NotOwned);
    }
    labels.insert(MANAGED_BY.into(), MANAGER.into());
    Ok(())
}

/// Build a projection without storing or logging key material. Values must be
/// pre-encoded gateway entries containing hashes, never raw API keys.
pub fn projection(name: &str, data: BTreeMap<String, String>) -> Result<ConfigMap, Error> {
    validate_name(name)?;
    let mut size = 0usize;
    for (key, value) in &data {
        if key.is_empty()
            || key.len() > 253
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        {
            return Err(Error::Invalid("invalid ConfigMap key".into()));
        }
        size = size
            .checked_add(value.len())
            .ok_or_else(|| Error::Invalid("projection too large".into()))?;
    }
    if size > CONFIG_MAP_MAX_BYTES {
        return Err(Error::Invalid(
            "projection exceeds Kubernetes ConfigMap size limit".into(),
        ));
    }
    Ok(ConfigMap {
        metadata: ObjectMeta {
            name: Some(name.into()),
            labels: Some(owned_labels()),
            ..Default::default()
        },
        data: Some(data),
        ..Default::default()
    })
}

/// The API's three-valued condition status. Unknown and malformed values are
/// never interpreted as success.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConditionStatus {
    True,
    False,
    Unknown,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Condition {
    pub kind: String,
    pub status: ConditionStatus,
    pub reason: Option<String>,
    pub message: Option<String>,
    pub observed_generation: Option<i64>,
    pub last_transition_time: Option<DateTime<Utc>>,
}

/// Malformed or absent status produces no asserted condition. Unknown status
/// strings are retained as Unknown; malformed optional fields are omitted.
pub fn conditions(object: &DynamicObject) -> Vec<Condition> {
    object
        .data
        .pointer("/status/conditions")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let kind = item.get("type")?.as_str()?.to_owned();
                    if kind.is_empty() {
                        return None;
                    }
                    let status = match item.get("status").and_then(|v| v.as_str()) {
                        Some("True") => ConditionStatus::True,
                        Some("False") => ConditionStatus::False,
                        _ => ConditionStatus::Unknown,
                    };
                    Some(Condition {
                        kind,
                        status,
                        reason: item
                            .get("reason")
                            .and_then(|v| v.as_str())
                            .map(str::to_owned),
                        message: item
                            .get("message")
                            .and_then(|v| v.as_str())
                            .map(str::to_owned),
                        observed_generation: item
                            .get("observedGeneration")
                            .and_then(|v| v.as_i64()),
                        last_transition_time: item
                            .get("lastTransitionTime")
                            .and_then(|v| v.as_str())
                            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                            .map(|t| t.with_timezone(&Utc)),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Snapshot changes atomically at the end of a relist; errors and relists
/// invalidate freshness immediately. Consumers must also enforce max_age.
#[derive(Default)]
pub struct WatchState {
    objects: HashMap<String, DynamicObject>,
    pending: Option<HashMap<String, DynamicObject>>,
    observed: Option<Instant>,
}
impl WatchState {
    pub fn apply(&mut self, event: Event<DynamicObject>, now: Instant) {
        match event {
            Event::Init => {
                self.pending = Some(HashMap::new());
                self.observed = None;
            }
            Event::InitApply(object) => {
                if let Some(pending) = &mut self.pending {
                    pending.insert(object.name_any(), object);
                }
            }
            Event::InitDone => {
                if let Some(pending) = self.pending.take() {
                    self.objects = pending;
                    self.observed = Some(now);
                }
            }
            Event::Apply(object) => {
                self.objects.insert(object.name_any(), object);
                if self.pending.is_none() && self.observed.is_some() {
                    self.observed = Some(now);
                }
            }
            Event::Delete(object) => {
                self.objects.remove(&object.name_any());
                if self.pending.is_none() && self.observed.is_some() {
                    self.observed = Some(now);
                }
            }
        }
    }
    pub fn invalidate(&mut self) {
        self.pending = None;
        self.observed = None;
    }
    pub fn observed_at(&self) -> Option<Instant> {
        self.observed
    }
    pub fn is_fresh(&self, now: Instant, max_age: Duration) -> bool {
        self.observed
            .and_then(|at| now.checked_duration_since(at))
            .is_some_and(|age| age <= max_age)
    }
    pub fn objects(&self) -> &HashMap<String, DynamicObject> {
        &self.objects
    }
}

#[cfg(test)]
mod tests;
