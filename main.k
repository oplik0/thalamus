# Render from the repository root:
#   kcl main.k -D service_file=catalog/services/example-service.yaml
# Optional -D artifacts_file=... and -D hardware_file=... override catalog paths.
# Output is a Kubernetes List. Rendering does not install or verify controllers,
# images, CRDs, policies, gateway listener permissions, networking or model files.
# Example data is illustrative, not imported GitOps history. Thalamus edits only
# catalog data; templates and this entrypoint are reviewed as code.
# Upstream guide and tested composition (operator must select compatible pins):
# https://agentgateway.dev/docs/kubernetes/latest/integrations/llm/routing/kserve/
# https://agentgateway.dev/docs/kubernetes/latest/documentation/security/apikey/
# https://docs.vllm.ai/en/stable/configuration/engine_args/
# KServe owns the scheduler and InferencePool, but not the route. Its documented
# pool naming convention must match the pinned controller. No cluster is contacted.
# This route authenticates a service-scoped key set and requires a user UUID.
# A personal key may have a null project; other keys require a project UUID.
# It does not enforce individual metadata permissions, expiry, quotas or team
# isolation. The operator must restrict the selector and ConfigMap writes to
# trusted projections, ensure issuance/revocation, Gateway allowedRoutes and
# backend network isolation, and verify the policy is fully valid (not
# PartiallyValid) and effective before exposing the Gateway. List ordering is not
# an atomic rollout or admission guard.
# Optional packagedModel mode is Expected only, not a runtime attestation.
# The exact image digest contains engine, model and tokenizer at
# /opt/thalamus/model; verify image layers and symlinks independently. The
# runner must authenticate the gateway and actual answering backend separately.
# Read-only root and offline flags do not prove runtime or network isolation.
# x-thalamus-backend names the selected gateway backend, not a pod or
# cryptographic attestation. Validation runners must authenticate the gateway
# and independently observe its policy and pool membership.
import file
import json
import yaml
import templates.serving.kserve as serving

_service_file = option("service_file", required=True)
_artifacts_file = option("artifacts_file", default="catalog/artifacts.yaml")
_hardware_file = option("hardware_file", default="catalog/hardware.yaml")
_service: serving.Service = yaml.decode(file.read(_service_file))
_artifacts: {str:serving.Artifact} = yaml.decode(file.read(_artifacts_file))
_hardware: {str:serving.Hardware} = yaml.decode(file.read(_hardware_file))
assert all d in _service.deployments {d.artifact in _artifacts and d.hardware in _hardware}, "unknown artifact or hardware reference"
assert all d in _service.deployments {d.tensorParallelSize <= _hardware[d.hardware].gpuCount}, "tensor parallel size exceeds GPUs per node"
assert all d in _service.deployments {d.features.kvOffloadGi + d.features.shmGi < _hardware[d.hardware].memoryGi}, "KV offload and /dev/shm must fit within pod memory with headroom"
assert all d in _service.deployments {not _hardware[d.hardware].sharedGpuMemory or d.features.kvOffloadGi == 0}, "KV offload on shared GPU/host memory requires validated memory accounting"
assert all d in _service.deployments {not d.packagedModel or _artifacts[d.artifact].uri == "oci://${d.image}"}, "packaged model artifact URI must identify the exact serving image digest"

apiVersion = "v1"
kind = "List"
items = [
    {
        apiVersion: "agentgateway.dev/v1alpha1"
        kind: "AgentgatewayPolicy"
        metadata: {name: "${_service.name}-auth", namespace: _service.namespace}
        spec: {
            targetRefs: [{group: "gateway.networking.k8s.io", kind: "HTTPRoute", name: _service.name}]
            traffic: {
                apiKeyAuthentication: {mode: "Strict", configMapSelector: {matchLabels: _service.gateway.keySelector}}
                authorization: {action: "Require", policy: {matchExpressions: [
                    'type(apiKey.user) == string && apiKey.user.matches("^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")',
                    'type(apiKey.kind) == string && apiKey.kind in ["personal", "project", "job"]',
                    '(type(apiKey.project) == string && apiKey.project.matches("^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")) || (apiKey.kind == "personal" && apiKey.project == null)'
                ]}}
                # Set overwrites client values. In agentgateway v1.7.0 a null
                # project removes the existing header; verify the selected gateway
                # version preserves this behavior. The projection has no traffic
                # class, so never pass through team/class attribution from callers.
                transformation: {request: {
                    remove: ["x-api-key", "connection", "x-thalamus-team", "x-thalamus-class", "x-thalamus-backend"]
                    set: [
                        {name: "x-thalamus-user", value: "apiKey.user"},
                        {name: "x-thalamus-project", value: "apiKey.project"},
                    ]
                }}
            }
        }
    }
] + [{
    apiVersion: "serving.kserve.io/v1alpha2"
    kind: "LLMInferenceService"
    metadata: {name: d.name, namespace: _service.namespace}
    spec: {
        model: {uri: _artifacts[d.artifact].uri, name: _service.name}
        storageInitializer: {enabled: not d.packagedModel}
        replicas: d.replicas
        router: {scheduler: {}}
        template: {
            nodeSelector: _hardware[d.hardware].nodeSelector
            volumes: [{name: "shm", emptyDir: {medium: "Memory", sizeLimit: "${d.features.shmGi}Gi"}}] + ([{name: "tmp", emptyDir: {}}] if d.packagedModel else [])
            containers: [{
                name: "main"
                image: d.image
                command: (["vllm", "serve", "/opt/thalamus/model"] if d.packagedModel else Undefined)
                env: ([{name: "HOME", value: "/tmp"}, {name: "HF_HOME", value: "/tmp/huggingface"}, {name: "HF_HUB_OFFLINE", value: "1"}, {name: "TRANSFORMERS_OFFLINE", value: "1"}] if d.packagedModel else Undefined)
                securityContext: ({readOnlyRootFilesystem: True, allowPrivilegeEscalation: False, capabilities: {drop: ["ALL"]}} if d.packagedModel else Undefined)
                args: [
                    "--served-model-name", _service.name,
                    "--max-model-len", "${d.contextLength}",
                    "--max-num-seqs", "${d.maxNumSeqs}",
                    "--max-num-batched-tokens", "${d.maxNumBatchedTokens}",
                    "--tensor-parallel-size", "${d.tensorParallelSize}",
                ] + (["--tokenizer", "/opt/thalamus/model"] if d.packagedModel else []) + (["--enable-prefix-caching"] if d.features.prefixCaching else ["--no-enable-prefix-caching"]) + (["--kv-offloading-size", "${d.features.kvOffloadGi}", "--kv-offloading-backend", "native"] if d.features.kvOffloadGi > 0 else []) + (["--enable-auto-tool-choice", "--tool-call-parser", d.features.toolParser] if d.features.toolParser != "" else []) + (["--reasoning-parser", d.features.reasoningParser] if d.features.reasoningParser != "" else [])
                volumeMounts: [{name: "shm", mountPath: "/dev/shm"}] + ([{name: "tmp", mountPath: "/tmp"}] if d.packagedModel else [])
                resources: {limits: {"nvidia.com/gpu": "${d.tensorParallelSize}", cpu: _hardware[d.hardware].cpu, memory: "${_hardware[d.hardware].memoryGi}Gi"}}
            }]
        }
    }
} for d in _service.deployments] + [{
    apiVersion: "agentgateway.dev/v1alpha1"
    kind: "AgentgatewayBackend"
    metadata: {name: "${d.name}-backend", namespace: _service.namespace}
    spec: {
        ai: {provider: {custom: {
            backendRef: {group: "inference.networking.k8s.io", kind: "InferencePool", name: "${d.name}-inference-pool"}
            model: _service.name
            formats: [{type: "Completions", path: "/v1/chat/completions"}]
        }}}
        policies: {transformation: {response: {set: [{name: "x-thalamus-backend", value: json.encode(d.name)}]}}}
    }
} for d in _service.deployments] + [{
    apiVersion: "gateway.networking.k8s.io/v1"
    kind: "HTTPRoute"
    metadata: {name: _service.name, namespace: _service.namespace}
    spec: {
        parentRefs: [{group: "gateway.networking.k8s.io", kind: "Gateway", name: _service.gateway.name, namespace: _service.gateway.namespace}]
        hostnames: [_service.gateway.hostname]
        rules: [{
            matches: [{path: {type: "Exact", value: "/v1/chat/completions"}}]
            backendRefs: [{group: "agentgateway.dev", kind: "AgentgatewayBackend", name: "${d.name}-backend", weight: d.weight} for d in _service.deployments]
        }]
    }
}]
