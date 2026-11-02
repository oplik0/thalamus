# Run from the repository root: kcl test ./templates/serving
import json
import runtime
import templates.serving.raw as raw_serving
import templates.serving.kserve as raw_types
import templates.features.shared_pool as raw_pool_types
import templates.features.kv_offload as raw_picker

_raw_revision = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
_raw_image = "registry.example/model@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
_raw_digest = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
_raw_deployment = raw_types.Deployment {
    name: "model-a"
    artifact: "artifact"
    hardware: "hardware"
    image: _raw_image
    revision: _raw_revision
    rolloutStrategy: "Recreate"
    packagedModel: True
    replicas: 2
    contextLength: 8192
    maxNumSeqs: 32
    maxNumBatchedTokens: 2048
    tensorParallelSize: 2
    features: {prefixCaching: True, kvOffloadGi: 2, shmGi: 1, toolParser: "hermes", reasoningParser: "deepseek_r1"}
}
_raw_hardware = raw_types.Hardware {nodeSelector: {accelerator: "gpu"}, gpuCount: 2, cpu: "4", memoryGi: 16, hostMemoryGi: 32}
_raw_artifact = raw_types.Artifact {uri: "oci://${_raw_image}", modelDigest: _raw_digest, tokenizerDigest: _raw_digest}
_raw_service = {name: "public-model", namespace: "models", trafficClasses: Undefined}
_raw_pool = raw_pool_types.Pool {
    name: "shared"
    hardwareProfile: "hardware"
    nodeSelector: {accelerator: "gpu"}
    gpuPerNode: 2
    totalGpu: 4
    members: [{name: "model-a", service: "public-model", namespace: "models", gpuPerReplica: 2, prometheusQuery: "vllm_queue", threshold: "4"}]
    prometheusAddress: "http://prometheus:9090"
    versions: {kueue: "v0.18.0", keda: "2.20.2", "llm-d-router": "v1.0.0"}
}

# The normal workload is an actual Flux-owned Kubernetes Deployment with no
# unsupported KServe rollout field; the gateway pool selects its labelled Pods.
test_raw_recreate_workload_identity = lambda {
    items = raw_serving.render(_raw_service, _raw_deployment, _raw_artifact, _raw_hardware, Undefined)
    workload = [item for item in items if item.kind == "Deployment" and item.metadata.name == "model-a"][0]
    inference_pool = [item for item in items if item.kind == "InferencePool"][0]
    assert workload.apiVersion == "apps/v1"
    assert workload.metadata.namespace == "models"
    assert workload.spec.strategy.type == "Recreate"
    assert workload.spec.replicas == 2
    assert workload.metadata.annotations["thalamus.ai/serving-kind"] == "raw"
    assert workload.metadata.annotations["thalamus.ai/deployment"] == "model-a"
    assert workload.metadata.annotations["thalamus.ai/revision"] == _raw_revision
    assert workload.metadata.annotations["thalamus.ai/model-manifest"] == "model-a-manifest.0123456789abcdef0123456789abcdef.0123456789abcdef0123456789abcdef"
    assert workload.spec.template.metadata.annotations["thalamus.ai/revision"] == _raw_revision
    assert workload.spec.template.metadata.labels["thalamus.ai/deployment"] == inference_pool.spec.selector.matchLabels["thalamus.ai/deployment"]
    assert workload.spec.selector.matchLabels["thalamus.ai/pool-role"] == "normal"
    assert inference_pool.apiVersion == "inference.networking.k8s.io/v1"
    assert inference_pool.metadata.name == "model-a-inference-pool"
    assert inference_pool.spec.targetPorts[0].number == 8000
    assert inference_pool.spec.endpointPickerRef.name == "model-a-epp"
    assert inference_pool.spec.endpointPickerRef.port.number == 9002
    assert inference_pool.spec.endpointPickerRef.failureMode == "FailClose"
}

# Verify the engine's packaged model path, vLLM settings, bounded memory,
# offline environment, GPU resources, and tool/reasoning configuration.
test_raw_engine_contract = lambda {
    workload = [item for item in raw_serving.render(_raw_service, _raw_deployment, _raw_artifact, _raw_hardware, Undefined) if item.kind == "Deployment" and item.metadata.name == "model-a"][0]
    pod = workload.spec.template.spec
    engine = pod.containers[0]
    assert engine.image == _raw_image
    assert engine.command == ["vllm", "serve", "/opt/thalamus/model"]
    assert "--tokenizer" in engine.args and "/opt/thalamus/model" in engine.args
    assert "--kv-offloading-size" in engine.args and "native" in engine.args
    assert "--tool-call-parser" in engine.args and "hermes" in engine.args
    assert "--reasoning-parser" in engine.args and "deepseek_r1" in engine.args
    assert "--scheduling-policy" not in engine.args
    assert engine.resources.requests["nvidia.com/gpu"] == "2"
    assert engine.resources.requests.memory == "20Gi"
    assert engine.resources.requests == engine.resources.limits
    assert pod.volumes[0].emptyDir.sizeLimit == "5Gi"
    assert engine.readinessProbe.httpGet.path == "/health"
    assert engine.securityContext.readOnlyRootFilesystem
    assert [x.value for x in engine.env if x.name == "HF_HUB_OFFLINE"][0] == "1"
    assert not pod.automountServiceAccountToken
}

# Pinned upstream EPP uses the administrator-provisioned namespaced identity,
# gRPC service and native config for the same pool; no proxy is installed here.
test_raw_endpoint_picker_contract = lambda {
    items = raw_serving.render(_raw_service, _raw_deployment, _raw_artifact, _raw_hardware, Undefined)
    epp = [item for item in items if item.kind == "Deployment" and item.metadata.name == "model-a-epp"][0]
    svc = [item for item in items if item.kind == "Service" and item.metadata.name == "model-a-epp"][0]
    picker = epp.spec.template.spec.containers[0]
    config = json.decode(picker.args[-1])
    assert picker.image == "ghcr.io/llm-d/llm-d-router-endpoint-picker:v1.0.0"
    assert picker.command == ["/app/epp"]
    assert "--config-text" in picker.args and "--secure-serving=false" in picker.args
    assert "model-a-inference-pool" in picker.args and "models" in picker.args
    assert "--grpc-port=${svc.spec.ports[0].targetPort}" in picker.args
    grpc_port = [port.containerPort for port in picker.ports if port.name == "grpc"][0]
    assert svc.spec.ports[0].targetPort == grpc_port
    assert "--grpc-port=${grpc_port}" in raw_picker.scheduler("models", "model-a").template.containers[0].command
    assert config.apiVersion == "llm-d.ai/v1" and config.kind == "EndpointPickerConfig"
    assert "prefix-cache-scorer" in [p.type for p in config.plugins]
    assert "load-aware-scorer" in [p.type for p in config.plugins]
    assert svc.spec.selector == epp.spec.template.metadata.labels
    assert svc.spec.ports[0].port == 9002 and svc.spec.ports[0].appProtocol == "kubernetes.io/h2c"
    assert svc.spec.ports[1].name == "http-metrics" and svc.spec.ports[1].port == 9090
    assert epp.spec.template.spec.serviceAccountName == "thalamus-epp"
    assert len([item for item in items if item.kind in ["ServiceAccount", "Role", "RoleBinding", "ClusterRole", "ClusterRoleBinding"]]) == 0
}

test_raw_configuration_change_updates_epp_pod_spec = lambda {
    cachedFeatures = raw_types.Features {**_raw_deployment.features, kvOffloadGi = 0}
    cached = {**_raw_deployment, features = cachedFeatures}
    withoutCache = {**_raw_deployment, features = raw_types.Features {**cachedFeatures, prefixCaching = False}}
    original = [item for item in raw_serving.render(_raw_service, cached, _raw_artifact, _raw_hardware, Undefined) if item.kind == "Deployment" and item.metadata.name == "model-a-epp"][0]
    updated = [item for item in raw_serving.render(_raw_service, withoutCache, _raw_artifact, _raw_hardware, Undefined) if item.kind == "Deployment" and item.metadata.name == "model-a-epp"][0]
    assert original.spec.template.spec.containers[0].args != updated.spec.template.spec.containers[0].args
    assert "prefix-cache-scorer" not in [p.type for p in json.decode(updated.spec.template.spec.containers[0].args[-1]).plugins]
}

test_raw_rejects_nonpackaged_model = lambda {
    baseline = raw_serving.render(_raw_service, _raw_deployment, _raw_artifact, _raw_hardware, Undefined)
    assert any item in baseline {item.kind == "Deployment" and item.metadata.name == "model-a"}
    d = {**_raw_deployment, packagedModel = False}
    err = runtime.catch(lambda { items = raw_serving.render(_raw_service, d, _raw_artifact, _raw_hardware, Undefined) })
    assert err != Undefined
}

test_raw_rejects_mutable_image_and_revision = lambda {
    baseline = raw_serving.render(_raw_service, _raw_deployment, _raw_artifact, _raw_hardware, Undefined)
    assert any item in baseline {item.kind == "Deployment" and item.metadata.name == "model-a"}
    mutable = {**_raw_deployment, image = "registry.example/model:latest"}
    mutableArtifact = {**_raw_artifact, uri = "oci://${mutable.image}"}
    badRevision = {**_raw_deployment, revision = "latest"}
    badImage = runtime.catch(lambda { items = raw_serving.render(_raw_service, mutable, mutableArtifact, _raw_hardware, Undefined) })
    badRev = runtime.catch(lambda { items = raw_serving.render(_raw_service, badRevision, _raw_artifact, _raw_hardware, Undefined) })
    assert badImage != Undefined
    assert badRev != Undefined
}

test_raw_rejects_mismatched_artifact_and_hardware = lambda {
    baseline = raw_serving.render(_raw_service, _raw_deployment, _raw_artifact, _raw_hardware, Undefined)
    assert any item in baseline {item.kind == "Deployment" and item.metadata.name == "model-a"}
    badArtifact = {**_raw_artifact, uri = "oci://different@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"}
    insufficient = {**_raw_hardware, gpuCount = 1}
    badUri = runtime.catch(lambda { items = raw_serving.render(_raw_service, _raw_deployment, badArtifact, _raw_hardware, Undefined) })
    badHardware = runtime.catch(lambda { items = raw_serving.render(_raw_service, _raw_deployment, _raw_artifact, insufficient, Undefined) })
    assert badUri != Undefined
    assert badHardware != Undefined
}

test_raw_rejects_invalid_rollout_and_memory = lambda {
    baseline = raw_serving.render(_raw_service, _raw_deployment, _raw_artifact, _raw_hardware, Undefined)
    assert any item in baseline {item.kind == "Deployment" and item.metadata.name == "model-a"}
    rolling = {**_raw_deployment, rolloutStrategy = "RollingUpdate"}
    unifiedMemory = {**_raw_hardware, sharedGpuMemory = True}
    lowMemory = {**_raw_hardware, hostMemoryGi = 18}
    badStrategy = runtime.catch(lambda { items = raw_serving.render(_raw_service, rolling, _raw_artifact, _raw_hardware, Undefined) })
    badUnified = runtime.catch(lambda { items = raw_serving.render(_raw_service, _raw_deployment, _raw_artifact, unifiedMemory, Undefined) })
    badMemory = runtime.catch(lambda { items = raw_serving.render(_raw_service, _raw_deployment, _raw_artifact, lowMemory, Undefined) })
    assert badStrategy != Undefined
    assert badUnified != Undefined
    assert badMemory != Undefined
}

# A shared pool has distinct normal/filler owners but one pool and one EPP.
# KEDA owns normal replicas; the filler occupies only the remainder of its
# configured nominal share and has the identical engine and model identity.
test_raw_shared_pool_uses_same_engine_and_picker = lambda {
    d = {**_raw_deployment, rolloutStrategy = "RollingUpdate"}
    items = raw_serving.render(_raw_service, d, _raw_artifact, _raw_hardware, _raw_pool)
    normal = [item for item in items if item.kind == "Deployment" and item.metadata.name == "model-a"][0]
    filler = [item for item in items if item.kind == "Deployment" and item.metadata.name == "model-a-filler"][0]
    inference_pool = [item for item in items if item.kind == "InferencePool"][0]
    scaled = [item for item in items if item.kind == "ScaledObject"][0]
    assert normal.spec.strategy.type == "RollingUpdate" and filler.spec.strategy.type == "RollingUpdate"
    assert normal.spec.replicas == Undefined and filler.spec.replicas == 1
    assert normal.spec.selector.matchLabels["thalamus.ai/pool-role"] == "normal"
    assert filler.spec.selector.matchLabels["thalamus.ai/pool-role"] == "filler"
    assert normal.spec.template.metadata.labels["thalamus.ai/deployment"] == inference_pool.spec.selector.matchLabels["thalamus.ai/deployment"]
    assert filler.spec.template.metadata.labels["thalamus.ai/deployment"] == inference_pool.spec.selector.matchLabels["thalamus.ai/deployment"]
    assert normal.spec.template.metadata.labels["kueue.x-k8s.io/queue-name"] == "shared-model-a"
    assert filler.spec.template.metadata.labels["kueue.x-k8s.io/queue-name"] == "shared-model-a-filler"
    assert normal.spec.template.spec.priorityClassName == "shared-normal"
    assert filler.spec.template.spec.priorityClassName == "shared-filler"
    assert filler.spec.template.spec.containers[0].image == _raw_image
    assert filler.spec.template.spec.containers[0].command == ["vllm", "serve", "/opt/thalamus/model"]
    assert filler.spec.template.spec.containers[0].resources.requests["nvidia.com/gpu"] == "2"
    assert filler.metadata.annotations["thalamus.ai/revision"] == _raw_revision
    assert filler.metadata.annotations["thalamus.ai/model-manifest"] == "model-a-manifest.0123456789abcdef0123456789abcdef.0123456789abcdef0123456789abcdef"
    assert scaled.spec.scaleTargetRef.name == "model-a"
    assert len([item for item in items if item.kind == "InferencePool"]) == 1
    assert len([item for item in items if item.kind == "Service" and item.metadata.name == "model-a-epp"]) == 1
}

test_raw_recreate_rejects_independent_live_fillers = lambda {
    emptyPool = raw_pool_types.Pool {**_raw_pool, totalGpu = 2}
    items = raw_serving.render(_raw_service, _raw_deployment, _raw_artifact, _raw_hardware, emptyPool)
    filler = [item for item in items if item.kind == "Deployment" and item.metadata.name == "model-a-filler"][0]
    assert filler.spec.strategy.type == "Recreate" and filler.spec.replicas == 0
    err = runtime.catch(lambda { items = raw_serving.render(_raw_service, _raw_deployment, _raw_artifact, _raw_hardware, _raw_pool) })
    assert err != Undefined
}

test_raw_traffic_classes_configure_engine_and_epp = lambda {
    service = {
        **_raw_service
        trafficClasses = raw_types.TrafficClasses {
            ttl: "30s"
            classes: [raw_pool_types.TrafficClass {name: "interactive", priority: 10, enginePriority: -10, maxRequests: 100, maxBytes: "1Gi"}]
        }
    }
    items = raw_serving.render(service, _raw_deployment, _raw_artifact, _raw_hardware, Undefined)
    normal = [item for item in items if item.kind == "Deployment" and item.metadata.name == "model-a"][0]
    epp = [item for item in items if item.kind == "Deployment" and item.metadata.name == "model-a-epp"][0]
    config = json.decode(epp.spec.template.spec.containers[0].args[-1])
    assert normal.spec.template.metadata.labels["llm-d.ai/engine-type"] == "vllm"
    assert "--scheduling-policy" in normal.spec.template.spec.containers[0].args
    assert "priority" in normal.spec.template.spec.containers[0].args
    assert "flowControl" in config.featureGates
    assert config.flowControl.defaultRequestTTL == "30s"
    assert config.flowControl.priorityBands[0].priority == 10
    assert "prefix-cache-scorer" in [p.type for p in config.plugins]
}

# A rollout candidate has its own inference pool and selector, but consumes the
# existing logical member's normal queue. It never creates filler or an HPA.
test_raw_pooled_candidate_reuses_member_queue = lambda {
    candidate = {**_raw_deployment, name = "candidate", role = "candidate", poolMember = "model-a", replicas = 1, weight = 0}
    items = raw_serving.render(_raw_service, candidate, _raw_artifact, _raw_hardware, _raw_pool)
    workload = [item for item in items if item.kind == "Deployment" and item.metadata.name == "candidate"][0]
    inference_pool = [item for item in items if item.kind == "InferencePool"][0]
    assert workload.spec.replicas == 1
    assert workload.spec.strategy.type == "Recreate"
    assert workload.spec.template.metadata.labels["thalamus.ai/deployment"] == "candidate"
    assert workload.spec.template.metadata.labels["kueue.x-k8s.io/queue-name"] == "shared-model-a"
    assert workload.spec.template.spec.priorityClassName == "shared-normal"
    assert inference_pool.metadata.name == "candidate-inference-pool"
    assert inference_pool.spec.selector.matchLabels["thalamus.ai/deployment"] == "candidate"
    assert len([item for item in items if item.kind == "ScaledObject"]) == 0
    assert len([item for item in items if item.kind == "Deployment" and item.metadata.name == "candidate-filler"]) == 0
}

test_raw_pooled_promotion_and_retention = lambda {
    promoted = {**_raw_deployment, name = "candidate", poolMember = "model-a", role = "stable", weight = 100, rolloutStrategy = "RollingUpdate"}
    retained = {**_raw_deployment, role = "stable", weight = 0}
    promotedItems = raw_serving.render(_raw_service, promoted, _raw_artifact, _raw_hardware, _raw_pool)
    retainedItems = raw_serving.render(_raw_service, retained, _raw_artifact, _raw_hardware, _raw_pool)
    primary = [item for item in promotedItems if item.kind == "Deployment" and item.metadata.name == "candidate"][0]
    filler = [item for item in promotedItems if item.kind == "Deployment" and item.metadata.name == "candidate-filler"][0]
    scaled = [item for item in promotedItems if item.kind == "ScaledObject"][0]
    old = [item for item in retainedItems if item.kind == "Deployment" and item.metadata.name == "model-a"][0]
    assert primary.spec.replicas == Undefined and filler.spec.replicas == 1
    assert primary.spec.template.metadata.labels["kueue.x-k8s.io/queue-name"] == "shared-model-a"
    assert filler.spec.template.metadata.labels["kueue.x-k8s.io/queue-name"] == "shared-model-a-filler"
    assert scaled.spec.scaleTargetRef.name == "candidate"
    assert scaled.metadata.name == "shared-model-a"
    assert old.spec.replicas == 1 and old.spec.strategy.type == "Recreate"
    assert len([item for item in retainedItems if item.kind == "ScaledObject"]) == 0
    assert len([item for item in retainedItems if item.kind == "Deployment" and item.metadata.name == "model-a-filler"]) == 0
}

test_raw_pool_member_cannot_cross_service_or_namespace = lambda {
    candidate = {**_raw_deployment, name = "candidate", role = "candidate", poolMember = "model-a", replicas = 1, weight = 0}
    baseline = raw_serving.render(_raw_service, candidate, _raw_artifact, _raw_hardware, _raw_pool)
    assert any item in baseline {item.kind == "Deployment" and item.metadata.name == "candidate"}
    foreignService = {**_raw_service, name = "foreign"}
    foreign = {**_raw_service, namespace = "other"}
    unknown = {**candidate, poolMember = "missing"}
    wrongNamespace = runtime.catch(lambda { items = raw_serving.render(foreign, candidate, _raw_artifact, _raw_hardware, _raw_pool) })
    wrongService = runtime.catch(lambda { items = raw_serving.render(foreignService, candidate, _raw_artifact, _raw_hardware, _raw_pool) })
    wrongMember = runtime.catch(lambda { items = raw_serving.render(_raw_service, unknown, _raw_artifact, _raw_hardware, _raw_pool) })
    assert wrongNamespace != Undefined
    assert wrongService != Undefined
    assert wrongMember != Undefined
}

test_raw_pooled_candidate_limits_runtime_not_catalog_intent = lambda {
    candidate = {**_raw_deployment, name = "candidate", role = "candidate", poolMember = "model-a", replicas = 2, weight = 0}
    items = raw_serving.render(_raw_service, candidate, _raw_artifact, _raw_hardware, _raw_pool)
    workload = [item for item in items if item.kind == "Deployment" and item.metadata.name == "candidate"][0]
    assert workload.spec.replicas == 1
}
