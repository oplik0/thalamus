# Native vLLM CPU KV offload and the llm-d Router v1 endpoint picker.
#
# These are desired settings, not evidence that the controllers, image, model,
# driver, memory topology, or gateway are compatible in the target cluster.
# KServe v0.21.0 resolves scheduler.config.ref into --config-text and reconciles
# the scheduler Deployment when its ConfigMap changes. It continues to own the
# InferencePool, EPP Service, scheduler Deployment, and scheduler ServiceAccount.
#
# https://github.com/vllm-project/vllm/blob/v0.23.0/vllm/config/cache.py
# https://github.com/vllm-project/vllm/issues/53042
# https://github.com/llm-d/llm-d-router/blob/v1.0.0/deploy/config/epp-estimate-prefix-cache-config.yaml
# https://github.com/kserve/kserve/blob/v0.21.0/pkg/controller/v1alpha2/llmisvc/scheduler.go
import json

kv_router_image = "ghcr.io/llm-d/llm-d-router-endpoint-picker:v1.0.0"
kv_config_key = "endpoint-picker.yaml"

# vLLM's size is the total pool across the tensor-parallel ranks, not per GPU.
offload_args = lambda kv: int -> [str] {
    assert kv >= 0, "KV offload size cannot be negative"
    ["--kv-offloading-size", "${kv}", "--kv-offloading-backend", "native"] if kv > 0 else []
}

# memoryGi is the non-offload baseline, including ordinary /dev/shm use. Pinned
# allocation can round above the nominal pool; reserve twice the requested size
# in addition to the baseline (issue #53042). This is headroom, not a guarantee
# against model- or allocator-specific OOM. The operator must allow separately
# for node OS, other pods, and shared device memory, and benchmark actual usage.
# Kubernetes requests should equal this limit to prevent scheduler overcommit.
pod_memory_gi = lambda kv: int, shm: int, baseline: int, host: int, shared: bool -> int {
    assert kv >= 0 and shm > 0 and baseline > 0, "invalid KV, shared memory, or baseline size"
    assert shm < baseline, "baseline memory must leave headroom beyond shared memory"
    assert not shared or kv == 0, "KV offload on shared GPU/host memory requires validated memory accounting"
    assert baseline + 2 * kv <= host, "KV offload and baseline memory exceed host capacity"
    baseline + 2 * kv
}

# emptyDir.sizeLimit is a ceiling, not a separate memory reservation. Native
# CPU tensors are pinned host memory; depending on the vLLM implementation they
# need not be files in /dev/shm. Growing the mount also accommodates versions
# using shared-memory-backed offload without counting that pool twice.
shm_gi = lambda kv: int, shm: int -> int {
    assert kv >= 0 and shm > 0, "invalid KV or shared memory size"
    shm + 2 * kv
}

# Approximate locality uses a tokenizer-free in-memory index. It assumes that
# recently routed prefixes are cached; it does not observe actual GPU/CPU tier
# residency or eviction. With offload enabled retain a larger heuristic index,
# rather than auto-tuning it solely to reported GPU-cache capacity. Flow-control
# and locality plugins share one config, so neither feature replaces the other.
config = lambda cacheEnabled: bool, offloadGi: int, flow: any -> any {
    assert offloadGi >= 0, "KV offload size cannot be negative"
    assert offloadGi == 0 or cacheEnabled, "KV offload requires prefix caching"
    {
        apiVersion: "llm-d.ai/v1"
        kind: "EndpointPickerConfig"
        plugins: [
            {type: "single-profile-handler"},
            {type: "decode-filter"},
            {type: "load-aware-scorer"},
            {type: "max-score-picker"},
        ] + ([
            {type: "approx-prefix-cache-producer", parameters: {autoTune: False, blockSizeTokens: 64, lruCapacityPerServer: (62500 if offloadGi > 0 else 31250)}},
            {type: "prefix-cache-scorer"},
        ] if cacheEnabled else []) + (flow.plugins if flow != Undefined else [])
        schedulingProfiles: [{name: "default", plugins: [
            {pluginRef: "decode-filter"},
            {pluginRef: "load-aware-scorer", weight: 1.0},
        ] + ([{pluginRef: "prefix-cache-scorer", weight: 1.0}] if cacheEnabled else []) + [
            {pluginRef: "max-score-picker"},
        ]}]
        featureGates: (flow.featureGates if flow != Undefined else Undefined)
        requestHandler: (flow.requestHandler if flow != Undefined else Undefined)
        flowControl: (flow.flowControl if flow != Undefined else Undefined)
    }
}

# EndpointPickerConfig is a file format, not an independently applied CRD.
# JSON is valid YAML and preserves the exact object passed to the EPP.
resources = lambda namespace: str, deployment: str, cfg: any -> [any] {
    assert namespace != "" and deployment != "", "scheduler identity is required"
    assert cfg.apiVersion == "llm-d.ai/v1" and cfg.kind == "EndpointPickerConfig", "invalid endpoint picker configuration"
    [{
        apiVersion: "v1"
        kind: "ConfigMap"
        metadata: {name: "${deployment}-epp-config", namespace: namespace}
        data: {"endpoint-picker.yaml": json.encode(cfg)}
    }]
}

# KServe v0.21's preset uses router v0.10 and a command tailored to that
# binary. Override both image and command for v1; preserve the preset's
# ports, probes, security context, and ownership. Agentgateway's documented
# EPP integration uses plaintext ext_proc; isolate it with network controls.
# The controller appends --config-text from config.ref and handles updates.
# Before use, configure KServe's scheduler preset with a native Kubernetes
# preStop.sleep action instead of exec /bin/sleep: the v1 EPP image is
# distroless. A per-service lifecycle overlay does not reliably delete the
# preset's exec handler during KServe's strategic merge.
scheduler = lambda namespace: str, deployment: str -> any {
    assert namespace != "" and deployment != "", "scheduler identity is required"
    {
        annotations: {"app.kubernetes.io/version": "1.0.0"}
        config: {ref: {name: "${deployment}-epp-config", key: "endpoint-picker.yaml"}}
        template: {containers: [{
            name: "main"
            image: kv_router_image
            command: [
                "/app/epp",
                "--pool-name", "${deployment}-inference-pool",
                "--pool-namespace", namespace,
                "--zap-encoder", "json",
                "--grpc-port=9002",
                "--grpc-health-port", "9003",
                "--secure-serving=false",
                "--metrics-endpoint-auth=false",
            ]
        }]}
    }
}
