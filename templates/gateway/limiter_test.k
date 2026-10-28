# Rendered-object tests only; a live controller, Redis and an enforcing CNI are
# needed to verify actual request processing and network isolation.
import crypto
import runtime
import yaml

_rls_deployment_data = {
    image: "docker.io/envoyproxy/ratelimit@sha256:70453d5ca820dce0c2939d95a3eeb0f3e64ac083749bc021401c13e4233d1595"
    gatewayName: "serving-gateway"
    gatewayNamespace: "inference"
    gatewayPodSelector: {"gateway.networking.k8s.io/gateway-name": "serving-gateway", "example.net/revision": "stable"}
    redisSecret: "example-redis"
    redisPort: 6379
    redisCidrs: ["192.0.2.40/32", "2001:db8::/64"]
}
_rls_redis_selector = {"app.kubernetes.io/name": "example-redis", "app.kubernetes.io/component": "global-rate-limit-store"}
_rls_profile_data = {name: "shared", service: "example-ratelimit", namespace: "inference", port: 8081, domain: "example-shared", deployment: _rls_deployment_data}
_rls_profile = LimitProfile {**_rls_profile_data}
_rls_caps = BudgetCaps {teams: [{team_id: "11111111-1111-4111-8111-111111111111", max_tokens: 1000, unit: "tokens", window: "day"}]}
_rls_rate = from_catalog(_rls_profile, _rls_caps)
_rls_objects = limiter_resources(_rls_profile, _rls_rate)
_rls_labels = {"app.kubernetes.io/name": "example-ratelimit", "app.kubernetes.io/component": "global-rate-limiter"}

# Policy-only profiles do not have to deploy or own their RLS endpoint.
test_rls_external_profile_remains_valid = lambda {
    external = LimitProfile {name: "external", service: "other-rls", namespace: "inference", port: 9000, domain: "external"}
    assert from_catalog(external, _rls_caps).name == "other-rls"
    missing = runtime.catch(lambda {rendered = limiter_resources(external, from_catalog(external, _rls_caps))})
    assert missing != Undefined
}

test_rls_service_selects_exact_managed_pods = lambda {
    cm = _rls_objects[0]
    deployment = _rls_objects[1]
    service = _rls_objects[2]
    network = _rls_objects[3]
    assert [o.kind for o in _rls_objects] == ["ConfigMap", "Deployment", "Service", "NetworkPolicy"]
    assert cm.metadata.namespace == "inference"
    assert deployment.metadata.name == "example-ratelimit" and deployment.metadata.namespace == "inference"
    assert deployment.spec.replicas == 2
    assert deployment.spec.selector.matchLabels == _rls_labels
    assert deployment.spec.template.metadata.labels == _rls_labels
    assert service.spec.selector == _rls_labels
    assert network.spec.podSelector.matchLabels == _rls_labels
    assert service.spec.ports == [{name: "grpc", port: 8081, targetPort: 8081, protocol: "TCP", appProtocol: "kubernetes.io/h2c"}]
}

test_rls_config_mount_rollout_and_readiness = lambda {
    cm = _rls_objects[0]
    deployment = _rls_objects[1]
    pod = deployment.spec.template.spec
    container = pod.containers[0]
    env = {v.name: v for v in container.env}
    config = yaml.decode(cm.data["config.yaml"])
    assert config.domain == _rls_profile.domain
    assert [d.key for d in config.descriptors] == ["key_id", "project", "team", "benchmark_key"]
    assert container.command == ["/bin/ratelimit"]
    assert pod.volumes[0] == {name: "config", configMap: {name: cm.metadata.name}}
    assert container.volumeMounts[0] == {name: "config", mountPath: "/data/ratelimit/config", readOnly: True}
    assert deployment.spec.template.metadata.annotations["checksum/config"] == crypto.sha256(cm.data["config.yaml"])
    assert env["RUNTIME_ROOT"].value == "/data" and env["RUNTIME_SUBDIRECTORY"].value == "ratelimit"
    assert env["RUNTIME_IGNOREDOTFILES"].value == "true" and env["USE_STATSD"].value == "false"
    assert env["RUNTIME_WATCH_ROOT"].value == "false"
    assert env["GRPC_PORT"].value == "8081"
    assert env["HEALTHY_WITH_AT_LEAST_ONE_CONFIG_LOADED"].value == "true"
    assert env["REDIS_HEALTH_CHECK_ACTIVE_CONNECTION"].value == "true"
    assert container.readinessProbe.httpGet == {path: "/healthcheck", port: 8080}
    assert pod.automountServiceAccountToken == False
    assert pod.securityContext.runAsNonRoot == True and pod.securityContext.runAsUser == 10001
    assert pod.securityContext.seccompProfile.type == "RuntimeDefault"
    assert container.securityContext.readOnlyRootFilesystem == True
    assert container.securityContext.allowPrivilegeEscalation == False and container.securityContext.capabilities.drop == ["ALL"]
    changed_profile = LimitProfile {**_rls_profile_data, domain = "different-domain"}
    changed = limiter_resources(changed_profile, from_catalog(changed_profile, _rls_caps))
    assert changed[0].data["config.yaml"] != cm.data["config.yaml"]
    assert changed[1].spec.template.metadata.annotations["checksum/config"] != deployment.spec.template.metadata.annotations["checksum/config"]
}

test_rls_redis_secret_tls_and_optional_ca = lambda {
    pod = _rls_objects[1].spec.template.spec
    container = pod.containers[0]
    env = {v.name: v for v in container.env}
    assert env["REDIS_SOCKET_TYPE"].value == "tcp" and env["REDIS_TLS"].value == "true"
    assert env["REDIS_URL"].valueFrom.secretKeyRef == {name: "example-redis", key: "url"}
    assert env["REDIS_AUTH"].valueFrom.secretKeyRef == {name: "example-redis", key: "auth"}
    assert "REDIS_TLS_CACERT" not in env
    assert len(pod.volumes) == 1 and len(container.volumeMounts) == 1
    private_ca = LimitProfile {**_rls_profile_data, deployment = {**_rls_deployment_data, redisCASecret = "private-ca"}}
    ca_pod = limiter_resources(private_ca, from_catalog(private_ca, _rls_caps))[1].spec.template.spec
    ca_env = {v.name: v for v in ca_pod.containers[0].env}
    assert ca_env["REDIS_TLS_CACERT"].value == "/etc/redis-ca/ca.crt"
    assert ca_pod.volumes[1] == {name: "redis-ca", secret: {secretName: "private-ca", items: [{key: "ca.crt", path: "ca.crt"}]}}
    assert ca_pod.containers[0].volumeMounts[1] == {name: "redis-ca", mountPath: "/etc/redis-ca", readOnly: True}
}

test_rls_network_paths_are_scoped = lambda {
    network = _rls_objects[3].spec
    assert network.policyTypes == ["Ingress", "Egress"]
    assert len(network.ingress) == 1
    assert network.ingress[0]["from"] == [{namespaceSelector: {matchLabels: {"kubernetes.io/metadata.name": "inference"}}, podSelector: {matchLabels: _rls_deployment_data.gatewayPodSelector}}]
    assert network.ingress[0].ports == [{protocol: "TCP", port: 8081}]
    assert len(network.egress) == 2
    assert network.egress[0]["to"] == [{namespaceSelector: {matchLabels: {"kubernetes.io/metadata.name": "kube-system"}}, podSelector: {matchLabels: {"k8s-app": "kube-dns"}}}]
    assert network.egress[0].ports == [{protocol: "UDP", port: 53}, {protocol: "TCP", port: 53}]
    assert network.egress[1]["to"] == [{ipBlock: {cidr: "192.0.2.40/32"}}, {ipBlock: {cidr: "2001:db8::/64"}}]
    assert network.egress[1].ports == [{protocol: "TCP", port: 6379}]
    custom = LimitProfile {**_rls_profile_data, deployment = {**_rls_deployment_data, gatewayPodSelector = {"gateway.networking.k8s.io/gateway-name": "serving-gateway", "operator.example/revision": "stable"}}}
    custom_ingress = limiter_resources(custom, from_catalog(custom, _rls_caps))[3].spec.ingress[0]
    assert custom_ingress["from"][0].podSelector.matchLabels == {"gateway.networking.k8s.io/gateway-name": "serving-gateway", "operator.example/revision": "stable"}
}

test_rls_allows_optional_same_namespace_agentic_gateway = lambda {
    selector = {"gateway.networking.k8s.io/gateway-name": "agentic-internal", "operator.example/revision": "stable"}
    configured = LimitProfile {**_rls_profile_data, deployment = {**_rls_deployment_data, agenticGatewayPodSelector = selector}}
    ingress = limiter_resources(configured, from_catalog(configured, _rls_caps))[3].spec.ingress
    assert len(ingress) == 1 and ingress[0].ports == [{protocol: "TCP", port: 8081}]
    assert ingress[0]["from"] == [
        {namespaceSelector: {matchLabels: {"kubernetes.io/metadata.name": "inference"}}, podSelector: {matchLabels: _rls_deployment_data.gatewayPodSelector}},
        {podSelector: {matchLabels: selector}},
    ]
    # No namespaceSelector on the second peer means this namespace only.
    assert "namespaceSelector" not in ingress[0]["from"][1]
}

test_rls_rejects_empty_agentic_gateway_selector = lambda {
    assert runtime.catch(lambda {p = LimitProfile {**_rls_profile_data}}) == Undefined
    assert runtime.catch(lambda {p = LimitProfile {**_rls_profile_data, deployment = {**_rls_deployment_data, agenticGatewayPodSelector = {}}}}) != Undefined
}

test_rls_rejects_agentic_gateway_selector_without_gateway_name = lambda {
    assert runtime.catch(lambda {p = LimitProfile {**_rls_profile_data}}) == Undefined
    assert runtime.catch(lambda {p = LimitProfile {**_rls_profile_data, deployment = {**_rls_deployment_data, agenticGatewayPodSelector = {"app": "proxy"}}}}) != Undefined
}

test_rls_rejects_invalid_agentic_gateway_name = lambda {
    assert runtime.catch(lambda {p = LimitProfile {**_rls_profile_data}}) == Undefined
    assert runtime.catch(lambda {p = LimitProfile {**_rls_profile_data, deployment = {**_rls_deployment_data, agenticGatewayPodSelector = {"gateway.networking.k8s.io/gateway-name": "INVALID_NAME"}}}}) != Undefined
}

test_rls_rejects_overlong_agentic_gateway_name = lambda {
    assert runtime.catch(lambda {p = LimitProfile {**_rls_profile_data}}) == Undefined
    assert runtime.catch(lambda {p = LimitProfile {**_rls_profile_data, deployment = {**_rls_deployment_data, agenticGatewayPodSelector = {"gateway.networking.k8s.io/gateway-name": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}}}) != Undefined
}

test_rls_redis_pod_and_cidr_destinations = lambda {
    # An omitted CIDR list defaults to empty; podSelector peers without a
    # namespaceSelector select only Pods in the RLS namespace.
    pod_profile = LimitProfile {
        name: "shared", service: "example-ratelimit", namespace: "inference", port: 8081, domain: "example-shared"
        deployment: {
            image: _rls_deployment_data.image
            gatewayName: _rls_deployment_data.gatewayName
            gatewayNamespace: _rls_deployment_data.gatewayNamespace
            gatewayPodSelector: _rls_deployment_data.gatewayPodSelector
            redisSecret: _rls_deployment_data.redisSecret
            redisPort: _rls_deployment_data.redisPort
            redisPodSelector: _rls_redis_selector
        }
    }
    assert pod_profile.deployment.redisCidrs == []
    pod_rule = limiter_resources(pod_profile, from_catalog(pod_profile, _rls_caps))[3].spec.egress[1]
    assert pod_rule["to"] == [{podSelector: {matchLabels: _rls_redis_selector}}]
    assert pod_rule.ports == [{protocol: "TCP", port: 6379}]
    both = LimitProfile {**_rls_profile_data, deployment = {**_rls_deployment_data, redisPodSelector = _rls_redis_selector}}
    both_rule = limiter_resources(both, from_catalog(both, _rls_caps))[3].spec.egress[1]
    assert both_rule["to"] == [
        {podSelector: {matchLabels: _rls_redis_selector}},
        {ipBlock: {cidr: "192.0.2.40/32"}},
        {ipBlock: {cidr: "2001:db8::/64"}},
    ]
    assert both_rule.ports == [{protocol: "TCP", port: 6379}]
}

test_rls_rejects_mutable_image = lambda {
    assert runtime.catch(lambda {p = LimitProfile {**_rls_profile_data}}) == Undefined
    assert runtime.catch(lambda {p = LimitProfile {**_rls_profile_data, deployment = {**_rls_deployment_data, image = "docker.io/envoyproxy/ratelimit:latest"}}}) != Undefined
}

test_rls_rejects_invalid_gateway_name = lambda {
    assert runtime.catch(lambda {p = LimitProfile {**_rls_profile_data}}) == Undefined
    assert runtime.catch(lambda {p = LimitProfile {**_rls_profile_data, deployment = {**_rls_deployment_data, gatewayName = "INVALID", gatewayPodSelector = {"gateway.networking.k8s.io/gateway-name": "INVALID", "example.net/revision": "stable"}}}}) != Undefined
}

test_rls_rejects_invalid_gateway_namespace = lambda {
    assert runtime.catch(lambda {p = LimitProfile {**_rls_profile_data}}) == Undefined
    assert runtime.catch(lambda {p = LimitProfile {**_rls_profile_data, deployment = {**_rls_deployment_data, gatewayNamespace = "INVALID"}}}) != Undefined
}

test_rls_rejects_empty_gateway_selector = lambda {
    assert runtime.catch(lambda {p = LimitProfile {**_rls_profile_data}}) == Undefined
    assert runtime.catch(lambda {p = LimitProfile {**_rls_profile_data, deployment = {**_rls_deployment_data, gatewayPodSelector = {}}}}) != Undefined
}

test_rls_rejects_mismatched_gateway_selector = lambda {
    assert runtime.catch(lambda {p = LimitProfile {**_rls_profile_data}}) == Undefined
    assert runtime.catch(lambda {p = LimitProfile {**_rls_profile_data, deployment = {**_rls_deployment_data, gatewayPodSelector = {"gateway.networking.k8s.io/gateway-name": "other-gateway"}}}}) != Undefined
}

test_rls_rejects_gateway_selector_without_gateway_name = lambda {
    assert runtime.catch(lambda {p = LimitProfile {**_rls_profile_data}}) == Undefined
    assert runtime.catch(lambda {p = LimitProfile {**_rls_profile_data, deployment = {**_rls_deployment_data, gatewayPodSelector = {"operator.example/revision": "stable"}}}}) != Undefined
}

test_rls_rejects_invalid_redis_secret = lambda {
    assert runtime.catch(lambda {p = LimitProfile {**_rls_profile_data}}) == Undefined
    assert runtime.catch(lambda {p = LimitProfile {**_rls_profile_data, deployment = {**_rls_deployment_data, redisSecret = "bad_secret"}}}) != Undefined
}

test_rls_rejects_empty_redis_ca_secret = lambda {
    assert runtime.catch(lambda {p = LimitProfile {**_rls_profile_data}}) == Undefined
    assert runtime.catch(lambda {p = LimitProfile {**_rls_profile_data, deployment = {**_rls_deployment_data, redisCASecret = ""}}}) != Undefined
}

test_rls_rejects_missing_redis_destination = lambda {
    assert runtime.catch(lambda {p = LimitProfile {**_rls_profile_data}}) == Undefined
    assert runtime.catch(lambda {p = LimitProfile {**_rls_profile_data, deployment = {**_rls_deployment_data, redisCidrs = []}}}) != Undefined
}

test_rls_rejects_empty_redis_pod_selector = lambda {
    assert runtime.catch(lambda {p = LimitProfile {**_rls_profile_data}}) == Undefined
    assert runtime.catch(lambda {p = LimitProfile {**_rls_profile_data, deployment = {**_rls_deployment_data, redisPodSelector = {}}}}) != Undefined
}

test_rls_rejects_broad_redis_cidr = lambda {
    assert runtime.catch(lambda {p = LimitProfile {**_rls_profile_data}}) == Undefined
    assert runtime.catch(lambda {p = LimitProfile {**_rls_profile_data, deployment = {**_rls_deployment_data, redisCidrs = ["0.0.0.0/0"]}}}) != Undefined
}

test_rls_rejects_invalid_redis_cidr = lambda {
    assert runtime.catch(lambda {p = LimitProfile {**_rls_profile_data}}) == Undefined
    assert runtime.catch(lambda {p = LimitProfile {**_rls_profile_data, deployment = {**_rls_deployment_data, redisCidrs = ["not-cidr"]}}}) != Undefined
}

test_rls_rejects_redis_cidr_with_host_bits = lambda {
    assert runtime.catch(lambda {p = LimitProfile {**_rls_profile_data}}) == Undefined
    assert runtime.catch(lambda {p = LimitProfile {**_rls_profile_data, deployment = {**_rls_deployment_data, redisCidrs = ["192.0.2.1/24"]}}}) != Undefined
}

test_rls_rejects_invalid_redis_port = lambda {
    assert runtime.catch(lambda {p = LimitProfile {**_rls_profile_data}}) == Undefined
    assert runtime.catch(lambda {p = LimitProfile {**_rls_profile_data, deployment = {**_rls_deployment_data, redisPort = 0}}}) != Undefined
}

test_rls_rejects_mismatched_render_and_unsafe_resource_name = lambda {
    assert runtime.catch(lambda {rendered = limiter_resources(_rls_profile, _rls_rate)}) == Undefined
    other_profile = LimitProfile {**_rls_profile_data, domain = "other-domain"}
    mismatch = runtime.catch(lambda {rendered = limiter_resources(_rls_profile, from_catalog(other_profile, _rls_caps))})
    reserved_profile = LimitProfile {**_rls_profile_data, service = "thalamus-reserved"}
    reserved = runtime.catch(lambda {rendered = limiter_resources(reserved_profile, from_catalog(reserved_profile, _rls_caps))})
    conflict_profile = LimitProfile {**_rls_profile_data, port = 8080}
    conflict = runtime.catch(lambda {rendered = limiter_resources(conflict_profile, from_catalog(conflict_profile, _rls_caps))})
    assert mismatch != Undefined and reserved != Undefined and conflict != Undefined
}
