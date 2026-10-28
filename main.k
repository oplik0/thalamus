# Render from the repository root using the root entrypoint:
# kcl main.k -D service_file=catalog/services/example-service.yaml
# Optional artifacts_file and hardware_file paths are relative to the module root
# and must stay inside it. CI must invoke main.k, not templates/main.k, so that
# KCL uses the repository root as the file.read sandbox.
# This example requires a compatible KServe LLMInferenceService controller, its
# managed gateway/scheduler dependencies and a vLLM-compatible image. No upstream
# versions are asserted or pinned here; verify the composition in your environment.
# KServe owns routing and creates the HTTPRoute and InferencePool. This baseline
# is one deployment per service, fixed replicas and a single GPU node per replica;
# it is not a multi-variant, authenticated, or production-ready gateway policy.
# The artifact URI must resolve to weights accessible to KServe; ensure the URI
# itself refers to an immutable revision if reproducibility is required.
import file
import yaml
import templates.serving.kserve as serving
import templates.gateway.routes as routes

_service_file = option("service_file", required=True)
_artifacts_file = option("artifacts_file", default="catalog/artifacts.yaml")
_hardware_file = option("hardware_file", default="catalog/hardware.yaml")
_service: serving.Service = yaml.decode(file.read(_service_file))
_artifacts = yaml.decode(file.read(_artifacts_file))
_hardware_profiles = yaml.decode(file.read(_hardware_file))
_artifact: serving.Artifact = _artifacts[_service.artifact]
_hardware: serving.Hardware = _hardware_profiles[_service.hardware]
assert _service.tensorParallelSize <= _hardware.gpuCount, "tensor parallel size exceeds GPUs per replica"

apiVersion = "serving.kserve.io/v1alpha1"
kind = "LLMInferenceService"
metadata = {
    name: _service.name
    namespace: _service.namespace
}
spec = {
    model: {
        uri: _artifact.uri
        name: _service.name
    }
    replicas: _service.replicas
    template: {
        nodeSelector: _hardware.nodeSelector
        containers: [{
            name: "main"
            image: _service.image
            args: [
                "--served-model-name", _service.name,
                "--max-model-len", "${_service.contextLength}",
                "--max-num-seqs", "${_service.maxNumSeqs}",
                "--max-num-batched-tokens", "${_service.maxNumBatchedTokens}",
                "--tensor-parallel-size", "${_service.tensorParallelSize}",
            ] + (["--enable-auto-tool-choice", "--tool-call-parser", _service.features.toolParser] if _service.features.toolParser != "" else []) + (["--reasoning-parser", _service.features.reasoningParser] if _service.features.reasoningParser != "" else [])
            resources: {
                limits: {
                    "nvidia.com/gpu": "${_hardware.gpuCount}"
                    cpu: _hardware.cpu
                    memory: _hardware.memory
                }
            }
        }]
    }
    router: routes.router
}
