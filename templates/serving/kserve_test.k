# Run from the repository root: kcl test ./templates/serving
import runtime

_revision = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
_other_revision = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
_image = "example@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
_digest = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
_gateway = {
    name: "gw"
    namespace: "ns"
    hostname: "example.invalid"
    keySelector: {"thalamus.dev/gateway-keys": "true"}
    rateLimit: "shared"
}
_versions = {kserve: "v0.21.0", agentgateway: "v1.7.0", inferenceExtension: "v1.6.2", vllm: "v0.23.0"}
_schema_test_access = {teams: ["00000000-0000-4000-8000-000000000001"]}
_schema_test_deployment = {name: "d", artifact: "a", hardware: "h", image: "image", revision: _revision}
_schema_test_packaged = {**_schema_test_deployment, image = _image, packagedModel: True}
_schema_test_candidate = {**_schema_test_packaged, name = "preview", revision = _other_revision, role: "candidate"}
_schema_test_artifact = {uri: "hf://model", modelDigest: _digest, tokenizerDigest: _digest}
_schema_test_hardware = {nodeSelector: {accelerator: "gpu"}, gpuCount: 1, cpu: "4", memoryGi: 32, hostMemoryGi: 64}
_schema_test_classes = {
    ttl: "30s"
    classes: [
        {name: "standard", priority: 0, enginePriority: 0, maxRequests: 32, maxBytes: "10Mi"},
        {name: "batch", priority: -1, enginePriority: 1, maxRequests: 16, maxBytes: "5Mi"},
    ]
}
_schema_test_service = {
    name: "svc"
    namespace: "ns"
    access: _schema_test_access
    network: {
        apiServer: [{cidr: "192.0.2.0/24", ports: [6443]}]
        gatewayPodSelector: {"gateway.networking.k8s.io/gateway-name": "gw"}
        dns: {namespaceSelector: {"kubernetes.io/metadata.name": "kube-system"}, podSelector: {"k8s-app": "kube-dns"}}
        prometheus: {namespaceSelector: {"kubernetes.io/metadata.name": "observability"}, podSelector: {"app.kubernetes.io/name": "prometheus"}}
    }
    gateway: _gateway
    versions: _versions
    deployments: [_schema_test_deployment]
}
_schema_test_agentic = {
    name: "svc"
    namespace: "ns"
    mode: "shared-stateless"
    image: _image
    upstreamCommit: "27e60cf006cb771cf1961ea9909e64d812f5f3c8"
    gatewayVersion: "v1.7.0"
    publicGateway: "gw"
    publicGatewayNamespace: "ns"
    internalGateway: "internal"
    internalGatewayNamespace: "ns"
    internalGatewayService: "internal"
    internalGatewayTargetPort: 443
    internalHostname: "internal.ns.svc.cluster.local"
    internalCAConfigMap: "thalamus-internal-ca"
    gatewayNamespaceLabels: {"kubernetes.io/metadata.name": "ns"}
    gatewayPodLabels: _schema_test_service.network.gatewayPodSelector
    dnsNamespaceLabels: {app: "dns"}
    dnsPodLabels: {app: "dns"}
    internalGatewayPodLabels: {"gateway.networking.k8s.io/gateway-name": "internal"}
    trafficClasses: ["standard"]
    instances: [{
        name: "candidate"
        hostname: "agent.example.invalid"
        databaseSecret: "agent-db"
        databaseCidrs: ["192.0.2.0/24"]
        keySelector: _gateway.keySelector
    }]
}
_schema_test_agentic_service = {
    **_schema_test_service
    trafficClasses: {ttl: "30s", classes: [_schema_test_classes.classes[0]]}
    agentic: _schema_test_agentic
}

test_packaged_model_rejects_mutable_tag = lambda {
    valid = runtime.catch(lambda { d = Deployment {**_schema_test_packaged} })
    invalid = runtime.catch(lambda { d = Deployment {**_schema_test_packaged, image = "example:latest"} })
    assert valid == Undefined and invalid != Undefined
}

test_recreate_rejects_nonpackaged_model = lambda {
    base = {**_schema_test_packaged, rolloutStrategy: "Recreate"}
    valid = runtime.catch(lambda { d = Deployment {**base} })
    invalid = runtime.catch(lambda { d = Deployment {**base, packagedModel = False} })
    assert valid == Undefined and invalid != Undefined
}

test_candidate_rejects_nonpackaged_model = lambda {
    valid = runtime.catch(lambda { d = Deployment {**_schema_test_candidate} })
    invalid = runtime.catch(lambda { d = Deployment {**_schema_test_candidate, packagedModel = False} })
    assert valid == Undefined and invalid != Undefined
}

test_invalid_deployment_identity_and_strategy = lambda {
    valid = runtime.catch(lambda { d = Deployment {**_schema_test_deployment} })
    role = runtime.catch(lambda { d = Deployment {**_schema_test_deployment, role = "preview"} })
    strategy = runtime.catch(lambda { d = Deployment {**_schema_test_deployment, rolloutStrategy = "Canary"} })
    revision = runtime.catch(lambda { d = Deployment {**_schema_test_deployment, revision = "mutable-tag"} })
    name = runtime.catch(lambda { d = Deployment {**_schema_test_deployment, name = "Invalid_Name"} })
    member = runtime.catch(lambda { d = Deployment {**_schema_test_deployment, poolMember = "../invalid"} })
    weight = runtime.catch(lambda { d = Deployment {**_schema_test_deployment, weight = -1} })
    assert valid == Undefined
    assert role != Undefined and strategy != Undefined and revision != Undefined
    assert name != Undefined and member != Undefined and weight != Undefined
}

test_artifact_rejects_mutable_content_identities = lambda {
    valid = runtime.catch(lambda { a = Artifact {**_schema_test_artifact} })
    model = runtime.catch(lambda { a = Artifact {**_schema_test_artifact, modelDigest = "latest"} })
    tokenizer = runtime.catch(lambda { a = Artifact {**_schema_test_artifact, tokenizerDigest = "sha256:ABC"} })
    assert valid == Undefined and model != Undefined and tokenizer != Undefined
}

test_offload_requires_prefix_cache = lambda {
    base = {kvOffloadGi: 4, prefixCaching: True}
    valid = runtime.catch(lambda { f = Features {**base} })
    invalid = runtime.catch(lambda { f = Features {**base, prefixCaching = False} })
    assert valid == Undefined and invalid != Undefined
}

test_hardware_memory_bound = lambda {
    valid = runtime.catch(lambda { h = Hardware {**_schema_test_hardware} })
    invalid = runtime.catch(lambda { h = Hardware {**_schema_test_hardware, hostMemoryGi = 16} })
    assert valid == Undefined and invalid != Undefined
}

test_gateway_requires_projected_keys_and_valid_limit_profile = lambda {
    valid = runtime.catch(lambda { g = Gateway {**_gateway} })
    selector = runtime.catch(lambda { g = Gateway {**_gateway, keySelector = {unrelated: "true"}} })
    profile = runtime.catch(lambda { g = Gateway {**_gateway, rateLimit = "../invalid"} })
    assert valid == Undefined and selector != Undefined and profile != Undefined
}

test_candidate_and_retained_stable_are_excluded_from_public_contract = lambda {
    retained = {**_schema_test_deployment, name = "retained", contextLength: 16384, weight: 0, features: {toolParser: "old-parser"}}
    candidate = {**_schema_test_candidate, name = "candidate", contextLength: 8192, weight: -1, features: {toolParser: "candidate-parser", reasoningParser: "candidate-reasoning"}}
    stable = {**_schema_test_deployment, name = "stable", contextLength: 4096}
    valid = runtime.catch(lambda { s = Service {**_schema_test_service, deployments = [candidate, retained, stable]} })
    assert valid == Undefined
}

test_public_stable_contract_must_match = lambda {
    first = {**_schema_test_deployment, name = "d1", contextLength: 4096}
    second = {**first, name = "d2", revision = _other_revision}
    valid = runtime.catch(lambda { s = Service {**_schema_test_service, deployments = [first, second]} })
    invalid = runtime.catch(lambda { s = Service {**_schema_test_service, deployments = [first, {**second, contextLength = 8192}]} })
    assert valid == Undefined and invalid != Undefined
}

test_stable_route_requires_positive_weight = lambda {
    base = {**_schema_test_service, deployments = [_schema_test_deployment, _schema_test_candidate]}
    valid = runtime.catch(lambda { s = Service {**base} })
    retained = {**_schema_test_deployment, weight: 0}
    invalid = runtime.catch(lambda { s = Service {**base, deployments = [retained, _schema_test_candidate]} })
    assert valid == Undefined and invalid != Undefined
}

test_duplicate_deployment_names_rejected = lambda {
    valid = runtime.catch(lambda { s = Service {**_schema_test_service, deployments = [_schema_test_deployment, _schema_test_candidate]} })
    duplicate = {**_schema_test_candidate, name = "d"}
    invalid = runtime.catch(lambda { s = Service {**_schema_test_service, deployments = [_schema_test_deployment, duplicate]} })
    assert valid == Undefined and invalid != Undefined
}

test_traffic_classes_require_unique_priorities_and_nonempty_classes = lambda {
    valid = runtime.catch(lambda { c = TrafficClasses {**_schema_test_classes} })
    duplicate = {**_schema_test_classes.classes[1], priority = 0, enginePriority = 0}
    priority = runtime.catch(lambda { c = TrafficClasses {**_schema_test_classes, classes = [_schema_test_classes.classes[0], duplicate]} })
    empty = runtime.catch(lambda { c = TrafficClasses {**_schema_test_classes, classes = []} })
    assert valid == Undefined and priority != Undefined and empty != Undefined
}

test_traffic_class_ttl_syntax_and_bounds = lambda {
    base = {**_schema_test_classes, ttl = "01h2m500ms"}
    valid = runtime.catch(lambda { c = TrafficClasses {**base} })
    zero = runtime.catch(lambda { c = TrafficClasses {**base, ttl = "0h0m0s"} })
    four_segments = runtime.catch(lambda { c = TrafficClasses {**base, ttl = "1h1m1s1ms"} })
    long_segment = runtime.catch(lambda { c = TrafficClasses {**base, ttl = "100000s"} })
    unsupported = runtime.catch(lambda { c = TrafficClasses {**base, ttl = "30us"} })
    assert valid == Undefined
    assert zero != Undefined and four_segments != Undefined and long_segment != Undefined and unsupported != Undefined
}

test_invalid_pool_reference_rejected = lambda {
    base = {**_schema_test_service, pool: "pool", trafficClasses: _schema_test_classes}
    valid = runtime.catch(lambda { s = Service {**base} })
    invalid = runtime.catch(lambda { s = Service {**base, pool = "../invalid"} })
    assert valid == Undefined and invalid != Undefined
}

test_service_rejects_agentic_class_divergence = lambda {
    valid = runtime.catch(lambda { s = Service {**_schema_test_agentic_service} })
    invalid = runtime.catch(lambda { s = Service {**_schema_test_agentic_service, trafficClasses = Undefined} })
    assert valid == Undefined and invalid != Undefined
}

test_agentic_rejects_foreign_namespace = lambda {
    assert runtime.catch(lambda { s = Service {**_schema_test_agentic_service} }) == Undefined
    assert runtime.catch(lambda {
        foreign = {**_schema_test_agentic, namespace = "other", internalGatewayNamespace = "other", internalHostname = "internal.other.svc.cluster.local"}
        s = Service {**_schema_test_agentic_service, agentic = foreign}
    }) != Undefined
}

test_agentic_rejects_mismatched_key_projection = lambda {
    assert runtime.catch(lambda { s = Service {**_schema_test_agentic_service} }) == Undefined
    assert runtime.catch(lambda {
        other_selector = {**_gateway, keySelector = {"thalamus.dev/gateway-keys": "true", other: "true"}}
        s = Service {**_schema_test_agentic_service, gateway = other_selector}
    }) != Undefined
}

test_candidate_route_cannot_overwrite_public_route = lambda {
    candidate = {**_schema_test_candidate, name = "d1"}
    base = {**_schema_test_service, deployments = [_schema_test_deployment, candidate]}
    valid = runtime.catch(lambda { s = Service {**base} })
    invalid = runtime.catch(lambda { s = Service {**base, name = "d1-candidate"} })
    assert valid == Undefined and invalid != Undefined
}

test_agentic_route_on_same_gateway_requires_distinct_hostname = lambda {
    valid = runtime.catch(lambda { s = Service {**_schema_test_agentic_service} })
    same_host = {**_gateway, hostname = "agent.example.invalid"}
    invalid = runtime.catch(lambda { s = Service {**_schema_test_agentic_service, gateway = same_host} })
    assert valid == Undefined and invalid != Undefined
}

test_agentic_route_cannot_overwrite_candidate_route = lambda {
    stable = {**_schema_test_deployment, name = "stable"}
    candidate = {**_schema_test_candidate, name = "svc"}
    safe_instance = {**_schema_test_agentic.instances[0], name = "safe"}
    safe_recipe = {**_schema_test_agentic, instances = [safe_instance]}
    base = {**_schema_test_agentic_service, deployments = [stable, candidate], agentic = safe_recipe}
    valid = runtime.catch(lambda { s = Service {**base} })
    invalid = runtime.catch(lambda { s = Service {**base, agentic = _schema_test_agentic} })
    assert valid == Undefined and invalid != Undefined
}

test_agentic_public_route_can_share_name_with_serving_backend = lambda {
    stable = {**_schema_test_deployment, name = "svc"}
    instance = {**_schema_test_agentic.instances[0], name = "backend"}
    recipe = {**_schema_test_agentic, instances = [instance]}
    valid = runtime.catch(lambda { s = Service {**_schema_test_agentic_service, deployments = [stable], agentic = recipe} })
    assert valid == Undefined
}

test_access_rejects_nil_invalid_and_duplicate_teams = lambda {
    valid_admin = runtime.catch(lambda { access = Access {teams: []} })
    valid_team = runtime.catch(lambda { access = Access {**_schema_test_access} })
    nil_team = runtime.catch(lambda { access = Access {**_schema_test_access, teams = ["00000000-0000-0000-0000-000000000000"]} })
    invalid_team = runtime.catch(lambda { access = Access {**_schema_test_access, teams = ["not-a-uuid"]} })
    duplicate = runtime.catch(lambda { access = Access {**_schema_test_access, teams = _schema_test_access.teams + _schema_test_access.teams} })
    assert valid_admin == Undefined and valid_team == Undefined
    assert nil_team != Undefined and invalid_team != Undefined and duplicate != Undefined
}

test_candidate_and_promoted_stable_can_retain_pool_member = lambda {
    promoted = {**_schema_test_packaged, name = "promoted", poolMember: "original"}
    preview = {**_schema_test_candidate, poolMember: "original"}
    valid = runtime.catch(lambda { s = Service {**_schema_test_service, pool: "pool", deployments = [promoted, preview]} })
    assert valid == Undefined
}

test_pool_member_requires_pool_reference = lambda {
    member = {**_schema_test_deployment, poolMember: "member"}
    base = {**_schema_test_service, pool: "pool", deployments = [member]}
    valid = runtime.catch(lambda { s = Service {**base} })
    invalid = runtime.catch(lambda { s = Service {**base, pool = Undefined} })
    assert valid == Undefined and invalid != Undefined
}

# KCL 0.13.2 can lose captured bindings after a nested schema failure.
# Each rejection gets a fresh test invocation and proves its valid baseline first.
test_service_network_requires_gateway_selector = lambda {
    assert runtime.catch(lambda { s = Service {**_schema_test_service} }) == Undefined
    assert runtime.catch(lambda {
        no_gateway = {**_schema_test_service.network, gatewayPodSelector = {}}
        s = Service {**_schema_test_service, network = no_gateway}
    }) != Undefined
}

test_service_network_requires_dns_namespace_selector = lambda {
    assert runtime.catch(lambda { s = Service {**_schema_test_service} }) == Undefined
    assert runtime.catch(lambda {
        no_dns_namespace = {**_schema_test_service.network.dns, namespaceSelector = {}}
        no_dns = {**_schema_test_service.network, dns = no_dns_namespace}
        s = Service {**_schema_test_service, network = no_dns}
    }) != Undefined
}

test_service_network_requires_prometheus_pod_selector = lambda {
    assert runtime.catch(lambda { s = Service {**_schema_test_service} }) == Undefined
    assert runtime.catch(lambda {
        no_prometheus_pods = {**_schema_test_service.network.prometheus, podSelector = {}}
        no_prometheus = {**_schema_test_service.network, prometheus = no_prometheus_pods}
        s = Service {**_schema_test_service, network = no_prometheus}
    }) != Undefined
}

test_service_network_requires_api_server_destination = lambda {
    assert runtime.catch(lambda { s = Service {**_schema_test_service} }) == Undefined
    assert runtime.catch(lambda {
        no_api = {**_schema_test_service.network, apiServer = []}
        s = Service {**_schema_test_service, network = no_api}
    }) != Undefined
}

test_service_network_rejects_unbounded_api_server_destination = lambda {
    assert runtime.catch(lambda { s = Service {**_schema_test_service} }) == Undefined
    assert runtime.catch(lambda {
        world_api = {**_schema_test_service.network, apiServer = [{cidr: "0.0.0.0/0", ports: [6443]}]}
        s = Service {**_schema_test_service, network = world_api}
    }) != Undefined
}

test_service_network_rejects_invalid_api_server_port = lambda {
    assert runtime.catch(lambda { s = Service {**_schema_test_service} }) == Undefined
    assert runtime.catch(lambda {
        bad_port = {**_schema_test_service.network, apiServer = [{cidr: "192.0.2.0/24", ports: [0]}]}
        s = Service {**_schema_test_service, network = bad_port}
    }) != Undefined
}

test_service_gateway_must_share_workload_namespace = lambda {
    valid = runtime.catch(lambda { s = Service {**_schema_test_service} })
    other_gateway = {**_gateway, namespace = "other"}
    invalid = runtime.catch(lambda { s = Service {**_schema_test_service, gateway = other_gateway} })
    assert valid == Undefined and invalid != Undefined
}

test_service_network_rejects_noncanonical_storage_cidr = lambda {
    allowed = {**_schema_test_service.network, storageEgress: [{cidr: "198.51.100.0/24", ports: [443]}]}
    assert runtime.catch(lambda { s = Service {**_schema_test_service, network = allowed} }) == Undefined
    assert runtime.catch(lambda {
        bad_storage = {**_schema_test_service.network, storageEgress: [{cidr: "198.51.100.1/24", ports: [443]}]}
        s = Service {**_schema_test_service, network = bad_storage}
    }) != Undefined
}

test_service_network_rejects_duplicate_api_server_ports = lambda {
    assert runtime.catch(lambda { s = Service {**_schema_test_service} }) == Undefined
    assert runtime.catch(lambda {
        duplicate_api_ports = {**_schema_test_service.network, apiServer = [{cidr: "192.0.2.0/24", ports: [6443, 6443]}]}
        s = Service {**_schema_test_service, network = duplicate_api_ports}
    }) != Undefined
}

test_service_gateway_selector_must_name_the_configured_gateway = lambda {
    valid = runtime.catch(lambda { s = Service {**_schema_test_service} })
    other = {**_schema_test_service.network, gatewayPodSelector = {"gateway.networking.k8s.io/gateway-name": "other"}}
    invalid = runtime.catch(lambda { s = Service {**_schema_test_service, network = other} })
    assert valid == Undefined and invalid != Undefined
}

test_agentic_shared_gateway_rejects_mismatched_pod_selector = lambda {
    assert runtime.catch(lambda { s = Service {**_schema_test_agentic_service} }) == Undefined
    assert runtime.catch(lambda {
        wrong_pod = {**_schema_test_agentic, gatewayPodLabels = {"gateway.networking.k8s.io/gateway-name": "other"}}
        s = Service {**_schema_test_agentic_service, agentic = wrong_pod}
    }) != Undefined
}

test_agentic_shared_gateway_rejects_mismatched_namespace_selector = lambda {
    assert runtime.catch(lambda { s = Service {**_schema_test_agentic_service} }) == Undefined
    assert runtime.catch(lambda {
        wrong_namespace = {**_schema_test_agentic, gatewayNamespaceLabels = {"kubernetes.io/metadata.name": "other"}}
        s = Service {**_schema_test_agentic_service, agentic = wrong_namespace}
    }) != Undefined
}
