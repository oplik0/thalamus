# Run from the repository root: kcl test ./templates/serving
# These fixtures inspect desired resources; controller admission and runtime
# behavior still require integration tests against the pinned upstreams.
import crypto
import json
import runtime
import templates.features.shared_pool as render_pool_fixture
import templates.gateway.policy as render_gateway_fixture

_render_rev_a = "1111111111111111111111111111111111111111111111111111111111111111"
_render_rev_b = "2222222222222222222222222222222222222222222222222222222222222222"
_render_image = "registry.example/engine@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
_render_model_digest = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
_render_tokenizer_digest = "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
_render_gateway = {
    name: "gateway"
    namespace: "models"
    hostname: "inference.example.test"
    keySelector: {"thalamus.dev/gateway-keys": "true"}
    rateLimit: "shared-ratelimit"
}
_render_limit = render_gateway_fixture.LimitProfile {
    name: "shared-ratelimit"
    service: "limiter"
    namespace: "models"
    port: 8081
    domain: "shared-ratelimit"
    deployment: {
        image: _render_image
        gatewayName: "gateway"
        gatewayNamespace: "models"
        gatewayPodSelector: {"gateway.networking.k8s.io/gateway-name": "gateway"}
        agenticGatewayPodSelector: {"gateway.networking.k8s.io/gateway-name": "internal"}
        redisSecret: "redis"
        redisPort: 6379
        redisCidrs: ["192.0.2.40/32"]
    }
}
_render_budgets = render_gateway_fixture.BudgetCaps {teams: [{team_id: "00000000-0000-4000-8000-000000000001", max_tokens: 10000, unit: "tokens", window: "day", allowed_classes: ["standard"]}]}
_render_versions = {kserve: "v0.21.0", agentgateway: "v1.7.0", inferenceExtension: "v1.6.2", vllm: "v0.23.0"}
_render_artifacts: {str:Artifact} = {
    model: {uri: "oci://${_render_image}", modelDigest: _render_model_digest, tokenizerDigest: _render_tokenizer_digest}
}
_render_hardware: {str:Hardware} = {
    gpu: {nodeSelector: {accelerator: "gpu"}, gpuCount: 1, cpu: "4", memoryGi: 32, hostMemoryGi: 64}
}
_render_network = {
    gatewayPodSelector: {"gateway.networking.k8s.io/gateway-name": "gateway"}
    apiServer: [{cidr: "192.0.2.0/24", ports: [6443]}]
    dns: {namespaceSelector: {"kubernetes.io/metadata.name": "kube-system"}, podSelector: {"k8s-app": "kube-dns"}}
    prometheus: {namespaceSelector: {"kubernetes.io/metadata.name": "monitoring"}, podSelector: {"app.kubernetes.io/name": "prometheus"}}
}
_render_stable = {name: "stable", artifact: "model", hardware: "gpu", image: _render_image, revision: _render_rev_a, packagedModel: True, role: "stable", weight: 1}
_render_candidate = {name: "new", artifact: "model", hardware: "gpu", image: _render_image, revision: _render_rev_b, packagedModel: True, role: "candidate", weight: 1}
_render_pooled_candidate = _render_candidate | {poolMember: "stable", replicas: 2}
_render_input = {name: "model", namespace: "models", access: {teams: ["00000000-0000-4000-8000-000000000001"]}, gateway: _render_gateway, network: _render_network, versions: _render_versions, deployments: [_render_stable]}
_render_agentic = {
    name: "model"
    namespace: "models"
    mode: "shared-stateless"
    image: _render_image
    upstreamCommit: "27e60cf006cb771cf1961ea9909e64d812f5f3c8"
    gatewayVersion: "v1.7.0"
    publicGateway: "gateway"
    publicGatewayNamespace: "models"
    internalGateway: "internal"
    internalGatewayNamespace: "models"
    internalGatewayService: "internal"
    internalHostname: "internal.models.svc.cluster.local"
    internalGatewayTargetPort: 443
    internalCAConfigMap: "thalamus-agentic-ca"
    gatewayNamespaceLabels: {"kubernetes.io/metadata.name": "models"}
    gatewayPodLabels: {"gateway.networking.k8s.io/gateway-name": "gateway"}
    dnsNamespaceLabels: {"kubernetes.io/metadata.name": "kube-system"}
    dnsPodLabels: {"k8s-app": "kube-dns"}
    internalGatewayPodLabels: {"gateway.networking.k8s.io/gateway-name": "internal"}
    instances: [{name: "agent", hostname: "agent.example.test", databaseSecret: "agent-db", databaseCidrs: ["192.0.2.10/32"], keySelector: _render_gateway.keySelector}]
}
_render_pool = render_pool_fixture.Pool {
    name: "shared"
    hardwareProfile: "gpu"
    nodeSelector: {accelerator: "gpu"}
    gpuPerNode: 1
    totalGpu: 2
    prometheusAddress: "http://prometheus:9090"
    versions: {kueue: "v0.18.0", keda: "2.20.2", "llm-d-router": "v1.0.0"}
    members: [
        {name: "stable", service: "model", namespace: "models", gpuPerReplica: 1, prometheusQuery: "stable_queue", threshold: "1"},
    ]
}

render_find = lambda items: [any], kind: str, name: str -> any {
    matches = [item for item in items if item.kind == kind and item.metadata.name == name]
    assert len(matches) == 1, "expected exactly one rendered resource"
    matches[0]
}

test_composition_baseline_and_immutable_manifest = lambda {
    service = Service {**_render_input}
    output = render_service(service, _render_artifacts, _render_hardware, Undefined, _render_limit, _render_budgets)
    assert output.apiVersion == "v1" and output.kind == "List"
    workload = render_find(output.items, "LLMInferenceService", "stable")
    assert workload.spec.replicas == 1 and workload.spec.rolloutStrategy.maxSurge == 1
    assert workload.metadata.annotations["thalamus.ai/revision"] == _render_rev_a
    assert workload.metadata.annotations["thalamus.ai/serving-kind"] == "kserve"
    manifest = render_find(output.items, "ConfigMap", workload.metadata.annotations["thalamus.ai/model-manifest"])
    identity = json.decode(manifest.data["manifest.json"])
    assert manifest.immutable and identity.image == _render_image
    assert identity.modelPath == "/opt/thalamus/model"
    assert identity.modelDigest == _render_model_digest and identity.tokenizerDigest == _render_tokenizer_digest
    container = workload.spec.template.containers[0]
    assert container.command == ["vllm", "serve", "/opt/thalamus/model"]
    assert container.securityContext.readOnlyRootFilesystem
    assert "--tokenizer" in container.args
    assert all m in container.volumeMounts {m.mountPath != "/opt/thalamus/model"}
    assert render_find(output.items, "AgentgatewayPolicy", "model-auth").spec.traffic.apiKeyAuthentication.mode == "Strict"
    model_policy = render_find(output.items, "NetworkPolicy", "serving-stable-model")
    assert model_policy.spec.podSelector.matchLabels["app.kubernetes.io/name"] == "stable"
    assert model_policy.spec.podSelector.matchLabels["kserve.io/component"] == "workload"
    picker_policy = render_find(output.items, "NetworkPolicy", "serving-stable-picker")
    assert picker_policy.spec.podSelector.matchLabels["app.kubernetes.io/component"] == "llminferenceservice-router-scheduler"
    gateway_network_policy = render_find(output.items, "NetworkPolicy", "serving-stable-gateway")
    assert gateway_network_policy.spec.podSelector.matchLabels == _render_network.gatewayPodSelector
}

test_composition_candidate_is_private_and_run_bound = lambda {
    service = Service {**_render_input, deployments = [_render_stable, _render_candidate]}
    output = render_service(service, _render_artifacts, _render_hardware, Undefined, _render_limit, _render_budgets)
    public = render_find(output.items, "HTTPRoute", "model")
    assert [b.name for b in public.spec.rules[0].backendRefs] == ["stable-backend"]
    candidate = render_find(output.items, "HTTPRoute", "new-candidate")
    candidate_rule = candidate.spec.rules[0]
    assert candidate_rule.matches[0].path.value == "/candidates/new/v1/chat/completions"
    assert candidate_rule.filters[0].urlRewrite.path.replaceFullPath == "/v1/chat/completions"
    assert [b.name for b in candidate_rule.backendRefs] == ["new-backend"]
    policy = render_find(output.items, "AgentgatewayPolicy", "new-candidate-auth")
    expressions = policy.spec.traffic.authorization.policy.matchExpressions
    assert any e in expressions {'apiKey.kind == "job"' in e and 'apiKey.deployment == "new"' in e}
    assert any e in expressions {"apiKey.run_id" in e and "apiKey.expiry" in e}
    backend_resource = render_find(output.items, "AgentgatewayBackend", "new-backend")
    assert backend_resource.metadata.annotations["thalamus.ai/revision"] == _render_rev_b
    assert json.decode(backend_resource.spec.policies.transformation.response.set[0].value) == "model/new/${_render_rev_b}"
}

test_composition_legacy_benchmark_is_distinct_and_job_only = lambda {
    service = Service {**_render_input, deployments = [_render_stable, _render_candidate]}
    output = render_service(service, _render_artifacts, _render_hardware, Undefined, _render_limit, _render_budgets)
    route = render_find(output.items, "HTTPRoute", "new-benchmark")
    benchmark_rule = route.spec.rules[0]
    assert benchmark_rule.matches[0].path.value == "/candidates/new/v1/completions"
    assert benchmark_rule.filters[0].urlRewrite.path.replaceFullPath == "/v1/completions"
    assert benchmark_rule.filters[1].responseHeaderModifier.set[0].value == "model/new/${_render_rev_b}"
    assert benchmark_rule.backendRefs[0].kind == "InferencePool" and benchmark_rule.backendRefs[0].name == "new-inference-pool"
    policy = render_find(output.items, "AgentgatewayPolicy", "new-benchmark-auth")
    assert any e in policy.spec.traffic.authorization.policy.matchExpressions {'apiKey.run_kind == "benchmark"' in e and "apiKey.request_budget" in e}
    assert any e in policy.spec.traffic.authorization.policy.matchExpressions {'apiKey.kind == "job"' in e and 'apiKey.deployment == "new"' in e}
    assert policy.spec.traffic.rateLimit["global"].failureMode == "FailClosed"
    assert policy.spec.traffic.rateLimit["global"].descriptors[0].unit == "Requests"
    stable = render_find(output.items, "HTTPRoute", "stable-benchmark").spec.rules[0]
    assert stable.matches[0].path.value == "/candidates/stable/v1/completions"
    assert stable.filters[0].urlRewrite.path.replaceFullPath == "/v1/completions"
    assert stable.filters[1].responseHeaderModifier.set[0].value == "model/stable/${_render_rev_a}"
    assert stable.backendRefs[0].kind == "InferencePool" and stable.backendRefs[0].name == "stable-inference-pool"
    stable_policy = render_find(output.items, "AgentgatewayPolicy", "stable-benchmark-auth")
    assert any e in stable_policy.spec.traffic.authorization.policy.matchExpressions {'apiKey.kind == "job"' in e and 'apiKey.deployment == "stable"' in e}
    assert any e in stable_policy.spec.traffic.authorization.policy.matchExpressions {'apiKey.run_kind == "benchmark"' in e and "apiKey.request_budget" in e}
    assert stable_policy.spec.traffic.rateLimit["global"].failureMode == "FailClosed"
    assert stable_policy.spec.traffic.rateLimit["global"].descriptors[0].unit == "Requests"
}

test_composition_benchmark_with_service_named_like_deployment = lambda {
    service = Service {**_render_input, name = "stable"}
    output = render_service(service, _render_artifacts, _render_hardware, Undefined, _render_limit, _render_budgets)
    benchmark_rule = render_find(output.items, "HTTPRoute", "stable-benchmark").spec.rules[0]
    assert benchmark_rule.matches[0].path.value == "/candidates/stable/v1/completions"
    assert benchmark_rule.backendRefs[0].name == "stable-inference-pool"
    policy = render_find(output.items, "AgentgatewayPolicy", "stable-benchmark-auth")
    assert any e in policy.spec.traffic.authorization.policy.matchExpressions {'apiKey.deployment == "stable"' in e and 'apiKey.kind == "job"' in e}
}

test_composition_zero_weight_retained_without_public_route = lambda {
    retained = _render_stable | {weight = 0}
    promoted = _render_candidate | {role = "stable"}
    service = Service {**_render_input, deployments = [retained, promoted]}
    output = render_service(service, _render_artifacts, _render_hardware, Undefined, _render_limit, _render_budgets)
    assert render_find(output.items, "LLMInferenceService", "stable").metadata.annotations["thalamus.ai/revision"] == _render_rev_a
    assert [b.name for b in render_find(output.items, "HTTPRoute", "model").spec.rules[0].backendRefs] == ["new-backend"]
    assert not (any r in output.items {r.kind == "HTTPRoute" and r.metadata.name == "new-candidate"})
    assert render_find(output.items, "HTTPRoute", "stable-benchmark").spec.rules[0].backendRefs[0].name == "stable-inference-pool"
}

test_composition_recreate_and_kv_offload = lambda {
    deployment = _render_stable | {rolloutStrategy: "Recreate", features: {prefixCaching: True, kvOffloadGi: 4, shmGi: 1}}
    service = Service {**_render_input, deployments = [deployment]}
    output = render_service(service, _render_artifacts, _render_hardware, Undefined, _render_limit, _render_budgets)
    workload = render_find(output.items, "Deployment", "stable")
    assert workload.spec.strategy.type == "Recreate"
    assert workload.metadata.annotations["thalamus.ai/serving-kind"] == "raw"
    container = workload.spec.template.spec.containers[0]
    assert "--kv-offloading-size" in container.args and "native" in container.args
    assert container.resources.requests.memory == "40Gi" and container.resources.limits.memory == "40Gi"
    inference_pool = render_find(output.items, "InferencePool", "stable-inference-pool")
    assert inference_pool.spec.endpointPickerRef.failureMode == "FailClose"
    epp = render_find(output.items, "Deployment", "stable-epp")
    assert "--config-text" in epp.spec.template.spec.containers[0].args
    model_selector = render_find(output.items, "NetworkPolicy", "serving-stable-model").spec.podSelector.matchLabels
    picker_selector = render_find(output.items, "NetworkPolicy", "serving-stable-picker").spec.podSelector.matchLabels
    assert all key in model_selector {key in workload.spec.template.metadata.labels and workload.spec.template.metadata.labels[key] == model_selector[key]}
    assert all key in picker_selector {key in epp.spec.template.metadata.labels and epp.spec.template.metadata.labels[key] == picker_selector[key]}
}

test_composition_kserve_native_kv_and_picker_config = lambda {
    deployment = _render_stable | {features: {prefixCaching: True, kvOffloadGi: 4, shmGi: 1}}
    service = Service {**_render_input, deployments = [deployment]}
    output = render_service(service, _render_artifacts, _render_hardware, Undefined, _render_limit, _render_budgets)
    workload = render_find(output.items, "LLMInferenceService", "stable")
    assert workload.spec.router.scheduler.config.ref.name == "stable-epp-config"
    assert workload.spec.router.scheduler.config.ref.key == "endpoint-picker.yaml"
    config = json.decode(render_find(output.items, "ConfigMap", "stable-epp-config").data["endpoint-picker.yaml"])
    assert config.apiVersion == "llm-d.ai/v1" and config.kind == "EndpointPickerConfig"
    assert any plugin in config.plugins {plugin.type == "approx-prefix-cache-producer"}
    assert "--kv-offloading-backend" in workload.spec.template.containers[0].args
}

test_composition_shared_pool_and_traffic_classes = lambda {
    service = Service {**_render_input, pool = "shared", deployments = [_render_stable, _render_pooled_candidate], trafficClasses = {ttl: "30s", classes: [{name: "standard", priority: 0, enginePriority: 0, maxRequests: 32, maxBytes: "10Mi"}]}}
    output = render_service(service, _render_artifacts, _render_hardware, _render_pool, _render_limit, _render_budgets)
    assert render_find(output.items, "Deployment", "stable").spec.strategy.type == "RollingUpdate"
    assert render_find(output.items, "Deployment", "stable-filler").spec.replicas == 1
    filler = render_find(output.items, "Deployment", "stable-filler")
    model_selector = render_find(output.items, "NetworkPolicy", "serving-stable-model").spec.podSelector.matchLabels
    assert all key in model_selector {key in filler.spec.template.metadata.labels and filler.spec.template.metadata.labels[key] == model_selector[key]}
    assert render_find(output.items, "ScaledObject", "shared-stable").spec.scaleTargetRef.name == "stable"
    assert render_find(output.items, "LocalQueue", "shared-stable-filler").spec.clusterQueue == "shared-filler"
    pool_items = render_pool_fixture.render_pool(_render_pool)
    local_queue = render_find(output.items, "LocalQueue", "shared-stable")
    cluster_queue = render_find(pool_items, "ClusterQueue", local_queue.spec.clusterQueue)
    assert local_queue.metadata.namespace == service.namespace
    assert cluster_queue.spec.namespaceSelector.matchLabels["kubernetes.io/metadata.name"] == service.namespace
    assert render_find(pool_items, "ClusterQueue", "shared-filler").metadata.namespace == Undefined
    candidate = render_find(output.items, "Deployment", "new")
    assert candidate.spec.replicas == 1
    assert candidate.spec.template.metadata.labels["kueue.x-k8s.io/queue-name"] == "shared-stable"
    assert not (any r in output.items {r.kind == "ScaledObject" and r.spec.scaleTargetRef.name == "new"})
    assert not (any r in output.items {r.kind == "Deployment" and r.metadata.name == "new-filler"})
    assert not (any r in output.items {r.kind == "ClusterQueue" or r.kind == "ResourceFlavor"})
    objective = render_find(output.items, "InferenceObjective", "model-standard")
    assert objective.spec.poolRefs == [{name: "stable-inference-pool"}, {name: "new-inference-pool"}]
    args = render_find(output.items, "Deployment", "stable").spec.template.spec.containers[0].args
    assert "--scheduling-policy" in args and "priority" in args
    picker = render_find(output.items, "AgentgatewayPolicy", "stable-epp-timeout")
    assert picker.spec.targetRefs[0].name == "stable-epp"
    assert picker.spec.backend.http.requestTimeout == "30s5s"
}

test_composition_pool_promotion_retains_one_scaling_owner = lambda {
    retained = _render_stable | {weight = 0}
    promoted = _render_pooled_candidate | {role = "stable", weight = 1}
    service = Service {**_render_input, pool = "shared", deployments = [retained, promoted]}
    output = render_service(service, _render_artifacts, _render_hardware, _render_pool, _render_limit, _render_budgets)
    assert render_find(output.items, "Deployment", "stable").spec.replicas == 1
    assert render_find(output.items, "Deployment", "new-filler").spec.replicas == 1
    assert not (any r in output.items {r.kind == "Deployment" and r.metadata.name == "stable-filler"})
    assert render_find(output.items, "ScaledObject", "shared-stable").spec.scaleTargetRef.name == "new"
    assert [b.name for b in render_find(output.items, "HTTPRoute", "model").spec.rules[0].backendRefs] == ["new-backend"]
}

test_composition_rejects_unresolved_or_unsafe_pool = lambda {
    service = Service {**_render_input, pool = "shared", deployments = [_render_stable, _render_pooled_candidate]}
    valid = runtime.catch(lambda { result = render_service(service, _render_artifacts, _render_hardware, _render_pool, _render_limit, _render_budgets) })
    missing = runtime.catch(lambda { result = render_service(service, _render_artifacts, _render_hardware, Undefined, _render_limit, _render_budgets) })
    wrong_name = render_pool_fixture.Pool {**_render_pool, name = "different"}
    wrong = runtime.catch(lambda { result = render_service(service, _render_artifacts, _render_hardware, wrong_name, _render_limit, _render_budgets) })
    changed_hardware: {str:Hardware} = {gpu: {nodeSelector: {accelerator: "other"}, gpuCount: 1, cpu: "4", memoryGi: 32, hostMemoryGi: 64}}
    topology = runtime.catch(lambda { result = render_service(service, _render_artifacts, changed_hardware, _render_pool, _render_limit, _render_budgets) })
    recreate_service = Service {**_render_input, pool = "shared", deployments = [_render_stable | {rolloutStrategy: "Recreate"}, _render_pooled_candidate]}
    recreate = runtime.catch(lambda { result = render_service(recreate_service, _render_artifacts, _render_hardware, _render_pool, _render_limit, _render_budgets) })
    assert valid == Undefined and missing != Undefined and wrong != Undefined and topology != Undefined and recreate != Undefined
}

test_composition_recreate_pool_without_fillers = lambda {
    no_filler_pool = render_pool_fixture.Pool {**_render_pool, totalGpu = 1}
    stable_recreate = _render_stable | {rolloutStrategy: "Recreate"}
    service = Service {**_render_input, pool = "shared", deployments = [stable_recreate, _render_pooled_candidate]}
    output = render_service(service, _render_artifacts, _render_hardware, no_filler_pool, _render_limit, _render_budgets)
    assert render_find(output.items, "Deployment", "stable").spec.strategy.type == "Recreate"
    assert render_find(output.items, "Deployment", "stable-filler").spec.replicas == 0
}

test_composition_oci_binding_and_memory_rejection = lambda {
    service = Service {**_render_input}
    valid = runtime.catch(lambda { result = render_service(service, _render_artifacts, _render_hardware, Undefined, _render_limit, _render_budgets) })
    changed: {str:Artifact} = {model: {uri: "oci://other@sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff", modelDigest: _render_model_digest, tokenizerDigest: _render_tokenizer_digest}}
    uri_error = runtime.catch(lambda { result = render_service(service, changed, _render_hardware, Undefined, _render_limit, _render_budgets) })
    deployment = _render_stable | {features: {kvOffloadGi: 20, shmGi: 1}}
    large = Service {**_render_input, deployments = [deployment]}
    memory_error = runtime.catch(lambda { result = render_service(large, _render_artifacts, _render_hardware, Undefined, _render_limit, _render_budgets) })
    assert valid == Undefined and uri_error != Undefined and memory_error != Undefined
}

test_composition_rejects_wrong_limit_profile = lambda {
    service = Service {**_render_input}
    valid = runtime.catch(lambda { result = render_service(service, _render_artifacts, _render_hardware, Undefined, _render_limit, _render_budgets) })
    wrong_name = render_gateway_fixture.LimitProfile {**_render_limit, name = "other"}
    wrong_namespace = render_gateway_fixture.LimitProfile {**_render_limit, namespace = "other"}
    wrong_selector = render_gateway_fixture.LimitProfile {
        **_render_limit
        deployment = {**_render_limit.deployment, gatewayPodSelector = {"gateway.networking.k8s.io/gateway-name": "gateway", "app": "other"}}
    }
    missing_name = runtime.catch(lambda { result = render_service(service, _render_artifacts, _render_hardware, Undefined, wrong_name, _render_budgets) })
    wrong_ns = runtime.catch(lambda { result = render_service(service, _render_artifacts, _render_hardware, Undefined, wrong_namespace, _render_budgets) })
    selector_mismatch = runtime.catch(lambda { result = render_service(service, _render_artifacts, _render_hardware, Undefined, wrong_selector, _render_budgets) })
    assert valid == Undefined and missing_name != Undefined and wrong_ns != Undefined and selector_mismatch != Undefined
}

test_composition_agentic_modes_and_name_collision = lambda {
    service = Service {**_render_input, agentic = _render_agentic}
    output = render_service(service, _render_artifacts, _render_hardware, Undefined, _render_limit, _render_budgets)
    public = render_find(output.items, "HTTPRoute", "model-agent")
    assert any m in public.spec.rules[0].matches {m.method == "POST" and m.path.value == "/v1/responses"}
    policy = render_find(output.items, "AgentgatewayPolicy", "model-agent-auth")
    assert any e in policy.spec.traffic.authorization.policy.matchExpressions {'b.store == false' in e}
    pod_hash = crypto.sha256("model/agent/stable")[:32]
    assert render_find(output.items, "NetworkPolicy", "agentic-pod-${pod_hash}").spec.policyTypes == ["Ingress", "Egress"]
    internal = render_find(output.items, "AgentgatewayPolicy", "stable-agentic-internal-auth")
    assert internal.spec.traffic.rateLimit["global"].failureMode == "FailClosed"
    wrong_private_gateway = render_gateway_fixture.LimitProfile {
        **_render_limit
        deployment = {**_render_limit.deployment, agenticGatewayPodSelector = {"gateway.networking.k8s.io/gateway-name": "internal", "app": "wrong"}}
    }
    invalid_private_gateway = runtime.catch(lambda { result = render_service(service, _render_artifacts, _render_hardware, Undefined, wrong_private_gateway, _render_budgets) })
    assert invalid_private_gateway != Undefined
    same_name_deployment = _render_stable | {name = "model"}
    collision_base = Service {**_render_input, deployments = [same_name_deployment], agentic = _render_agentic}
    valid = runtime.catch(lambda { result = render_service(collision_base, _render_artifacts, _render_hardware, Undefined, _render_limit, _render_budgets) })
    collision_agentic = _render_agentic | {instances = [{name: "benchmark", hostname: "agent.example.test", databaseSecret: "agent-db", databaseCidrs: ["192.0.2.10/32"], keySelector: _render_gateway.keySelector}]}
    collision = runtime.catch(lambda {
        conflicting = Service {**_render_input, deployments = [same_name_deployment], agentic = collision_agentic}
        result = render_service(conflicting, _render_artifacts, _render_hardware, Undefined, _render_limit, _render_budgets)
    })
    assert valid == Undefined and collision != Undefined
}

test_composition_agentic_candidate_binds_every_hop = lambda {
    service = Service {**_render_input, agentic = _render_agentic, deployments = [_render_stable, _render_candidate]}
    output = render_service(service, _render_artifacts, _render_hardware, Undefined, _render_limit, _render_budgets)
    pod_hash = crypto.sha256("model/agent/new")[:32]
    outer = render_find(output.items, "HTTPRoute", "agentic-cand-${pod_hash}")
    assert [r.backendRefs[0].name for r in outer.spec.rules] == ["agentic-backend-${pod_hash}", "agentic-backend-${pod_hash}", "agentic-backend-${pod_hash}"]
    assert [r.matches[0].path.value for r in outer.spec.rules] == ["/candidates/new/v1/chat/completions", "/candidates/new/v1/responses", "/candidates/new/v1/messages"]
    outer_policy = render_find(output.items, "AgentgatewayPolicy", "agentic-cand-${pod_hash}-auth")
    assert any e in outer_policy.spec.traffic.authorization.policy.matchExpressions {'apiKey.kind == "job" && apiKey.deployment == "new"' in e}
    assert any e in outer_policy.spec.traffic.authorization.policy.matchExpressions {'/candidates/new/v1/responses' in e and 'b.store == false' in e}
    agentic_pod = render_find(output.items, "Deployment", "agentic-pod-${pod_hash}")
    envs = {e.name: e for e in agentic_pod.spec.template.spec.containers[0].env}
    assert envs["LLM_API_BASE"].value == "https://internal.models.svc.cluster.local/agentic/model/new"
    assert envs["SSL_CERT_FILE"].value == "/var/run/agentic-ca/ca.crt"
    inner = render_find(output.items, "HTTPRoute", "new-agentic-internal")
    assert all r in inner.spec.rules {len(r.backendRefs) == 1 and r.backendRefs[0].name == "new-backend"}
    inner_policy = render_find(output.items, "AgentgatewayPolicy", "new-agentic-internal-auth")
    assert any e in inner_policy.spec.traffic.authorization.policy.matchExpressions {'apiKey.kind == "job" && apiKey.deployment == "new"' in e}
    assert inner_policy.spec.traffic.rateLimit["global"].descriptors[0].unit == "Tokens"
    model_backend = render_find(output.items, "AgentgatewayBackend", "new-backend")
    assert [f.type for f in model_backend.spec.ai.provider.custom.formats] == ["Completions", "Responses", "Messages"]
    assert model_backend.spec.policies.ai.routes["/v1/responses"] == "Responses"
    assert model_backend.spec.policies.ai.routes["/v1/messages"] == "Messages"
}
