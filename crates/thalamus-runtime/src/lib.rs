//! Conservative, live Kubernetes observations used as evidence of serving identity.
//!
//! Operator configuration identifies resources; it is never an observation. A
//! model manifest is a curated, immutable ConfigMap binding content digests to
//! the exact OCI image digest and model path. It is not a pod self-attestation.

pub mod admission;
mod agentic;
pub mod binding;
mod drain;
mod live;
mod observe;
pub mod records;

pub use agentic::join_agentic_route;
pub use drain::{FillerPod, FillerReplicaSet, FillerScaler, FillerState, join_filler_state};
pub use live::LiveObserver;
pub use observe::{LiveResources, join_route, join_runner};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thalamus_core::{DesiredIdentity, PublishedService, RuntimeIdentity};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceRoutes {
    pub public: String,
    pub team_id: Uuid,
    pub project_id: Uuid,
    pub service: PublishedService,
}

/// Names and installed API versions only; none of these fields prove live state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    pub namespace: String,
    pub route_version: String,
    pub backend_version: String,
    pub policy_version: String,
    pub pool_version: String,
    pub serving_version: String,
    pub source_version: String,
    pub flux_version: String,
    pub helm_version: String,
    /// Exact digest-pinned agentic images trusted by administrators. This is a
    /// trust policy, not an observation or an inventory of candidate revisions.
    #[serde(default)]
    pub trusted_agentic_images: Vec<String>,
    pub routes: BTreeMap<String, ServiceRoutes>,
}

/// The underlying direct model chain independently joined for an agentic
/// route, before adding agentic components to its full execution identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnderlyingModelIdentity {
    pub rollout_configuration: String,
    pub runtime: RuntimeIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentSnapshot {
    pub deployment: String,
    pub revision: String,
    /// Flux revision independently reconciled from the live source.
    pub flux_revision: String,
    /// Constant asserted by the selected gateway backend, not by a serving pod.
    pub backend: String,
    pub desired: DesiredIdentity,
    /// Live serving identity with only intentional rollout scaling differences removed.
    pub rollout_configuration: String,
    pub runtime: RuntimeIdentity,
    /// Present only when `runtime` also includes an independently joined
    /// agentic chain; direct observations use `runtime` itself as model identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub underlying_model: Option<UnderlyingModelIdentity>,
    /// Reviewed non-secret projection of the actual serving container.
    pub engine_spec: serde_json::Value,
    /// Sorted, with one entry per selected pod (multiple pods may share a node).
    pub nodes: Vec<String>,
}

impl DeploymentSnapshot {
    fn model_identity(&self) -> (&str, &RuntimeIdentity) {
        match &self.underlying_model {
            Some(model) => (&model.rollout_configuration, &model.runtime),
            None => (&self.rollout_configuration, &self.runtime),
        }
    }

    /// Compare independently complete candidate/public observations while allowing
    /// a Flux commit, replica count, autoscaling/filler membership and nodes to change.
    pub fn same_serving_revision(&self, other: &Self) -> bool {
        let (left_config, left_runtime) = self.model_identity();
        let (right_config, right_runtime) = other.model_identity();
        self.deployment == other.deployment
            && self.revision == other.revision
            && self.backend == other.backend
            && self.underlying_model.is_some() == other.underlying_model.is_some()
            && left_config == right_config
            && left_runtime == right_runtime
            && self.engine_spec == other.engine_spec
            && (self.underlying_model.is_none()
                || (self.rollout_configuration == other.rollout_configuration
                    && self.runtime.image_digests == other.runtime.image_digests
                    && self.runtime.model_content == other.runtime.model_content
                    && self.runtime.tokenizer_content == other.runtime.tokenizer_content
                    && self.runtime.effective_configuration == other.runtime.effective_configuration
                    // Complete run evidence still compares every UID exactly.
                    && self.runtime.component_versions.iter()
                        .filter(|(key, _)| !key.starts_with("agentic-pod-uid/"))
                        .eq(other.runtime.component_versions.iter()
                            .filter(|(key, _)| !key.starts_with("agentic-pod-uid/")))))
    }
}

/// Actual accepted Gateway listener associated with a route hostname.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RouteListener {
    pub hostname: String,
    pub port: u16,
    /// HTTP or HTTPS as reported by the live Gateway listener.
    pub protocol: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteSnapshot {
    pub observed_at: DateTime<Utc>,
    pub service: String,
    pub route: String,
    /// True only for a route whose complete agentic and internal model hops
    /// were independently joined by the live observer.
    #[serde(default)]
    pub agentic: bool,
    /// Literal HTTPRoute hostnames; wildcard hosts are not accepted.
    pub hostnames: Vec<String>,
    /// Raw, exact HTTPRoute match paths, before the supported rewrite.
    pub paths: Vec<String>,
    /// Ports of the accepted listeners, sorted and deduplicated.
    pub ports: Vec<u16>,
    /// Bind hostname, port and protocol as a tuple, not a Cartesian product.
    pub listeners: Vec<RouteListener>,
    /// Actual HTTPRoute weights, including zero-weight backends.
    pub weights: BTreeMap<String, u32>,
    pub deployments: Vec<DeploymentSnapshot>,
}

impl RouteSnapshot {
    /// Historical observation time is metadata, not part of serving identity.
    pub fn same_identity(&self, other: &Self) -> bool {
        self.service == other.service
            && self.route == other.route
            && self.agentic == other.agentic
            && self.hostnames == other.hostnames
            && self.paths == other.paths
            && self.ports == other.ports
            && self.listeners == other.listeners
            && self.weights == other.weights
            && self.deployments == other.deployments
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerObservation {
    pub job: String,
    pub job_uid: String,
    pub pod: String,
    pub pod_uid: String,
    pub pod_owner_job_uid: String,
    /// Image reference from the actual runner Pod spec.
    pub image: String,
    /// Canonical sha256 image ID reported for that Pod by Kubernetes.
    pub image_id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Kubernetes API: {0}")]
    Kubernetes(#[from] kube::Error),
    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("incomplete or ambiguous runtime evidence: {0}")]
    Invalid(String),
    #[error("Kubernetes resources changed during observation")]
    Changed,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
#[cfg(test)]
mod agentic_tests;
#[cfg(test)]
mod drain_tests;
#[cfg(test)]
mod tests;
