import regex

# These resources require Kueue v0.18.0 with the plain-Pod integration in the
# serving namespace, featureGates.QuotaCheckStrategy=true, and
# resources.quotaCheckStrategy=IgnoreUndeclared. This is a cluster installation
# prerequisite, not something a pool can configure or verify. Fair Sharing must
# be disabled: borrowWithinCohort is a classical-preemption policy.
# https://github.com/kubernetes-sigs/kueue/blob/v0.18.0/apis/config/v1beta2/configuration_types.go
# https://github.com/kserve/kserve/blob/v0.21.0/pkg/apis/serving/v1alpha2/llm_inference_service_types.go
# https://github.com/llm-d/llm-d-router/blob/v1.0.0/apix/config/v1/endpointpickerconfig_types.go

schema Member:
    # Logical model member: multiple deployment revisions may share its quota.
    name: str
    service: str
    namespace: str
    gpuPerReplica: int
    prometheusQuery: str
    threshold: str
    check:
        name != "" and len(name) <= 56 and regex.match(name, "^[a-z0-9]([-a-z0-9]*[a-z0-9])?$"), "member must be a DNS name with room for the filler suffix"
        service != "" and namespace != "", "member service and namespace are required"
        gpuPerReplica > 0, "member GPU count must be positive"
        prometheusQuery != "" and threshold != "", "KEDA query and threshold are required"

schema Pool:
    # Cluster-scoped queue and flavor names use this globally unique name.
    name: str
    hardwareProfile: str
    nodeSelector: {str:str}
    gpuPerNode: int
    totalGpu: int
    members: [Member]
    prometheusAddress: str
    versions: {str:str}
    quotaCheckStrategy: str = "IgnoreUndeclared"
    scaleDownSeconds: int = 600
    check:
        name != "" and regex.match(name, "^[a-z0-9]([-a-z0-9]*[a-z0-9])?$") and len("${name}-filler") <= 63 and hardwareProfile != "" and len(nodeSelector) > 0, "pool identity and hardware are required"
        gpuPerNode > 0 and totalGpu > 0 and totalGpu % gpuPerNode == 0, "pool must contain whole GPU nodes"
        len(members) > 0 and len({m.name: True for m in members}) == len(members), "pool members must be nonempty and globally unique"
        all m in members {m.name != "filler" and len("${name}-${m.name}-filler") <= 63 and (all n in members {m.name != "${n.name}-filler"})}, "member queue name is reserved, ambiguous or too long"
        totalGpu % len(members) == 0, "pool must have equal whole GPU shares"
        all m in members {gpuPerNode % m.gpuPerReplica == 0 and (totalGpu // len(members)) % m.gpuPerReplica == 0}, "replicas must divide nodes and nominal shares"
        prometheusAddress != "" and scaleDownSeconds >= 300 and scaleDownSeconds <= 3600, "Prometheus and a 300-3600 second scale-down window are required"
        quotaCheckStrategy == "IgnoreUndeclared", "GPU-only queues require Kueue quotaCheckStrategy IgnoreUndeclared"
        "kueue" in versions and "keda" in versions and "llm-d-router" in versions, "component version pins are required"
        versions["kueue"] == "v0.18.0" and versions["keda"] == "2.20.2" and versions["llm-d-router"] == "v1.0.0", "unsupported shared-pool component version"

schema TrafficClass:
    name: str
    priority: int
    enginePriority: int
    maxRequests: int
    maxBytes: str
    check:
        name != "" and maxRequests > 0 and maxBytes != "" and maxBytes != "0", "class and bounded queue limits are required"
        priority >= -2147483647 and priority <= 2147483647 and enginePriority == -priority, "vLLM priority must invert router priority"

objective_header = "x-llm-d-inference-objective"
fairness_header = "x-llm-d-inference-fairness-id"

# This is a fragment for NativeKv's combined EndpointPickerConfig, not a
# Kubernetes resource. Each explicitly configured band needs its own fairness
# policy: defaultPriorityBand applies to unlisted priorities only.
router_config = lambda classes: [TrafficClass], ttl: str -> any {
    assert len(classes) > 0 and regex.match(ttl, "^([0-9]+([.][0-9]+)?(ns|us|µs|μs|ms|s|m|h))+$") and not regex.match(ttl, "^(0+([.]0+)?(ns|us|µs|μs|ms|s|m|h))+$"), "classes and a bounded queue TTL are required"
    assert len({c.name: True for c in classes}) == len(classes), "class names must be unique"
    assert len({"${c.priority}": True for c in classes}) == len(classes), "class priorities must be unique"
    {
        apiVersion: "llm-d.ai/v1"
        kind: "EndpointPickerConfig"
        featureGates: ["flowControl"]
        requestHandler: {propagatePriority: True}
        plugins: [{type: "round-robin-fairness-policy"}, {type: "fcfs-ordering-policy"}, {type: "utilization-detector"}]
        flowControl: {
            defaultRequestTTL: ttl
            saturationDetector: {pluginRef: "utilization-detector"}
            defaultPriorityBand: {fairnessPolicyRef: "round-robin-fairness-policy", orderingPolicyRef: "fcfs-ordering-policy"}
            priorityBands: [{priority: c.priority, maxRequests: "${c.maxRequests}", maxBytes: c.maxBytes, fairnessPolicyRef: "round-robin-fairness-policy", orderingPolicyRef: "fcfs-ordering-policy"} for c in classes]
        }
    }
}

# The gateway must derive the objective and fairness ID from authenticated key
# metadata and overwrite client-supplied llm-d headers. Pools are KServe- or
# raw-serving-owned, never independently recreated by this module.
objectives = lambda namespace: str, pools: [str], classes: [TrafficClass], serviceName: str -> any {
    assert namespace != "" and len(pools) > 0 and len(pools) <= 16 and len({p: True for p in pools}) == len(pools) and (all p in pools {p != ""}), "objective requires distinct same-namespace pools"
    assert len(classes) > 0 and len({c.name: True for c in classes}) == len(classes), "class names must be nonempty and unique"
    assert serviceName != "" and (all c in classes {len("${serviceName}-${c.name}") <= 63 and regex.match("${serviceName}-${c.name}", "^[a-z0-9]([-a-z0-9]*[a-z0-9])?$")}), "service-scoped objective names must be DNS labels of at most 63 characters"
    [{apiVersion: "llm-d.ai/v1", kind: "InferenceObjective", metadata: {name: "${serviceName}-${c.name}", namespace: namespace}, spec: {priority: c.priority, poolRefs: [{name: p} for p in pools]}} for c in classes]
}

member_name = lambda d: any -> str {
    d.poolMember if "poolMember" in d and d.poolMember != Undefined else d.name
}

member_for = lambda pool: Pool, d: any -> Member {
    matches = [m for m in pool.members if m.name == member_name(d)]
    assert len(matches) == 1, "deployment must be a shared pool member"
    m = matches[0]
    assert d.tensorParallelSize == m.gpuPerReplica and d.hardware == pool.hardwareProfile, "deployment hardware and GPU count must match its shared pool member"
    m
}

pool_labels = lambda pool: Pool, workloadName: str, memberName: str, role: str -> any {
    assert role in ["normal", "filler"], "unknown shared pool role"
    assert memberName in [m.name for m in pool.members], "deployment must be a shared pool member"
    {
        "thalamus.ai/deployment": workloadName
        "thalamus.ai/pool-role": role
        "kueue.x-k8s.io/queue-name": ("${pool.name}-${memberName}" if role == "normal" else "${pool.name}-${memberName}-filler")
        # Router v1 negates priority only for endpoints identifying this engine.
        "llm-d.ai/engine-type": "vllm"
    }
}

priority_class = lambda pool: Pool, role: str -> str {
    assert role in ["normal", "filler"], "unknown shared pool role"
    "${pool.name}-${role}"
}

filler_replicas = lambda pool: Pool, d: any -> int {
    m = member_for(pool, d)
    (pool.totalGpu // len(pool.members)) // m.gpuPerReplica - 1
}

# The ScaledObject follows the logical member across promotion; the active
# deployment name is substituted into reviewed PromQL when requested.
scaled_object = lambda service: any, pool: Pool, d: any -> any {
    m = member_for(pool, d)
    assert m.service == service.name and m.namespace == service.namespace, "deployment must belong to this pool member service and namespace"
    {
        apiVersion: "keda.sh/v1alpha1"
        kind: "ScaledObject"
        metadata: {name: "${pool.name}-${m.name}", namespace: service.namespace}
        spec: {
            scaleTargetRef: {apiVersion: "apps/v1", kind: "Deployment", name: d.name}
            minReplicaCount: 1
            maxReplicaCount: pool.totalGpu // m.gpuPerReplica
            advanced: {horizontalPodAutoscalerConfig: {behavior: {scaleDown: {stabilizationWindowSeconds: pool.scaleDownSeconds}}}}
            triggers: [{type: "prometheus", metadata: {serverAddress: pool.prometheusAddress, query: m.prometheusQuery.replace("{{deployment}}", d.name), threshold: m.threshold, ignoreNullValues: "false"}}]
        }
    }
}

# The dedicated pool root owns all cluster-scoped resources exactly once.
# Service roots receive this same resolved Pool and render LocalQueues only.
render_pool = lambda pool: Pool -> any {
    share = pool.totalGpu // len(pool.members)
    flavor = "${pool.name}-${pool.hardwareProfile}"
    filler = "${pool.name}-filler"
    annotations = {"thalamus.ai/kueue-version": pool.versions["kueue"], "thalamus.ai/keda-version": pool.versions["keda"], "thalamus.ai/router-version": pool.versions["llm-d-router"]}
    base = [
        {apiVersion: "kueue.x-k8s.io/v1beta2", kind: "ResourceFlavor", metadata: {name: flavor, annotations: annotations}, spec: {nodeLabels: pool.nodeSelector}},
        {apiVersion: "scheduling.k8s.io/v1", kind: "PriorityClass", metadata: {name: "${pool.name}-normal"}, value: 1000, globalDefault: False},
        {apiVersion: "scheduling.k8s.io/v1", kind: "PriorityClass", metadata: {name: filler}, value: 0, globalDefault: False},
        {apiVersion: "kueue.x-k8s.io/v1beta2", kind: "ClusterQueue", metadata: {name: filler}, spec: {cohortName: pool.name, namespaceSelector: {matchExpressions: [{key: "kubernetes.io/metadata.name", operator: "In", values: [m.namespace for m in pool.members]}]}, resourceGroups: [{coveredResources: ["nvidia.com/gpu"], flavors: [{name: flavor, resources: [{name: "nvidia.com/gpu", nominalQuota: "0"}]}]}]}},
    ]
    clusterQueues = [{apiVersion: "kueue.x-k8s.io/v1beta2", kind: "ClusterQueue", metadata: {name: "${pool.name}-${m.name}"}, spec: {cohortName: pool.name, namespaceSelector: {matchLabels: {"kubernetes.io/metadata.name": m.namespace}}, preemption: {reclaimWithinCohort: "Any", borrowWithinCohort: {policy: "LowerPriority"}}, resourceGroups: [{coveredResources: ["nvidia.com/gpu"], flavors: [{name: flavor, resources: [{name: "nvidia.com/gpu", nominalQuota: "${share}"}]}]}]}} for m in pool.members]
    base + clusterQueues
}

local_queues = lambda service: any, pool: Pool -> any {
    assert service.name != "" and service.namespace != "" and len(service.deployments) > 0, "service namespace and deployments are required"
    members = [m for m in pool.members if m.service == service.name and m.namespace == service.namespace]
    assert len({d.name: True for d in service.deployments}) == len(service.deployments) and (all d in service.deployments {len([m for m in members if m.name == member_name(d)]) == 1}) and (all m in members {any d in service.deployments {member_name(d) == m.name}}), "service deployments must exactly match its shared pool members"
    assert all d in service.deployments {d.hardware == pool.hardwareProfile and (all m in members {m.name != member_name(d) or m.gpuPerReplica == d.tensorParallelSize})}, "deployment hardware and GPU count must match its shared pool member"
    assert all m in members {len([d for d in service.deployments if member_name(d) == m.name and d.role == "stable" and d.weight > 0]) <= 1}, "shared pool member can have at most one active stable deployment"
    filler = "${pool.name}-filler"
    [{apiVersion: "kueue.x-k8s.io/v1beta2", kind: "LocalQueue", metadata: {name: "${pool.name}-${m.name}", namespace: service.namespace}, spec: {clusterQueue: "${pool.name}-${m.name}"}} for m in members] + [{apiVersion: "kueue.x-k8s.io/v1beta2", kind: "LocalQueue", metadata: {name: "${pool.name}-${m.name}-filler", namespace: service.namespace}, spec: {clusterQueue: filler}} for m in members]
}
