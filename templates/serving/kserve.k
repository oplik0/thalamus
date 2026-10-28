import net
import regex
import templates.features.agentic_api as agentic_api
import templates.features.shared_pool as shared_pool

# Catalog schemas. Values describe desired intent, not observed cluster state.
schema Artifact:
    uri: str
    # Content identities are declarations, not runtime measurements.
    modelDigest: str
    tokenizerDigest: str
    check:
        uri != "", "artifact URI is required"
        regex.match(modelDigest, "^sha256:[0-9a-f]{64}$"), "model digest must be a lowercase sha256 content identity"
        regex.match(tokenizerDigest, "^sha256:[0-9a-f]{64}$"), "tokenizer digest must be a lowercase sha256 content identity"

schema Hardware:
    nodeSelector: {str:str}
    gpuCount: int
    cpu: str
    memoryGi: int
    hostMemoryGi: int
    sharedGpuMemory: bool = False
    check:
        len(nodeSelector) > 0, "hardware node selector is required"
        gpuCount > 0, "GPU count must be positive"
        cpu != "", "CPU quantity is required"
        memoryGi > 0 and hostMemoryGi >= memoryGi, "invalid pod/host memory"

schema Features:
    toolParser: str = ""
    reasoningParser: str = ""
    prefixCaching: bool = True
    kvOffloadGi: int = 0
    shmGi: int = 1
    check:
        kvOffloadGi >= 0 and shmGi > 0, "invalid cache or shared memory size"
        kvOffloadGi == 0 or prefixCaching, "KV offload requires prefix caching"

schema Deployment:
    name: str
    artifact: str
    hardware: str
    image: str
    # Revision is the SHA-256 of canonical deployment, artifact and hardware
    # content, computed by the workflow rather than inferred from a mutable tag.
    revision: str
    role: str = "stable"
    # A candidate or promoted revision can retain its logical shared-pool quota.
    # If omitted, the deployment name is the member identity.
    poolMember?: str
    rolloutStrategy: str = "RollingUpdate"
    # A shared-pool candidate renders one effective replica; this catalog value
    # remains unchanged for promotion, when KEDA and filler capacity resume.
    # The role transition therefore does not change the deployment revision.
    replicas: int = 1
    contextLength: int = 4096
    maxNumSeqs: int = 64
    maxNumBatchedTokens: int = 4096
    tensorParallelSize: int = 1
    weight: int = 1
    # Image contains engine, model and tokenizer at /opt/thalamus/model.
    # This constrains intent only; runtime evidence requires independent observation.
    packagedModel: bool = False
    features: Features = Features {}
    check:
        len(name) <= 40 and regex.match(name, "^[a-z0-9]([-a-z0-9]*[a-z0-9])?$"), "deployment name must be a DNS label of at most 40 characters"
        len(poolMember) <= 56 and regex.match(poolMember, "^[a-z0-9]([-a-z0-9]*[a-z0-9])?$") if poolMember != Undefined, "pool member must be a DNS label of at most 56 characters"
        artifact != "" and hardware != "" and image != "", "deployment references and image are required"
        regex.match(revision, "^[0-9a-f]{64}$"), "deployment revision must be 64 lowercase hexadecimal characters"
        role in ["stable", "candidate"], "deployment role must be stable or candidate"
        rolloutStrategy in ["Recreate", "RollingUpdate"], "rollout strategy must be Recreate or RollingUpdate"
        role != "candidate" or packagedModel, "candidate deployments require a packaged immutable model"
        rolloutStrategy != "Recreate" or packagedModel, "Recreate requires a packaged immutable model"
        replicas > 0 and contextLength > 0 and maxNumSeqs > 0 and maxNumBatchedTokens > 0 and tensorParallelSize > 0, "deployment sizes must be positive"
        not packagedModel or (len(image.split("@sha256:")) == 2 and len(image.split("@sha256:")[1]) == 64 and regex.match(image.split("@sha256:")[1], "^[0-9a-f]{64}$")), "packaged model requires an immutable sha256 image digest"
        role != "stable" or (weight >= 0 and weight <= 1000000), "stable route weight must be between 0 and 1000000"

schema Gateway:
    name: str
    namespace: str
    hostname: str
    keySelector: {str:str}
    rateLimit: str
    check:
        name != "" and namespace != "" and hostname != "", "gateway identity and hostname are required"
        "thalamus.dev/gateway-keys" in keySelector and keySelector["thalamus.dev/gateway-keys"] == "true", "key selector must select the projected gateway keys"
        len(rateLimit) <= 63 and regex.match(rateLimit, "^[a-z0-9]([-a-z0-9]*[a-z0-9])?$"), "rate limit profile must be a DNS label of at most 63 characters"

# Class names are Git-reviewed; the gateway derives the objective from the
# authenticated key projection and overwrites caller-provided routing headers.
schema TrafficClasses:
    classes: [shared_pool.TrafficClass]
    ttl: str
    check:
        len(classes) > 0, "traffic classes and a bounded queue TTL are required"
        # The gateway timeout appends 5s; leave room for that fourth segment.
        regex.match(ttl, "^([0-9]{1,5}(h|ms|m|s)){1,3}$") and regex.match(ttl, ".*[1-9].*"), "queue TTL must be nonzero with one to three h/m/s/ms segments of at most five digits"
        len({c.name: True for c in classes}) == len(classes), "traffic class names must be unique"
        len({"${c.priority}": True for c in classes}) == len(classes), "traffic class priorities must be unique"

# Empty teams restricts the service to platform administrators. This is policy
# data consumed from the committed catalog by thalamus-access.
schema Access:
    teams: [str]
    check:
        all team in teams {regex.match(team, "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$") and team != "00000000-0000-0000-0000-000000000000"}, "access teams must be non-nil lowercase UUIDs"
        len({team: True for team in teams}) == len(teams), "access teams must be unique"

# These selectors and destination lists are administrator-reviewed network
# policy inputs, not proof of identity. Protect catalog updates, namespace and
# pod labels, admissions, and the CNI; policies are additive.
schema NetworkPeer:
    namespaceSelector: {str:str}
    podSelector: {str:str}
    check:
        len(namespaceSelector) > 0 and len(podSelector) > 0, "network peer requires namespace and pod selectors"
        (all key in namespaceSelector {key != ""}) and (all key in podSelector {key != ""}), "network selector keys cannot be empty"

schema NetworkDestination:
    cidr: str
    ports: [int]
    check:
        len(net.parse_CIDR(cidr)) > 0 and net.parse_CIDR(cidr)["mask"] > 0, "network destination requires a valid bounded CIDR"
        cidr.split("/")[0] == net.parse_CIDR(cidr)["ip"], "network destination CIDR must use the canonical network address"
        len(ports) > 0 and (all port in ports {port > 0 and port <= 65535}), "network destination requires valid TCP ports"
        len({"${port}": True for port in ports}) == len(ports), "network destination ports must be unique"

schema ServingNetwork:
    # Select the actual managed Gateway proxy Pods, not the Gateway object.
    gatewayPodSelector: {str:str}
    dns: NetworkPeer
    prometheus: NetworkPeer
    apiServer: [NetworkDestination]
    # Nonpackaged remote model downloads require explicit destinations,
    # including token exchange endpoints. NetworkPolicy applies to the whole
    # Pod; these allowances also apply to its model container.
    storageEgress: [NetworkDestination] = []
    check:
        len(gatewayPodSelector) > 0 and (all key in gatewayPodSelector {key != ""}), "managed gateway pod selector is required"
        len(apiServer) > 0, "endpoint picker requires explicit Kubernetes API destinations"

schema Service:
    name: str
    namespace: str
    access: Access
    gateway: Gateway
    network: ServingNetwork
    deployments: [Deployment]
    # Version values are provenance, not installed or verified by this renderer.
    versions: {str:str}
    # Optional feature values are data; rendering and policy stay reviewed code.
    agentic?: agentic_api.Recipe
    pool?: str
    trafficClasses?: TrafficClasses
    check:
        len(name) <= 40 and regex.match(name, "^[a-z0-9]([-a-z0-9]*[a-z0-9])?$") and namespace != "", "service name must be a DNS label of at most 40 characters and namespace is required"
        gateway.namespace == namespace, "managed serving gateway must share the workload namespace"
        "gateway.networking.k8s.io/gateway-name" in network.gatewayPodSelector and network.gatewayPodSelector["gateway.networking.k8s.io/gateway-name"] == gateway.name, "gateway pod selector must select the named managed gateway"
        len(deployments) > 0, "at least one deployment is required"
        any d in deployments {d.role == "stable" and d.weight > 0}, "at least one stable deployment must receive public traffic"
        all d in deployments {all other in deployments {d.role != "stable" or d.weight == 0 or other.role != "stable" or other.weight == 0 or (d.contextLength == other.contextLength and d.features.toolParser == other.features.toolParser and d.features.reasoningParser == other.features.reasoningParser)}}, "weighted stable deployments must expose the same declared contract"
        len({d.name: True for d in deployments}) == len(deployments), "deployment names must be unique"
        all d in deployments {d.role != "candidate" or "${d.name}-candidate" != name}, "candidate route must not collide with the public route"
        "kserve" in versions and "agentgateway" in versions and "inferenceExtension" in versions and "vllm" in versions, "upstream version pins are required"
        versions["kserve"] != "" and versions["agentgateway"] != "" and versions["inferenceExtension"] != "" and versions["vllm"] != "", "version pins cannot be empty"
        len(pool) <= 56 and regex.match(pool, "^[a-z0-9]([-a-z0-9]*[a-z0-9])?$") if pool != Undefined, "pool reference must be a DNS label of at most 56 characters"
        all d in deployments {d.poolMember == Undefined} if pool == Undefined, "pool member requires a shared pool reference"
        agentic.name == name and agentic.namespace == namespace if agentic != Undefined, "agentic recipe must belong to the service"
        all instance in agentic.instances {instance.keySelector == gateway.keySelector} if agentic != Undefined, "agentic and serving routes must use the same key projection"
        agentic.gatewayPodLabels == network.gatewayPodSelector if agentic != Undefined and agentic.publicGateway == gateway.name and agentic.publicGatewayNamespace == gateway.namespace, "shared agentic gateway must select the serving gateway pods"
        ("kubernetes.io/metadata.name" in agentic.gatewayNamespaceLabels and agentic.gatewayNamespaceLabels["kubernetes.io/metadata.name"] == namespace) if agentic != Undefined and agentic.publicGateway == gateway.name and agentic.publicGatewayNamespace == gateway.namespace, "shared agentic gateway must select the serving gateway namespace"
        all instance in agentic.instances {instance.hostname != gateway.hostname} if agentic != Undefined and agentic.publicGateway == gateway.name and agentic.publicGatewayNamespace == gateway.namespace, "agentic and serving routes on the same gateway require distinct hostnames"
        all instance in agentic.instances {all d in deployments {d.role != "candidate" or "${name}-${instance.name}" != "${d.name}-candidate"}} if agentic != Undefined, "agentic public route conflicts with a candidate route"
        agentic.trafficClasses == [c.name for c in trafficClasses.classes] if agentic != Undefined and trafficClasses != Undefined, "agentic and serving traffic classes must match"
        len(agentic.trafficClasses) == 0 if agentic != Undefined and trafficClasses == Undefined, "agentic and serving traffic classes must match"
