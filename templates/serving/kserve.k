# Intent schemas for the single-node vLLM / KServe template.
schema Artifact:
    uri: str
    check:
        uri != "", "artifact URI is required"

schema Hardware:
    nodeSelector: {str:str}
    gpuCount: int
    cpu: str
    memory: str
    check:
        len(nodeSelector) > 0, "hardware node selector is required"
        gpuCount > 0, "GPU count must be positive"
        cpu != "", "CPU quantity is required"
        memory != "", "memory quantity is required"

schema Features:
    toolParser: str = ""
    reasoningParser: str = ""

schema Service:
    name: str
    namespace: str
    artifact: str
    hardware: str
    image: str
    replicas: int
    contextLength: int
    maxNumSeqs: int
    maxNumBatchedTokens: int
    tensorParallelSize: int
    features: Features
    check:
        name != "", "service name is required"
        namespace != "", "namespace is required"
        artifact != "", "artifact reference is required"
        hardware != "", "hardware reference is required"
        image != "", "image is required"
        replicas > 0, "replicas must be positive"
        contextLength > 0, "context length must be positive"
        maxNumSeqs > 0, "maxNumSeqs must be positive"
        maxNumBatchedTokens > 0, "maxNumBatchedTokens must be positive"
        tensorParallelSize > 0, "tensor parallel size must be positive"
