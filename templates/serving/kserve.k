import regex

# Catalog schemas. Values describe desired intent, not observed cluster state.
schema Artifact:
    uri: str
    check:
        uri != "", "artifact URI is required"

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
        name != "" and artifact != "" and hardware != "" and image != "", "deployment references and image are required"
        replicas > 0 and contextLength > 0 and maxNumSeqs > 0 and maxNumBatchedTokens > 0 and tensorParallelSize > 0, "deployment sizes must be positive"
        not packagedModel or (len(image.split("@sha256:")) == 2 and len(image.split("@sha256:")[1]) == 64 and regex.match(image.split("@sha256:")[1], "^[0-9a-f]{64}$")), "packaged model requires an immutable sha256 image digest"
        weight > 0, "route weight must be positive"

schema Gateway:
    name: str
    namespace: str
    hostname: str
    keySelector: {str:str}
    check:
        name != "" and namespace != "" and hostname != "", "gateway identity and hostname are required"
        len(keySelector) > 0, "a service-scoped key selector is required"

schema Service:
    name: str
    namespace: str
    gateway: Gateway
    deployments: [Deployment]
    # Version values are provenance, not installed or verified by this renderer.
    versions: {str:str}
    # These modes cannot be emitted until their trust and controller contracts are verified.
    agenticMode: str = "disabled"
    scalingMode: str = "fixed"
    trafficClasses: bool = False
    check:
        name != "" and namespace != "", "service identity is required"
        len(deployments) > 0, "at least one deployment is required"
        all d in deployments {d.contextLength == deployments[0].contextLength and d.features.toolParser == deployments[0].features.toolParser and d.features.reasoningParser == deployments[0].features.reasoningParser}, "weighted deployments must expose the same declared contract"
        len({d.name: True for d in deployments}) == len(deployments), "deployment names must be unique"
        "kserve" in versions and "agentgateway" in versions and "inferenceExtension" in versions and "vllm" in versions, "upstream version pins are required"
        versions["kserve"] != "" and versions["agentgateway"] != "" and versions["inferenceExtension"] != "" and versions["vllm"] != "", "version pins cannot be empty"
        agenticMode == "disabled", "agentic-api requires verified tenant isolation and trusted attribution; unsupported"
        scalingMode == "fixed", "KEDA/shared-pool integration is not yet implemented"
        not trafficClasses, "traffic class header trust and priority propagation are not yet implemented"
