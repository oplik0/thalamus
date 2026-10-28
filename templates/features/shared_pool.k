# Experimental rendering only: install compatible pinned Kueue (plain-Pod
# integration enabled), KEDA and llm-d Router v1 separately. No runtime is
# installed, configured or validated by this module.
# GPU-only queues require Kueue's cluster-wide quotaCheckStrategy:
# IgnoreUndeclared (or equivalent explicit CPU/memory quota configuration);
# otherwise Pods requesting CPU and memory cannot be admitted.
# https://kueue.sigs.k8s.io/docs/tasks/run/plain_pods/
# https://keda.sh/docs/2.21/scalers/prometheus/
# https://llm-d.ai/docs/architecture/router/configuration

schema Member:
    name: str
    image: str
    model: str
    gpuPerReplica: int
    cpu: str
    memory: str
    port: int = 8000
    prometheusQuery: str
    threshold: str
    check:
        name != "" and image != "" and model != "", "member identity, image and model are required"
        gpuPerReplica > 0 and cpu != "" and memory != "" and port > 0, "invalid serving resources"
        prometheusQuery != "" and threshold != "", "KEDA query and threshold are required"

schema Pool:
    name: str
    namespace: str
    hardwareProfile: str
    nodeSelector: {str:str}
    gpuPerNode: int
    totalGpu: int
    members: [Member]
    prometheusAddress: str
    versions: {str:str}
    # Operator attestation; this renderer cannot inspect cluster configuration.
    quotaCheckStrategy: str = ""
    scaleDownSeconds: int = 600
    check:
        name != "" and namespace != "" and hardwareProfile != "" and len(nodeSelector) > 0, "pool identity and hardware are required"
        gpuPerNode > 0 and totalGpu > 0 and totalGpu % gpuPerNode == 0, "pool must contain whole GPU nodes"
        len(members) > 0 and len({m.name: True for m in members}) == len(members), "pool members must be nonempty and unique"
        all m in members {m.name != "filler"}, "member name filler is reserved for the shared filler queue"
        totalGpu % len(members) == 0, "pool must have equal whole GPU shares"
        all m in members {gpuPerNode % m.gpuPerReplica == 0 and (totalGpu // len(members)) % m.gpuPerReplica == 0}, "replicas must divide nodes and nominal shares"
        prometheusAddress != "" and scaleDownSeconds >= 300, "Prometheus and a long scale-down window are required"
        quotaCheckStrategy == "IgnoreUndeclared", "GPU-only queues require Kueue quotaCheckStrategy IgnoreUndeclared"
        "kueue" in versions and "keda" in versions and "llm-d-router" in versions and (all v in versions {versions[v] != ""}), "component version pins are required"

schema TrafficClass:
    name: str
    priority: int
    enginePriority: int
    maxRequests: int
    maxBytes: str
    check:
        name != "" and maxRequests > 0 and maxBytes != "", "class and bounded queue limits are required"
        priority >= -2147483647 and priority <= 2147483647 and enginePriority == -priority, "vLLM priority must invert router priority"

# The routing trust boundary must remove ALL client x-llm-d-inference-*
# headers and replace objective/fairness-id from authenticated metadata. Never
# forward untrusted class, project, or client-selected elevated priority. This
# module deliberately does not invent an agentgateway metadata mapping.
objective_header = "x-llm-d-inference-objective"
fairness_header = "x-llm-d-inference-fairness-id"

# EndpointPickerConfig is a file, NOT a Kubernetes API object. Mount this into
# the EPP and restart it on changes. Set a common bounded TTL; per-class TTL is
# not supported by the upstream priority band schema.
router_config = lambda classes: [TrafficClass], ttl: str -> any {
    assert len(classes) > 0 and ttl != "", "classes and queue TTL are required"
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
            priorityBands: [{priority: c.priority, maxRequests: c.maxRequests, maxBytes: c.maxBytes} for c in classes]
        }
    }
}

objectives = lambda namespace: str, pools: [str], classes: [TrafficClass] -> any {
    assert namespace != "" and len(pools) > 0 and len(pools) <= 16 and len({p: True for p in pools}) == len(pools), "objective requires distinct same-namespace pools"
    assert len({c.name: True for c in classes}) == len(classes), "class names must be unique"
    [{apiVersion: "llm-d.ai/v1", kind: "InferenceObjective", metadata: {name: c.name, namespace: namespace}, spec: {priority: c.priority, poolRefs: [{name: p} for p in pools]}} for c in classes]
}

# Each member's InferencePool must select thalamus.ai/pool=<pool> and
# thalamus.ai/pool-member=<member>. Queue names and PriorityClasses are pool-scoped. Filler
# desired count excludes the one required normal replica, so idle replicas
# together occupy exactly each member's nominal share.
render_pool = lambda pool: Pool -> any {
    share = pool.totalGpu // len(pool.members)
    annotations = {"thalamus.ai/kueue-version": pool.versions["kueue"], "thalamus.ai/keda-version": pool.versions["keda"], "thalamus.ai/router-version": pool.versions["llm-d-router"]}
    flavor = "${pool.name}-${pool.hardwareProfile}"
    filler = "${pool.name}-filler"
    base = [{apiVersion: "kueue.x-k8s.io/v1beta2", kind: "ResourceFlavor", metadata: {name: flavor, annotations: annotations}, spec: {nodeLabels: pool.nodeSelector}},
     {apiVersion: "scheduling.k8s.io/v1", kind: "PriorityClass", metadata: {name: "${pool.name}-normal"}, value: 1000, globalDefault: False},
     {apiVersion: "scheduling.k8s.io/v1", kind: "PriorityClass", metadata: {name: "${pool.name}-filler"}, value: 0, globalDefault: False},
     {apiVersion: "kueue.x-k8s.io/v1beta2", kind: "ClusterQueue", metadata: {name: filler}, spec: {cohort: pool.name, namespaceSelector: {matchLabels: {"kubernetes.io/metadata.name": pool.namespace}}, resourceGroups: [{coveredResources: ["nvidia.com/gpu"], flavors: [{name: flavor, resources: [{name: "nvidia.com/gpu", nominalQuota: "0"}]}]}]}},
     {apiVersion: "kueue.x-k8s.io/v1beta2", kind: "LocalQueue", metadata: {name: filler, namespace: pool.namespace}, spec: {clusterQueue: filler}}]
    clusterQueues = [{apiVersion: "kueue.x-k8s.io/v1beta2", kind: "ClusterQueue", metadata: {name: "${pool.name}-${m.name}"}, spec: {cohort: pool.name, namespaceSelector: {matchLabels: {"kubernetes.io/metadata.name": pool.namespace}}, preemption: {reclaimWithinCohort: "Any", borrowWithinCohort: {policy: "LowerPriority"}}, resourceGroups: [{coveredResources: ["nvidia.com/gpu"], flavors: [{name: flavor, resources: [{name: "nvidia.com/gpu", nominalQuota: "${share}"}]}]}]}} for m in pool.members]
    localQueues = [{apiVersion: "kueue.x-k8s.io/v1beta2", kind: "LocalQueue", metadata: {name: "${pool.name}-${m.name}", namespace: pool.namespace}, spec: {clusterQueue: "${pool.name}-${m.name}"}} for m in pool.members]
    deployments = [{apiVersion: "apps/v1", kind: "Deployment", metadata: {name: "${pool.name}-${m.name}-${role}", namespace: pool.namespace}, spec: {
        replicas: (1 if role == "normal" else share // m.gpuPerReplica - 1)
        selector: {matchLabels: {"thalamus.ai/workload": "${pool.name}-${m.name}-${role}"}}
        template: {metadata: {labels: {"thalamus.ai/workload": "${pool.name}-${m.name}-${role}", "thalamus.ai/pool": pool.name, "thalamus.ai/pool-member": m.name, "kueue.x-k8s.io/queue-name": ("${pool.name}-${m.name}" if role == "normal" else filler)}}, spec: {
            priorityClassName: "${pool.name}-${role}"
            nodeSelector: pool.nodeSelector
            containers: [{name: "vllm", image: m.image, args: ["--model", m.model, "--served-model-name", m.name, "--port", "${m.port}", "--tensor-parallel-size", "${m.gpuPerReplica}", "--scheduling-policy", "priority"], ports: [{containerPort: m.port}], readinessProbe: {httpGet: {path: "/health", port: m.port}, periodSeconds: 10}, resources: {requests: {"nvidia.com/gpu": "${m.gpuPerReplica}", cpu: m.cpu, memory: m.memory}, limits: {"nvidia.com/gpu": "${m.gpuPerReplica}", cpu: m.cpu, memory: m.memory}}}]
        }}
    }} for m in pool.members for role in ["normal", "filler"]]
    scaledObjects = [{apiVersion: "keda.sh/v1alpha1", kind: "ScaledObject", metadata: {name: "${pool.name}-${m.name}", namespace: pool.namespace}, spec: {
        scaleTargetRef: {apiVersion: "apps/v1", kind: "Deployment", name: "${pool.name}-${m.name}-normal"}
        minReplicaCount: 1
        maxReplicaCount: pool.totalGpu // m.gpuPerReplica
        advanced: {horizontalPodAutoscalerConfig: {behavior: {scaleDown: {stabilizationWindowSeconds: pool.scaleDownSeconds}}}}
        triggers: [{type: "prometheus", metadata: {serverAddress: pool.prometheusAddress, query: m.prometheusQuery, threshold: m.threshold, ignoreNullValues: "false"}}]
    }} for m in pool.members]
    base + clusterQueues + localQueues + deployments + scaledObjects
}
