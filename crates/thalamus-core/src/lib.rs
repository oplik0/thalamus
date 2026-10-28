//! Serializable control-plane domain types.
//!
//! Catalog objects describe desired state; observations and run evidence describe
//! independently collected facts. None of these types contacts an external system.
//! Identifiers and content identities are strings supplied by the owning adapter;
//! callers must not use mutable tags or PVC names as content identities.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// A stable caller-facing service and every deployment that may serve it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PublishedService {
    /// Stable name exposed to callers.
    pub name: String,
    /// API protocols offered by the service.
    pub protocols: BTreeSet<EngineProtocol>,
    /// Deployments that can receive traffic, including zero-weight failover targets.
    pub deployments: Vec<ServiceDeployment>,
    /// Git-defined access policy reference.
    pub access_policy: String,
    /// Teams or users authorized to approve changes.
    pub owners: Vec<String>,
}

/// A deployment reference and routing weight; membership, not weight, determines eligibility.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceDeployment {
    /// Catalog deployment name.
    pub deployment: String,
    /// Relative route weight, interpreted by the route implementation.
    pub weight: u32,
}

/// Desired serving configuration in one cluster and namespace.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Deployment {
    /// Deployment name, unique within the catalog.
    pub name: String,
    /// Recipe catalog reference.
    pub recipe: String,
    /// Artifact catalog reference.
    pub artifact: String,
    /// Hardware profile catalog reference.
    pub hardware_profile: String,
    /// Target cluster identifier.
    pub cluster: String,
    /// Target Kubernetes namespace.
    pub namespace: String,
    /// Optional override; the running digest must still be observed independently.
    pub image: Option<ImageReference>,
    /// Structured recipe overrides, interpreted and validated by the renderer.
    pub overrides: BTreeMap<String, serde_json::Value>,
}

/// An exact model and tokenizer source, format and quantization.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelArtifact {
    /// Catalog name.
    pub name: String,
    /// Source and its configured revision or immutable snapshot.
    pub source: ArtifactSource,
    /// Tokenizer source or revision, as resolved by the catalog.
    pub tokenizer: String,
    /// Model serialization format.
    pub format: String,
    /// Quantization, if any.
    pub quantization: Option<String>,
    /// Capabilities claimed by the artifact, not proof of runtime behavior.
    pub declared_capabilities: CapabilityContract,
}

/// Where model weights originate. PVC names alone never identify their contents.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ArtifactSource {
    /// Hugging Face repository and exact configured revision.
    HuggingFace {
        repository: String,
        revision: String,
    },
    /// OCI artifact reference, which may require resolution to a digest.
    Oci { reference: String },
    /// PVC plus a content manifest or immutable snapshot identifier, if available.
    Pvc {
        claim: String,
        content_identity: Option<String>,
    },
}

/// Hardware properties declared in Git and checked against node observations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HardwareProfile {
    /// Catalog name.
    pub name: String,
    /// GPU product name.
    pub gpu_product: String,
    /// Bytes of memory per GPU.
    pub gpu_memory_bytes: u64,
    /// Maximum GPUs on a node of this profile.
    pub gpus_per_node: u32,
    /// GPU interconnect description.
    pub interconnect: String,
    /// CPU architecture.
    pub cpu_architecture: String,
    /// Host memory in bytes.
    pub host_memory_bytes: u64,
    /// Whether GPU and CPU share this memory.
    pub shared_memory: bool,
    /// Required driver stack description.
    pub driver_stack: String,
    /// Labels used to check membership of observed nodes.
    pub node_labels: BTreeMap<String, String>,
}

/// Engine template identity; other engines can be named without changing the model.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Engine(pub String);

/// An upstream protocol needed by a recipe or exposed by a service.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineProtocol {
    /// OpenAI-compatible chat completions.
    OpenAiChat,
    /// Anthropic-compatible Messages.
    AnthropicMessages,
    /// A protocol named by an engine adapter.
    Other(String),
}

/// Registry reference as configured; a tag is not an immutable image identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageReference {
    /// Complete image reference including optional tag or digest.
    pub reference: String,
}

/// Engine defaults and checks for one template family.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Recipe {
    /// Catalog name.
    pub name: String,
    /// Engine template to render.
    pub engine: Engine,
    /// Default image, overridable per deployment.
    pub default_image: ImageReference,
    /// Protocols required from the engine.
    pub required_protocols: BTreeSet<EngineProtocol>,
    /// Tensor parallel GPU count per serving unit.
    pub tensor_parallelism: u32,
    /// Default maximum context tokens.
    pub context_tokens: u32,
    /// Engine-specific batching defaults.
    pub batching: BTreeMap<String, serde_json::Value>,
    /// Optional chat template reference.
    pub chat_template: Option<String>,
    /// Engine-specific tool parser.
    pub tool_parser: Option<String>,
    /// Engine-specific reasoning parser.
    pub reasoning_parser: Option<String>,
    /// Optional component identifiers, such as agentic-api or KV offload.
    pub components: BTreeSet<String>,
    /// Validation suite identifier and version.
    pub validation_suite: String,
    /// Human-readable known limits.
    pub known_limits: Vec<String>,
    /// Claims, which are not validation evidence.
    pub declared_capabilities: CapabilityContract,
}

/// One of the five independent status facts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusValue {
    /// The fact was positively established for the specified revision.
    Yes,
    /// The fact was negatively established for the specified revision.
    No,
    /// No reliable observation is available.
    Unknown,
    /// The last observation is no longer fresh enough to rely on.
    Stale,
}

/// An observation, including provenance and time even when unknown or stale.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusObservation {
    /// Observed value.
    pub value: StatusValue,
    /// Revision to which the observation applies.
    pub revision: String,
    /// When the observation or loss of freshness was recorded.
    pub observed_at: DateTime<Utc>,
    /// Explanation and/or source of the observation.
    pub reason: String,
}

/// Separate status columns; success in one column does not imply any other.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentStatuses {
    /// Change is present on the default branch.
    pub accepted: StatusObservation,
    /// Complete catalog-to-serving-resource reconciliation chain is established.
    pub applied: StatusObservation,
    /// Serving resource is ready with the rendered image digests.
    pub running: StatusObservation,
    /// Latest validation for this revision passed.
    pub functional: StatusObservation,
    /// A benchmark exists for this revision or eligible identical runtime.
    pub measured: StatusObservation,
}

/// A capability set. Context probing establishes acceptance, not quality under load.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityContract {
    /// Maximum accepted context in tokens.
    pub context_tokens: u32,
    /// Features supported by the source of this contract.
    pub features: BTreeSet<String>,
    /// API protocols supported by the source of this contract.
    pub protocols: BTreeSet<EngineProtocol>,
}

/// Capabilities claimed by a recipe or artifact, without test evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclaredCapabilities {
    /// Recipe or artifact making the claim.
    pub source: String,
    /// Claimed contract.
    pub contract: CapabilityContract,
}

/// A successful validation result tied to a deployment revision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidatedCapabilities {
    /// Deployment tested.
    pub deployment: String,
    /// Exact deployment revision tested.
    pub revision: String,
    /// Run providing evidence.
    pub run_id: String,
    /// Contract established by the run.
    pub contract: CapabilityContract,
}

/// Contract callers may rely on across all deployments eligible to receive traffic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishedCapabilities {
    /// Published service name.
    pub service: String,
    /// Intersection of validated context and features, with service-exposed protocols.
    pub contract: CapabilityContract,
}

/// A deployment revision currently eligible to receive service traffic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EligibleDeployment {
    /// Deployment name.
    pub deployment: String,
    /// Current revision that needs validation.
    pub revision: String,
}

/// Failure to establish a safe published capability contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CapabilityError {
    /// A service without eligible deployments cannot claim tested capabilities.
    NoDeployments,
    /// A deployment appears more than once in the eligible set.
    DuplicateDeployment(String),
    /// A current deployment revision has no unique, usable validation.
    MissingOrAmbiguousValidation(String),
    /// The published service requires a protocol not validated on every deployment.
    UnsupportedProtocol,
}

/// Intersect successful current-revision validations for *all* eligible deployments,
/// publishing only protocols exposed by the service.
///
/// Stale validations, empty run identities, duplicates, and missing validations fail
/// closed. Callers must supply every deployment that can serve traffic, including
/// failover and zero-weight targets that can become active without revalidation.
pub fn published_capabilities(
    service: &PublishedService,
    eligible: &[EligibleDeployment],
    validations: &[ValidatedCapabilities],
) -> Result<PublishedCapabilities, CapabilityError> {
    if eligible.is_empty() || service.deployments.is_empty() {
        return Err(CapabilityError::NoDeployments);
    }
    let mut seen = BTreeSet::new();
    let mut contract: Option<CapabilityContract> = None;
    for entry in eligible {
        if !seen.insert(entry.deployment.as_str()) {
            return Err(CapabilityError::DuplicateDeployment(
                entry.deployment.clone(),
            ));
        }
        if entry.deployment.is_empty() || entry.revision.is_empty() {
            return Err(CapabilityError::MissingOrAmbiguousValidation(
                entry.deployment.clone(),
            ));
        }
        let mut matches = validations.iter().filter(|v| {
            v.deployment == entry.deployment
                && v.revision == entry.revision
                && !v.run_id.is_empty()
                && v.contract.context_tokens > 0
        });
        let validation = matches.next().ok_or_else(|| {
            CapabilityError::MissingOrAmbiguousValidation(entry.deployment.clone())
        })?;
        if matches.next().is_some() {
            return Err(CapabilityError::MissingOrAmbiguousValidation(
                entry.deployment.clone(),
            ));
        }
        match &mut contract {
            None => contract = Some(validation.contract.clone()),
            Some(current) => {
                current.context_tokens = current
                    .context_tokens
                    .min(validation.contract.context_tokens);
                current.features = current
                    .features
                    .intersection(&validation.contract.features)
                    .cloned()
                    .collect();
                current.protocols = current
                    .protocols
                    .intersection(&validation.contract.protocols)
                    .cloned()
                    .collect();
            }
        }
    }
    let service_names: BTreeSet<_> = service
        .deployments
        .iter()
        .map(|d| d.deployment.as_str())
        .collect();
    if service_names.len() != service.deployments.len() || service_names != seen {
        return Err(CapabilityError::MissingOrAmbiguousValidation(
            service.name.clone(),
        ));
    }
    let mut contract = contract.ok_or(CapabilityError::NoDeployments)?;
    if !service.protocols.is_subset(&contract.protocols) {
        return Err(CapabilityError::UnsupportedProtocol);
    }
    contract.protocols.clone_from(&service.protocols);
    Ok(PublishedCapabilities {
        service: service.name.clone(),
        contract,
    })
}

/// Validation classification for a recipe/artifact/hardware combination.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "tier", rename_all = "snake_case")]
pub enum ValidationTier {
    /// Suite passed for pinned component versions.
    Validated {
        component_versions: BTreeMap<String, String>,
        run_id: String,
    },
    /// Declared requirements match but have not been tested here.
    Expected,
    /// A known blocker prevents operation.
    Incompatible { reason: String },
}

/// Desired configuration identity, produced from canonical render and resolved dependencies.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesiredIdentity {
    /// Digest of canonical rendered resources.
    pub rendered_resources: String,
    /// Digest of all resolved configuration dependencies.
    pub resolved_dependencies: String,
}

/// Runtime identity observed rather than inferred from desired image tags or PVC names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeIdentity {
    /// Immutable image digests by component.
    pub image_digests: BTreeMap<String, String>,
    /// Digest of actual model contents (including a PVC manifest or snapshot).
    pub model_content: String,
    /// Digest of actual tokenizer contents.
    pub tokenizer_content: String,
    /// Digest of effective runtime configuration.
    pub effective_configuration: String,
    /// Observed versions of runtime components, including the gateway.
    pub component_versions: BTreeMap<String, String>,
}

/// Test definition identity, independent from the configuration under test.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestIdentity {
    /// Suite name and version or digest.
    pub suite: String,
    /// Runner name and version or digest.
    pub runner: String,
    /// Client name and version or digest.
    pub client: String,
    /// Workload definition identity.
    pub workload: String,
    /// Random seed used for the run.
    pub seed: u64,
}

/// Conditions under which a run was measured.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunConditions {
    /// Observed hardware and driver identity.
    pub hardware: String,
    /// Number of serving replicas.
    pub replicas: u32,
    /// Cache state at the start of the run.
    pub cache_state: String,
    /// Competing traffic conditions.
    pub competing_traffic: String,
    /// Time or time-window identity relevant to comparison, supplied by the observer.
    pub time: String,
}

/// All four identities needed to compare run evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceIdentity {
    /// Canonical desired configuration.
    pub desired: DesiredIdentity,
    /// Observed runtime and immutable content identities.
    pub runtime: RuntimeIdentity,
    /// Exact test definition.
    pub test: TestIdentity,
    /// Recorded run conditions.
    pub conditions: RunConditions,
}

impl EvidenceIdentity {
    /// Return whether a prior run can serve as evidence for this identity.
    ///
    /// Matching is deliberately conservative: all four identities must be complete
    /// and equal, and image identities must be sha256 digests, not mutable tags.
    pub fn permits_reuse_of(&self, prior: &Self) -> bool {
        fn digest(s: &str) -> bool {
            s.strip_prefix("sha256:")
                .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        }
        fn complete(id: &EvidenceIdentity) -> bool {
            let r = &id.runtime;
            !id.desired.rendered_resources.is_empty()
                && !id.desired.resolved_dependencies.is_empty()
                && !r.image_digests.is_empty()
                && r.image_digests
                    .iter()
                    .all(|(name, value)| !name.is_empty() && digest(value))
                && !r.model_content.is_empty()
                && !r.tokenizer_content.is_empty()
                && !r.effective_configuration.is_empty()
                && !r.component_versions.is_empty()
                && r.component_versions
                    .iter()
                    .all(|(k, v)| !k.is_empty() && !v.is_empty())
                && !id.test.suite.is_empty()
                && !id.test.runner.is_empty()
                && !id.test.client.is_empty()
                && !id.test.workload.is_empty()
                && !id.conditions.hardware.is_empty()
                && id.conditions.replicas > 0
                && !id.conditions.cache_state.is_empty()
                && !id.conditions.competing_traffic.is_empty()
                && !id.conditions.time.is_empty()
        }
        complete(self) && complete(prior) && self == prior
    }
}

/// Type of a recorded Kubernetes Job run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunKind {
    Validation,
    Benchmark,
    Diagnostic,
}

/// Outcome of a run; only successful results can serve as passing evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    Pending,
    Running,
    Passed,
    Failed,
    Cancelled,
}

/// Run provenance; raw results remain in the configured object store.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    /// Durable run identifier.
    pub id: String,
    /// Stable retry key used to find an existing Job.
    pub idempotency_key: String,
    /// Deployment tested.
    pub deployment: String,
    /// Deployment revision tested.
    pub revision: String,
    /// Job purpose.
    pub kind: RunKind,
    /// Current or final outcome.
    pub outcome: RunOutcome,
    /// Run start time.
    pub started_at: DateTime<Utc>,
    /// Four-part evidence identity, if observation is complete.
    pub identity: Option<EvidenceIdentity>,
    /// Object-store location of raw results, if available.
    pub results_uri: Option<String>,
}

impl Run {
    /// Whether this successful prior run is evidence for the requested test identity.
    pub fn is_reusable_for(&self, kind: RunKind, identity: &EvidenceIdentity) -> bool {
        self.outcome == RunOutcome::Passed
            && self.kind == kind
            && self
                .identity
                .as_ref()
                .is_some_and(|prior| identity.permits_reuse_of(prior))
    }
}

/// Available GPUs on one node, not aggregated across the cluster.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeGpuCapacity {
    /// Node name.
    pub node: String,
    /// GPUs presently free and schedulable for this workload.
    pub free_gpus: u32,
    /// GPUs held by fillers that this workload can actually preempt.
    pub preemptible_filler_gpus: u32,
}

/// Chosen update strategy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RolloutMode {
    Gated,
    Rolling,
    InPlace,
}

/// Rollout decision and explanation suitable for a change request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutDecision {
    /// Selected strategy.
    pub mode: RolloutMode,
    /// Number of additional whole serving units that can fit.
    pub available_units: u64,
    /// Human-readable explanation.
    pub reason: String,
}

/// Invalid or incomplete capacity input must not silently imply available capacity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CapacityError {
    /// The profile or requested serving unit has zero GPUs.
    InvalidGpuCount,
    /// A node is repeated or has no identity.
    InvalidNode(String),
    /// Reported free plus preemptible GPUs exceed the node's profile.
    ExceedsNodeCapacity(String),
    /// A candidate must have at least one serving unit.
    InvalidReplicaCount,
}

/// Choose a rollout from schedulable per-node headroom, including preemptible fillers.
///
/// `candidate_replicas` is the full candidate size. A change requiring a gate
/// needs room for that whole candidate; a change not requiring a gate rolls when
/// one surge unit fits. This is a capacity decision, not a scheduler simulation:
/// callers must supply only nodes actually eligible for the candidate workload.
pub fn select_rollout_mode(
    profile: &HardwareProfile,
    nodes: &[NodeGpuCapacity],
    gpus_per_unit: u32,
    candidate_replicas: u32,
    requires_gate: bool,
) -> Result<RolloutDecision, CapacityError> {
    if profile.gpus_per_node == 0 || gpus_per_unit == 0 || gpus_per_unit > profile.gpus_per_node {
        return Err(CapacityError::InvalidGpuCount);
    }
    if candidate_replicas == 0 {
        return Err(CapacityError::InvalidReplicaCount);
    }
    let mut names = BTreeSet::new();
    let mut units = 0u64;
    for node in nodes {
        if node.node.is_empty() || !names.insert(&node.node) {
            return Err(CapacityError::InvalidNode(node.node.clone()));
        }
        let available = node
            .free_gpus
            .checked_add(node.preemptible_filler_gpus)
            .filter(|n| *n <= profile.gpus_per_node)
            .ok_or_else(|| CapacityError::ExceedsNodeCapacity(node.node.clone()))?;
        units += u64::from(available / gpus_per_unit);
    }
    let (mode, reason) = if requires_gate && units >= u64::from(candidate_replicas) {
        (
            RolloutMode::Gated,
            format!("{units} additional units fit; the candidate needs {candidate_replicas}"),
        )
    } else if !requires_gate && units >= 1 {
        (
            RolloutMode::Rolling,
            format!("{units} additional units fit and no validation gate is required"),
        )
    } else {
        (
            RolloutMode::InPlace,
            format!(
                "{units} additional units fit; {} required",
                if requires_gate { candidate_replicas } else { 1 }
            ),
        )
    };
    Ok(RolloutDecision {
        mode,
        available_units: units,
        reason,
    })
}
