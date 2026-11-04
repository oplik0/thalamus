# Run from the repository root: kcl test ./templates/serving
# These tests inspect rendered policies, not CNI enforcement or live labels.
import runtime
import templates.features.shared_pool as network_test_pool

_network_test_revision = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
_network_test_image = "registry.example/model@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
_network_test_digest = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
_network_test_gateway = {name: "gateway", namespace: "models", hostname: "example.invalid", keySelector: {"thalamus.dev/gateway-keys": "true"}, rateLimit: "shared"}
_network_test_config = ServingNetwork {
    gatewayPodSelector: {"gateway.networking.k8s.io/gateway-name": "gateway"}
    dns: {namespaceSelector: {"kubernetes.io/metadata.name": "kube-system"}, podSelector: {"k8s-app": "kube-dns"}}
    prometheus: {namespaceSelector: {"kubernetes.io/metadata.name": "monitoring"}, podSelector: {"app.kubernetes.io/name": "prometheus"}}
    apiServer: [{cidr: "192.0.2.0/24", ports: [443, 6443]}]
    storageEgress: [{cidr: "198.51.100.0/24", ports: [443]}]
}
_network_test_stable = Deployment {name: "stable", artifact: "model", hardware: "gpu", image: _network_test_image, revision: _network_test_revision, packagedModel: True, rolloutStrategy: "RollingUpdate"}
_network_test_candidate = Deployment {**_network_test_stable, name = "candidate", role = "candidate", poolMember = "stable", replicas = 2}
_network_test_artifacts: {str:Artifact} = {model: {uri: "oci://${_network_test_image}", modelDigest: _network_test_digest, tokenizerDigest: _network_test_digest}}
_network_test_hardware = Hardware {nodeSelector: {accelerator: "gpu"}, gpuCount: 1, cpu: "4", memoryGi: 16, hostMemoryGi: 32}
_network_test_base = {name: "svc", namespace: "models", access: {teams: []}, gateway: _network_test_gateway, network: _network_test_config, versions: {kserve: "v0.21.0", agentgateway: "v1.7.0", inferenceExtension: "v1.6.2", vllm: "v0.23.0"}}
_network_test_private_gateway = {"gateway.networking.k8s.io/gateway-name": "agentic-internal"}
_network_test_agentic = {
    name: "svc"
    namespace: "models"
    mode: "shared-stateless"
    image: _network_test_image
    upstreamCommit: "27e60cf006cb771cf1961ea9909e64d812f5f3c8"
    gatewayVersion: "v1.7.0"
    publicGateway: "gateway"
    publicGatewayNamespace: "models"
    internalGateway: "agentic-internal"
    internalGatewayNamespace: "models"
    internalGatewayService: "agentic-internal"
    internalGatewayTargetPort: 443
    internalHostname: "agentic-internal.models.svc.cluster.local"
    internalCAConfigMap: "thalamus-agentic-internal-ca"
    gatewayNamespaceLabels: {"kubernetes.io/metadata.name": "models"}
    gatewayPodLabels: {"gateway.networking.k8s.io/gateway-name": "gateway"}
    dnsNamespaceLabels: {"kubernetes.io/metadata.name": "kube-system"}
    dnsPodLabels: {"k8s-app": "kube-dns"}
    internalGatewayPodLabels: _network_test_private_gateway
    externalMcp: []
    trafficClasses: []
    instances: [{
        name: "chat"
        hostname: "agentic.example.invalid"
        databaseSecret: "agentic-db"
        databaseCidrs: ["192.0.2.20/32"]
        keySelector: {"thalamus.dev/gateway-keys": "true"}
    }]
}
_network_test_service = Service {**_network_test_base, deployments: [_network_test_stable]}
_network_test_pooled_service = Service {**_network_test_base, pool: "shared", deployments: [_network_test_stable, _network_test_candidate]}
_network_test_pool = network_test_pool.Pool {
    name: "shared"
    hardwareProfile: "gpu"
    nodeSelector: {accelerator: "gpu"}
    gpuPerNode: 1
    totalGpu: 2
    prometheusAddress: "http://prometheus:9090"
    versions: {kueue: "v0.18.0", keda: "2.20.2", "llm-d-router": "v1.0.0"}
    members: [{name: "stable", service: "svc", namespace: "models", gpuPerReplica: 1, prometheusQuery: "queue", threshold: "1"}]
}

_network_test_find = lambda items: [any], name: str -> any {
    matches = [item for item in items if item.kind == "NetworkPolicy" and item.metadata.name == name]
    assert len(matches) == 1, "expected exactly one serving NetworkPolicy"
    matches[0]
}

# All selectors refer to the actual raw Pods, including a pooled filler and
# a separate candidate; no selector can include another deployment by name.
test_network_raw_pooled_workload_selectors = lambda {
    items = network_resources(_network_test_pooled_service, _network_test_artifacts, _network_test_pool)
    stable = render(_network_test_pooled_service, _network_test_stable, _network_test_artifacts["model"], _network_test_hardware, _network_test_pool)
    candidate = render(_network_test_pooled_service, _network_test_candidate, _network_test_artifacts["model"], _network_test_hardware, _network_test_pool)
    primary = [item for item in stable if item.kind == "Deployment" and item.metadata.name == "stable"][0]
    filler = [item for item in stable if item.kind == "Deployment" and item.metadata.name == "stable-filler"][0]
    picker = [item for item in stable if item.kind == "Deployment" and item.metadata.name == "stable-epp"][0]
    candidatePod = [item for item in candidate if item.kind == "Deployment" and item.metadata.name == "candidate"][0]
    modelPolicy = _network_test_find(items, "serving-stable-model")
    pickerPolicy = _network_test_find(items, "serving-stable-picker")
    candidatePolicy = _network_test_find(items, "serving-candidate-model")
    assert all key in modelPolicy.spec.podSelector.matchLabels {primary.spec.template.metadata.labels[key] == modelPolicy.spec.podSelector.matchLabels[key] and filler.spec.template.metadata.labels[key] == modelPolicy.spec.podSelector.matchLabels[key]}
    assert all key in pickerPolicy.spec.podSelector.matchLabels {picker.spec.template.metadata.labels[key] == pickerPolicy.spec.podSelector.matchLabels[key]}
    assert all key in candidatePolicy.spec.podSelector.matchLabels {candidatePod.spec.template.metadata.labels[key] == candidatePolicy.spec.podSelector.matchLabels[key]}
    assert modelPolicy.spec.podSelector.matchLabels["thalamus.ai/deployment"] == "stable"
    assert candidatePolicy.spec.podSelector.matchLabels["thalamus.ai/deployment"] == "candidate"
    assert len(modelPolicy.spec.egress) == 0
    assert all item in items {item.metadata.namespace == "models" and not item.metadata.name.startswith("thalamus-")}
}

test_network_raw_recreate_selects_deployment_and_picker = lambda {
    d = Deployment {**_network_test_stable, rolloutStrategy = "Recreate"}
    service = Service {**_network_test_base, deployments: [d]}
    items = network_resources(service, _network_test_artifacts, Undefined)
    workloads = render(service, d, _network_test_artifacts["model"], _network_test_hardware, Undefined)
    model = [item for item in workloads if item.kind == "Deployment" and item.metadata.name == "stable"][0]
    picker = [item for item in workloads if item.kind == "Deployment" and item.metadata.name == "stable-epp"][0]
    modelPolicy = _network_test_find(items, "serving-stable-model")
    pickerPolicy = _network_test_find(items, "serving-stable-picker")
    assert model.spec.strategy.type == "Recreate"
    assert all key in modelPolicy.spec.podSelector.matchLabels {model.spec.template.metadata.labels[key] == modelPolicy.spec.podSelector.matchLabels[key]}
    assert all key in pickerPolicy.spec.podSelector.matchLabels {picker.spec.template.metadata.labels[key] == pickerPolicy.spec.podSelector.matchLabels[key]}
}

# The KServe controller constructs these exact v0.21 workload/scheduler labels.
# Components must remain distinct even though their application names coincide.
test_network_pinned_kserve_selectors = lambda {
    items = network_resources(_network_test_service, _network_test_artifacts, Undefined)
    model = _network_test_find(items, "serving-stable-model")
    picker = _network_test_find(items, "serving-stable-picker")
    assert model.spec.podSelector.matchLabels["app.kubernetes.io/name"] == "stable"
    assert model.spec.podSelector.matchLabels["app.kubernetes.io/part-of"] == "llminferenceservice"
    assert model.spec.podSelector.matchLabels["app.kubernetes.io/component"] == "llminferenceservice-workload"
    assert model.spec.podSelector.matchLabels["kserve.io/component"] == "workload"
    assert picker.spec.podSelector.matchLabels["app.kubernetes.io/name"] == "stable"
    assert picker.spec.podSelector.matchLabels["app.kubernetes.io/part-of"] == "llminferenceservice"
    assert picker.spec.podSelector.matchLabels["app.kubernetes.io/component"] == "llminferenceservice-router-scheduler"
    # A configured storage allowlist never grants egress to a packaged Pod.
    assert len(model.spec.egress) == 0
}

test_network_kserve_candidate_has_distinct_policy_selectors = lambda {
    candidate = Deployment {**_network_test_stable, name = "candidate", role = "candidate"}
    service = Service {**_network_test_base, deployments: [_network_test_stable, candidate]}
    items = network_resources(service, _network_test_artifacts, Undefined)
    stable = _network_test_find(items, "serving-stable-model")
    candidateModel = _network_test_find(items, "serving-candidate-model")
    candidatePicker = _network_test_find(items, "serving-candidate-picker")
    assert stable.spec.podSelector.matchLabels["app.kubernetes.io/name"] == "stable"
    assert candidateModel.spec.podSelector.matchLabels["app.kubernetes.io/name"] == "candidate"
    assert candidatePicker.spec.podSelector.matchLabels["app.kubernetes.io/name"] == "candidate"
    assert candidateModel.spec.podSelector.matchLabels["kserve.io/component"] == "workload"
    assert candidatePicker.spec.podSelector.matchLabels["app.kubernetes.io/component"] == "llminferenceservice-router-scheduler"
}

# Gateway and EPP reach model port 8000; the Gateway reaches EPP gRPC 9002;
# separately selected Prometheus Pods can scrape model 8000 and EPP 9090.
# Cross-namespace peers constrain namespace and Pod in the same peer object.
test_network_gateway_prometheus_and_picker_paths = lambda {
    items = network_resources(_network_test_service, _network_test_artifacts, Undefined)
    model = _network_test_find(items, "serving-stable-model")
    picker = _network_test_find(items, "serving-stable-picker")
    gateway = _network_test_find(items, "serving-stable-gateway")
    assert model.spec.ingress[0].ports[0].port == 8000
    assert model.spec.ingress[0]["from"][0].namespaceSelector.matchLabels["kubernetes.io/metadata.name"] == "models"
    assert model.spec.ingress[0]["from"][0].podSelector.matchLabels["gateway.networking.k8s.io/gateway-name"] == "gateway"
    assert model.spec.ingress[0]["from"][1].podSelector.matchLabels == picker.spec.podSelector.matchLabels
    assert model.spec.ingress[0]["from"][2].namespaceSelector.matchLabels["kubernetes.io/metadata.name"] == "monitoring"
    assert model.spec.ingress[0]["from"][2].podSelector.matchLabels["app.kubernetes.io/name"] == "prometheus"
    assert picker.spec.ingress[0].ports[0].port == 9002
    assert picker.spec.ingress[0]["from"][0].podSelector.matchLabels["gateway.networking.k8s.io/gateway-name"] == "gateway"
    assert picker.spec.ingress[1].ports[0].port == 9090
    assert picker.spec.ingress[1]["from"][0].namespaceSelector.matchLabels["kubernetes.io/metadata.name"] == "monitoring"
    assert picker.spec.ingress[1]["from"][0].podSelector.matchLabels["app.kubernetes.io/name"] == "prometheus"
    assert gateway.spec.podSelector.matchLabels["gateway.networking.k8s.io/gateway-name"] == "gateway"
    assert gateway.spec.egress[0].to[0].podSelector.matchLabels == model.spec.podSelector.matchLabels
    assert gateway.spec.egress[0].ports[0].port == 8000
    assert gateway.spec.egress[1].to[0].podSelector.matchLabels == picker.spec.podSelector.matchLabels
    assert gateway.spec.egress[1].ports[0].port == 9002
}

test_network_picker_dns_api_and_model_egress = lambda {
    items = network_resources(_network_test_service, _network_test_artifacts, Undefined)
    model = _network_test_find(items, "serving-stable-model")
    picker = _network_test_find(items, "serving-stable-picker")
    assert picker.spec.egress[0].to[0].podSelector.matchLabels == model.spec.podSelector.matchLabels
    assert picker.spec.egress[0].ports[0].port == 8000
    assert picker.spec.egress[1].to[0].namespaceSelector.matchLabels["kubernetes.io/metadata.name"] == "kube-system"
    assert picker.spec.egress[1].to[0].podSelector.matchLabels["k8s-app"] == "kube-dns"
    assert {p.protocol: p.port for p in picker.spec.egress[1].ports} == {"UDP": 53, "TCP": 53}
    assert picker.spec.egress[2].to[0].ipBlock.cidr == "192.0.2.0/24"
    assert [p.port for p in picker.spec.egress[2].ports] == [443, 6443]
    assert all p in picker.spec.egress[2].ports {p.protocol == "TCP"}
}

# Storage CIDRs and DNS are applied only to nonpackaged remote downloads; the
# operator includes any required redirect/CDN and credential exchange targets.
test_network_remote_storage_egress = lambda {
    remote = Deployment {**_network_test_stable, packagedModel = False}
    service = Service {**_network_test_base, deployments: [remote]}
    uris = ["hf://owner/model", "s3://bucket/model", "https://example.invalid/model", "gs://bucket/model"]
    outputs = [network_resources(service, {model: Artifact {**_network_test_artifacts["model"], uri = modelUri}}, Undefined) for modelUri in uris]
    models = [_network_test_find(items, "serving-stable-model") for items in outputs]
    assert all model in models {model.spec.egress[0].to[0].namespaceSelector.matchLabels["kubernetes.io/metadata.name"] == "kube-system"}
    assert all model in models {model.spec.egress[0].to[0].podSelector.matchLabels["k8s-app"] == "kube-dns"}
    assert all model in models {model.spec.egress[1].to[0].ipBlock.cidr == "198.51.100.0/24" and model.spec.egress[1].ports[0].port == 443}
}

test_network_local_model_volumes_need_no_external_egress = lambda {
    local = Deployment {**_network_test_stable, packagedModel = False}
    noStorage = ServingNetwork {**_network_test_config, storageEgress = []}
    service = Service {**_network_test_base, network = noStorage, deployments: [local]}
    uris = ["pvc://models/path", "oci://registry.example/model:tag", "oci+native://registry.example/model:tag"]
    outputs = [network_resources(service, {model: Artifact {**_network_test_artifacts["model"], uri = modelUri}}, Undefined) for modelUri in uris]
    assert all items in outputs {len(_network_test_find(items, "serving-stable-model").spec.egress) == 0}
}

test_network_rejects_unapproved_remote_storage_and_unknown_artifact = lambda {
    remote = Deployment {**_network_test_stable, packagedModel = False}
    service = Service {**_network_test_base, deployments: [remote]}
    artifact = Artifact {**_network_test_artifacts["model"], uri = "hf://owner/model"}
    valid = runtime.catch(lambda { items = network_resources(service, {model: artifact}, Undefined) })
    noStorage = ServingNetwork {**_network_test_config, storageEgress = []}
    unapprovedService = Service {**_network_test_base, network = noStorage, deployments: [remote]}
    unapproved = runtime.catch(lambda { items = network_resources(unapprovedService, {model: artifact}, Undefined) })
    missingArtifact = runtime.catch(lambda { items = network_resources(service, {}, Undefined) })
    assert valid == Undefined and unapproved != Undefined and missingArtifact != Undefined
}

# Agentic requests enter through a private Gateway. Both endpoint ingress and
# Gateway egress are needed when the workload namespace is default-deny.
test_network_private_gateway_to_kserve_model_and_picker = lambda {
    service = Service {**_network_test_base, agentic: _network_test_agentic, deployments: [_network_test_stable]}
    items = network_resources(service, _network_test_artifacts, Undefined)
    model = _network_test_find(items, "serving-stable-model")
    picker = _network_test_find(items, "serving-stable-picker")
    publicGateway = _network_test_find(items, "serving-stable-gateway")
    privateGateway = _network_test_find(items, "serving-stable-private")
    modelPeer = model.spec.ingress[0]["from"][3]
    pickerPeer = picker.spec.ingress[0]["from"][1]
    assert model.spec.ingress[0].ports[0].port == 8000
    assert modelPeer.namespaceSelector.matchLabels["kubernetes.io/metadata.name"] == "models"
    assert modelPeer.podSelector.matchLabels == _network_test_private_gateway
    assert picker.spec.ingress[0].ports[0].port == 9002
    assert pickerPeer.namespaceSelector.matchLabels["kubernetes.io/metadata.name"] == "models"
    assert pickerPeer.podSelector.matchLabels == _network_test_private_gateway
    assert privateGateway.metadata.namespace == "models"
    assert privateGateway.spec.podSelector.matchLabels == _network_test_private_gateway
    assert publicGateway.spec.podSelector.matchLabels["gateway.networking.k8s.io/gateway-name"] == "gateway"
    assert len(privateGateway.spec.egress) == 2
    assert privateGateway.spec.egress[0].to[0].podSelector.matchLabels == model.spec.podSelector.matchLabels
    assert privateGateway.spec.egress[0].ports[0].port == 8000
    assert privateGateway.spec.egress[1].to[0].podSelector.matchLabels == picker.spec.podSelector.matchLabels
    assert privateGateway.spec.egress[1].ports[0].port == 9002
}

test_network_private_gateway_targets_each_raw_pool_deployment = lambda {
    service = Service {**_network_test_base, agentic: _network_test_agentic, pool: "shared", deployments: [_network_test_stable, _network_test_candidate]}
    items = network_resources(service, _network_test_artifacts, _network_test_pool)
    assert all deploymentName in ["stable", "candidate"] {_network_test_find(items, "serving-${deploymentName}-model").spec.podSelector.matchLabels["thalamus.ai/deployment"] == deploymentName}
    assert all deploymentName in ["stable", "candidate"] {_network_test_find(items, "serving-${deploymentName}-private").spec.egress[0].to[0].podSelector.matchLabels == _network_test_find(items, "serving-${deploymentName}-model").spec.podSelector.matchLabels}
    assert all deploymentName in ["stable", "candidate"] {_network_test_find(items, "serving-${deploymentName}-private").spec.egress[1].to[0].podSelector.matchLabels == _network_test_find(items, "serving-${deploymentName}-picker").spec.podSelector.matchLabels}
}

test_network_without_agentic_does_not_allow_private_gateway = lambda {
    items = network_resources(_network_test_service, _network_test_artifacts, Undefined)
    model = _network_test_find(items, "serving-stable-model")
    picker = _network_test_find(items, "serving-stable-picker")
    assert len([item for item in items if item.metadata.name == "serving-stable-private"]) == 0
    assert len(model.spec.ingress[0]["from"]) == 3
    assert len(picker.spec.ingress[0]["from"]) == 1
}
