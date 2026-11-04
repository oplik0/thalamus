# Run from the repository root: kcl test ./templates/features
# CEL evaluation and CNI enforcement additionally require the pinned runtime.
import crypto
import json
import runtime
import templates.gateway.policy as gateway_policy

_team_a = "44444444-4444-4444-8444-444444444444"
_team_b = "55555555-5555-4555-8555-555555555555"
_selector = {"thalamus.dev/gateway-keys": "true"}
_test_rate = gateway_policy.RateLimit {
    name: "limiter", namespace: "models", port: 8081, domain: "model"
    teamCaps: {"44444444-4444-4444-8444-444444444444": 1000, "55555555-5555-4555-8555-555555555555": 2000}
    teamWindows: {"44444444-4444-4444-8444-444444444444": "day", "55555555-5555-4555-8555-555555555555": "month"}
}
_test_recipe = {
    name: "model"
    namespace: "models"
    mode: "shared-stateless"
    image: "registry.example/agentic@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    upstreamCommit: "27e60cf006cb771cf1961ea9909e64d812f5f3c8"
    gatewayVersion: "v1.7.0"
    publicGateway: "public"
    publicGatewayNamespace: "gateways"
    internalGateway: "internal"
    internalGatewayNamespace: "models"
    internalGatewayService: "internal"
    internalHostname: "internal.models.svc.cluster.local"
    internalCAConfigMap: "thalamus-internal-ca"
    gatewayNamespaceLabels: {"kubernetes.io/metadata.name": "gateways"}
    gatewayPodLabels: {app: "public-proxy"}
    dnsNamespaceLabels: {"kubernetes.io/metadata.name": "kube-system"}
    dnsPodLabels: {"k8s-app": "kube-dns"}
    internalGatewayPodLabels: {app: "internal-proxy", "gateway.networking.k8s.io/gateway-name": "internal"}
    externalMcp: [{host: "mcp.example.test", cidr: "203.0.113.8/32", ports: [443, 8443]}]
    trafficClasses: ["interactive", "batch"]
    instances: [{name: "shared", hostname: "model.example.test", databaseSecret: "db", databaseCidrs: ["10.40.1.5/32"], keySelector: _selector}]
}
_team_instances = [
    {name: "a", team: _team_a, hostname: "a.example.test", databaseSecret: "db-a", databaseCidrs: ["10.40.1.5/32"], keySelector: _selector},
    {name: "b", team: _team_b, hostname: "b.example.test", databaseSecret: "db-b", databaseCidrs: ["10.40.1.6/32"], databasePort: 6432, keySelector: _selector},
]
_test_deployments = [
    {name: "stable-a", revision: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", role: "stable", weight: 7},
    {name: "stable-b", revision: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", role: "stable", weight: 3},
    {name: "stable-zero", revision: "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc", role: "stable", weight: 0},
    {name: "candidate", revision: "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd", role: "candidate", weight: 0},
]
_hash_a = crypto.sha256("model/shared/stable-a")[:32]
_hash_b = crypto.sha256("model/shared/stable-b")[:32]
_hash_zero = crypto.sha256("model/shared/stable-zero")[:32]
_hash_candidate = crypto.sha256("model/shared/candidate")[:32]

_agenticTestResource = lambda items: [any], kindName: str, resourceName: str -> any {
    matches = [item for item in items if item.kind == kindName and item.metadata.name == resourceName]
    assert len(matches) == 1
    matches[0]
}

test_weighted_public_and_candidate_routing = lambda {
    r = Recipe {**_test_recipe}
    items = agentic_resources(r, _test_rate, _test_deployments)
    public = _agenticTestResource(items, "HTTPRoute", "model-shared")
    assert public.metadata.annotations["thalamus.ai/agentic-role"] == "public"
    assert public.metadata.annotations["thalamus.ai/service"] == "model"
    assert public.metadata.annotations["thalamus.ai/agentic-instance"] == "shared"
    assert public.metadata.annotations["thalamus.ai/team"] == "shared"
    assert public.spec.hostnames == ["model.example.test"]
    targets = public.spec.rules[0].backendRefs
    assert targets == [
        {group: "agentgateway.dev", kind: "AgentgatewayBackend", name: "agentic-backend-${_hash_a}", weight: 7},
        {group: "agentgateway.dev", kind: "AgentgatewayBackend", name: "agentic-backend-${_hash_b}", weight: 3},
    ]
    assert not (any target in targets {target.name == "agentic-backend-${_hash_candidate}" or target.name == "agentic-backend-${_hash_zero}"})
    matches = public.spec.rules[0].matches
    assert all apiPath in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {any m in matches {m.method == "POST" and m.path.type == "Exact" and m.path.value == apiPath}}
    assert any m in matches {m.method == "GET" and m.path.type == "PathPrefix" and m.path.value == "/v1/models"}
    assert not (any m in matches {m.path.value in ["/v1/responses/compact", "/v1/conversations", "/v1/audio/transcriptions", "/v1/completions", "/v1/embeddings", "/v1/messages/count_tokens"]})
    assert not (any m in matches {m.method == "GET" and m.path.value == "/v1/responses"})

    candidate = _agenticTestResource(items, "HTTPRoute", "agentic-cand-${_hash_candidate}")
    assert candidate.metadata.annotations["thalamus.ai/agentic-role"] == "candidate"
    assert candidate.metadata.annotations["thalamus.ai/deployment"] == "candidate"
    assert candidate.metadata.annotations["thalamus.ai/revision"] == _test_deployments[3].revision
    assert candidate.spec.hostnames == public.spec.hostnames
    assert len(candidate.spec.rules) == 3
    assert all rule in candidate.spec.rules {rule.matches[0].method == "POST" and rule.matches[0].path.type == "Exact"}
    assert all rule in candidate.spec.rules {rule.matches[0].path.value == "/candidates/candidate${rule.filters[0].urlRewrite.path.replaceFullPath}"}
    assert all rule in candidate.spec.rules {rule.filters[0].type == "URLRewrite" and rule.filters[0].urlRewrite.path.type == "ReplaceFullPath"}
    assert all rule in candidate.spec.rules {rule.backendRefs == [{group: "agentgateway.dev", kind: "AgentgatewayBackend", name: "agentic-backend-${_hash_candidate}"}]}
    assert all apiPath in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {any rule in candidate.spec.rules {rule.filters[0].urlRewrite.path.replaceFullPath == apiPath}}
    candidate_backend = _agenticTestResource(items, "AgentgatewayBackend", "agentic-backend-${_hash_candidate}")
    assert candidate_backend.spec.policies.transformation.response.set == [{name: "x-thalamus-backend", value: json.encode("model/candidate/${_test_deployments[3].revision}")}]
    assert candidate_backend.spec.static.host == "agentic-pod-${_hash_candidate}.models.svc.cluster.local"
}

test_private_routes_bind_deployment_and_reauthenticate = lambda {
    r = Recipe {**_test_recipe}
    items = agentic_resources(r, _test_rate, _test_deployments)
    assert all d in _test_deployments {len([route for route in items if route.kind == "HTTPRoute" and route.metadata.name == "${d.name}-agentic-internal"]) == 1}
    stable = _agenticTestResource(items, "HTTPRoute", "stable-a-agentic-internal")
    candidate = _agenticTestResource(items, "HTTPRoute", "candidate-agentic-internal")
    assert stable.spec.hostnames == ["internal.models.svc.cluster.local"]
    assert stable.spec.parentRefs[0].name == "internal" and stable.spec.parentRefs[0].namespace == "models"
    assert stable.metadata.annotations["thalamus.ai/deployment"] == "stable-a"
    assert stable.metadata.annotations["thalamus.ai/revision"] == _test_deployments[0].revision
    assert len(stable.spec.rules) == 3
    assert all rule in stable.spec.rules {rule.matches[0].path.type == "Exact"}
    assert all rule in stable.spec.rules {rule.matches[0].path.value == "/agentic/model/stable-a${rule.filters[0].urlRewrite.path.replaceFullPath}"}
    assert all rule in stable.spec.rules {rule.filters[0].type == "URLRewrite" and rule.filters[0].urlRewrite.path.type == "ReplaceFullPath"}
    assert all rule in stable.spec.rules {rule.backendRefs == [{group: "agentgateway.dev", kind: "AgentgatewayBackend", name: "stable-a-backend"}]}
    assert all rule in candidate.spec.rules {rule.backendRefs == [{group: "agentgateway.dev", kind: "AgentgatewayBackend", name: "candidate-backend"}]}
    models = _agenticTestResource(items, "HTTPRoute", "stable-a-agentic-models")
    model_rule = models.spec.rules[0]
    assert model_rule.matches == [{method: "GET", path: {type: "Exact", value: "/agentic/model/stable-a/v1/models"}}]
    assert model_rule.filters == [{type: "URLRewrite", urlRewrite: {path: {type: "ReplaceFullPath", replaceFullPath: "/v1/models"}}}]
    assert model_rule.backendRefs == [{group: "inference.networking.k8s.io", kind: "InferencePool", name: "stable-a-inference-pool"}]
    models_policy = _agenticTestResource(items, "AgentgatewayPolicy", "stable-a-agentic-models-auth")
    candidate_models_policy = _agenticTestResource(items, "AgentgatewayPolicy", "candidate-agentic-models-auth")
    assert models_policy.spec.traffic.apiKeyAuthentication.mode == "Strict"
    assert models_policy.spec.traffic.rateLimit["global"].failureMode == "FailClosed"
    assert all descriptor in models_policy.spec.traffic.rateLimit["global"].descriptors {descriptor.unit == "Requests"}
    assert 'apiKey.kind == "job" && apiKey.deployment == "candidate"' in candidate_models_policy.spec.traffic.authorization.policy.matchExpressions
    stable_policy = _agenticTestResource(items, "AgentgatewayPolicy", "stable-a-agentic-internal-auth")
    candidate_policy = _agenticTestResource(items, "AgentgatewayPolicy", "candidate-agentic-internal-auth")
    assert stable_policy.spec.traffic.apiKeyAuthentication.mode == "Strict"
    assert stable_policy.spec.traffic.apiKeyAuthentication.configMapSelector.matchLabels == _selector
    assert 'apiKey.kind != "job" || apiKey.deployment == "stable-a"' in stable_policy.spec.traffic.authorization.policy.matchExpressions
    assert 'apiKey.kind == "job" && apiKey.deployment == "candidate"' in candidate_policy.spec.traffic.authorization.policy.matchExpressions
    assert any e in stable_policy.spec.traffic.authorization.policy.matchExpressions {'"model" in apiKey.allowed_services' in e}
    assert any e in stable_policy.spec.traffic.authorization.policy.matchExpressions {'request.startTime < timestamp(apiKey.expiry)' in e}
    assert stable_policy.spec.traffic.rateLimit["global"].failureMode == "FailClosed"
    assert all descriptor in stable_policy.spec.traffic.rateLimit["global"].descriptors {descriptor.unit == "Tokens"}
    assert any h in stable_policy.spec.traffic.transformation.request.set {h.name == "x-thalamus-team" and h.value == "apiKey.team"}
}

test_public_and_candidate_guards_precede_rewrite = lambda {
    r = Recipe {**_test_recipe}
    items = agentic_resources(r, _test_rate, _test_deployments)
    public = _agenticTestResource(items, "AgentgatewayPolicy", "model-shared-auth")
    candidate = _agenticTestResource(items, "AgentgatewayPolicy", "agentic-cand-${_hash_candidate}-auth")
    assert public.spec.traffic.apiKeyAuthentication.mode == "Strict" and candidate.spec.traffic.apiKeyAuthentication.mode == "Strict"
    assert public.spec.traffic.apiKeyAuthentication.configMapSelector.matchLabels == _selector
    assert candidate.spec.traffic.transformation.request.set == [{name: "authorization", value: '"Bearer " + apiKey.key.unredacted()'}]
    assert "x-thalamus-user" in candidate.spec.traffic.transformation.request.remove
    assert "rateLimit" not in public.spec.traffic and "rateLimit" not in candidate.spec.traffic
    requirements = candidate.spec.traffic.authorization.policy.matchExpressions
    assert 'apiKey.kind == "job" && apiKey.deployment == "candidate"' in requirements
    stateless = [e for e in requirements if 'b.store == false' in e]
    assert len(stateless) == 1 and '"/candidates/candidate/v1/responses"' in stateless[0]
    assert '"previous_response_id" in b' in stateless[0] and '"conversation" in b' in stateless[0] and '"conversation_id" in b' in stateless[0]
    assert 'type(b.input)' not in stateless[0] and 'b.keys().all' not in stateless[0]
    guard = [e for e in requirements if '"mcp_servers" in b' in e]
    assert len(guard) == 1
    assert all apiPath in ["/candidates/candidate/v1/chat/completions", "/candidates/candidate/v1/responses", "/candidates/candidate/v1/messages"] {'"${apiPath}"' in guard[0]}
    assert '"https://mcp.example.test:443"' in guard[0] and '"https://mcp.example.test:8443"' in guard[0]
    assert 't.server_url.startsWith(o + "/")' in guard[0] and 's.url.startsWith(o + "/")' in guard[0]
    assert '"http://127.0.0.1:9000"' not in guard[0]
}

test_deployment_uses_private_tls_and_scoped_network = lambda {
    r = Recipe {**_test_recipe}
    items = agentic_resources(r, _test_rate, _test_deployments)
    stable = _agenticTestResource(items, "Deployment", "agentic-pod-${_hash_a}")
    candidate = _agenticTestResource(items, "Deployment", "agentic-pod-${_hash_candidate}")
    svc = _agenticTestResource(items, "Service", "agentic-pod-${_hash_a}")
    assert stable.spec.selector.matchLabels == {"thalamus.dev/agentic-instance": "agentic-pod-${_hash_a}"}
    assert svc.spec.selector == stable.spec.selector.matchLabels
    assert stable.metadata.annotations["thalamus.ai/deployment"] == "stable-a"
    assert candidate.metadata.annotations["thalamus.ai/deployment"] == "candidate"
    pod = stable.spec.template.spec
    envs = {e.name: e for e in pod.containers[0].env}
    candidate_envs = {e.name: e for e in candidate.spec.template.spec.containers[0].env}
    assert envs["LLM_API_BASE"].value == "https://internal.models.svc.cluster.local/agentic/model/stable-a"
    assert candidate_envs["LLM_API_BASE"].value == "https://internal.models.svc.cluster.local/agentic/model/candidate"
    assert envs["SSL_CERT_FILE"].value == "/var/run/agentic-ca/ca.crt"
    assert pod.volumes == [{name: "internal-ca", configMap: {name: "thalamus-internal-ca", items: [{key: "ca.crt", path: "ca.crt"}]}}]
    assert pod.containers[0].volumeMounts == [{name: "internal-ca", mountPath: "/var/run/agentic-ca", readOnly: True}]
    assert envs["DATABASE_URL"].valueFrom.secretKeyRef == {name: "db", key: "DATABASE_URL"}
    assert "OPENAI_API_KEY" not in envs and pod.automountServiceAccountToken == False
    assert pod.containers[0].securityContext.runAsNonRoot
    n = _agenticTestResource(items, "NetworkPolicy", "agentic-pod-${_hash_a}").spec
    assert n.policyTypes == ["Ingress", "Egress"]
    assert n.ingress[0]["from"][0].namespaceSelector.matchLabels == {"kubernetes.io/metadata.name": "gateways"}
    assert n.ingress[0]["from"][0].podSelector.matchLabels == {app: "public-proxy"}
    assert n.ingress[0].ports == [{protocol: "TCP", port: 9000}]
    assert n.egress[0].to[0].podSelector.matchLabels == {"k8s-app": "kube-dns"}
    assert n.egress[0].ports == [{protocol: "UDP", port: 53}, {protocol: "TCP", port: 53}]
    assert n.egress[1].to[0].namespaceSelector.matchLabels == {"kubernetes.io/metadata.name": "models"}
    assert n.egress[1].to[0].podSelector.matchLabels == {app: "internal-proxy", "gateway.networking.k8s.io/gateway-name": "internal"}
    assert n.egress[1].ports == [{protocol: "TCP", port: 443}]
    assert n.egress[2].to == [{ipBlock: {cidr: "10.40.1.5/32"}}] and n.egress[2].ports == [{protocol: "TCP", port: 5432}]
    assert n.egress[3].to == [{ipBlock: {cidr: "203.0.113.8/32"}}] and n.egress[3].ports == [{protocol: "TCP", port: 443}, {protocol: "TCP", port: 8443}]
    assert _agenticTestResource(items, "NetworkPolicy", "agentic-pod-${_hash_candidate}").spec.egress == n.egress
}

test_team_shared_state_and_isolation = lambda {
    r = Recipe {**_test_recipe, mode = "team-shared", instances = _team_instances}
    items = agentic_resources(r, _test_rate, _test_deployments)
    public_a = _agenticTestResource(items, "HTTPRoute", "model-a")
    public_b = _agenticTestResource(items, "HTTPRoute", "model-b")
    assert public_a.metadata.annotations["thalamus.ai/team"] == _team_a
    assert public_b.metadata.annotations["thalamus.ai/team"] == _team_b
    policy_a = _agenticTestResource(items, "AgentgatewayPolicy", "model-a-auth")
    policy_b = _agenticTestResource(items, "AgentgatewayPolicy", "model-b-auth")
    assert 'apiKey.team == "${_team_a}"' in policy_a.spec.traffic.authorization.policy.matchExpressions
    assert 'apiKey.team == "${_team_b}"' in policy_b.spec.traffic.authorization.policy.matchExpressions
    assert any e in policy_a.spec.traffic.authorization.policy.matchExpressions {'request.path != "/v1/responses"' in e and 'request.method != "GET"' in e}
    assert not (any e in policy_a.spec.traffic.authorization.policy.matchExpressions {'b.store == false' in e})
    matches = public_a.spec.rules[0].matches
    assert any m in matches {m.method == "POST" and m.path.value == "/v1/responses/compact"}
    assert any m in matches {m.method == "GET" and m.path.value == "/v1/responses" and m.path.type == "PathPrefix"}
    assert any m in matches {m.method == "DELETE" and m.path.value == "/v1/responses"}
    assert all method in ["POST", "GET", "DELETE"] {any m in matches {m.method == method and m.path.value == "/v1/conversations"}}
    hash_a = crypto.sha256("model/a/candidate")[:32]
    hash_b = crypto.sha256("model/b/candidate")[:32]
    candidate_a = _agenticTestResource(items, "AgentgatewayPolicy", "agentic-cand-${hash_a}-auth")
    candidate_b = _agenticTestResource(items, "AgentgatewayPolicy", "agentic-cand-${hash_b}-auth")
    assert 'apiKey.team == "${_team_a}"' in candidate_a.spec.traffic.authorization.policy.matchExpressions
    assert 'apiKey.team == "${_team_b}"' in candidate_b.spec.traffic.authorization.policy.matchExpressions
    pod_a = _agenticTestResource(items, "Deployment", "agentic-pod-${hash_a}")
    pod_b = _agenticTestResource(items, "Deployment", "agentic-pod-${hash_b}")
    env_a = {e.name: e for e in pod_a.spec.template.spec.containers[0].env}
    env_b = {e.name: e for e in pod_b.spec.template.spec.containers[0].env}
    assert env_a["DATABASE_URL"].valueFrom.secretKeyRef.name == "db-a"
    assert env_b["DATABASE_URL"].valueFrom.secretKeyRef.name == "db-b"
    assert _agenticTestResource(items, "NetworkPolicy", "agentic-pod-${hash_b}").spec.egress[2].ports == [{protocol: "TCP", port: 6432}]
}

test_gateway_source_egress_is_namespace_local = lambda {
    external = Recipe {**_test_recipe}
    external_items = agentic_resources(external, _test_rate, _test_deployments)
    external_policies = [p for p in external_items if p.kind == "NetworkPolicy"]
    assert all p in external_policies {p.metadata.namespace == "models"}
    assert not (any p in external_policies {p.spec.podSelector.matchLabels == {app: "public-proxy"}})
    local = Recipe {**_test_recipe, mode = "team-shared", instances = _team_instances, publicGatewayNamespace = "models", gatewayNamespaceLabels = {"kubernetes.io/metadata.name": "models"}}
    local_items = agentic_resources(local, _test_rate, _test_deployments)
    sources = [p for p in local_items if p.kind == "NetworkPolicy" and p.spec.podSelector.matchLabels == {app: "public-proxy"}]
    assert len(sources) == 1 and sources[0].spec.policyTypes == ["Egress"]
    assert len(sources[0].spec.egress) == 1 and sources[0].spec.egress[0].ports == [{protocol: "TCP", port: 9000}]
    expected_hashes = [crypto.sha256("model/${instance.name}/${d.name}")[:32] for instance in _team_instances for d in _test_deployments]
    assert len(expected_hashes) == 8
    assert sources[0].spec.egress[0].to == [{podSelector: {matchLabels: {"thalamus.dev/agentic-instance": "agentic-pod-${hashValue}"}}} for hashValue in expected_hashes]
}

test_search_and_mcp_configuration = lambda {
    r = Recipe {**_test_recipe, webSearch = {provider: "brave", host: "api.search.brave.com", cidrs: ["203.0.113.9/32"], apiKeySecret: "search-key", apiKeyKey: "token"}}
    items = agentic_resources(r, _test_rate, _test_deployments)
    pod = _agenticTestResource(items, "Deployment", "agentic-pod-${_hash_candidate}")
    envs = {e.name: e for e in pod.spec.template.spec.containers[0].env}
    assert envs["BRAVE_API_KEY"].valueFrom.secretKeyRef == {name: "search-key", key: "token"}
    assert envs["AGENTIC_WEB_SEARCH_BASE_URL"].value == "https://api.search.brave.com:443"
    network = _agenticTestResource(items, "NetworkPolicy", "agentic-pod-${_hash_candidate}").spec
    assert network.egress[4].to == [{ipBlock: {cidr: "203.0.113.9/32"}}] and network.egress[4].ports == [{protocol: "TCP", port: 443}]
    internal = Recipe {**_test_recipe, externalMcp = [{host: "mcp.corp.svc.cluster.local", scheme: "http", cidr: "10.22.4.0/24", ports: [8080]}], webSearch = {provider: "searxng", host: "search.corp.internal", scheme: "http", port: 8080, path: "/tenant", cidrs: ["10.22.5.0/24"]}}
    internal_items = agentic_resources(internal, _test_rate, _test_deployments)
    internal_pod = _agenticTestResource(internal_items, "Deployment", "agentic-pod-${_hash_a}")
    internal_envs = {e.name: e for e in internal_pod.spec.template.spec.containers[0].env}
    assert internal_envs["AGENTIC_MCP_ALLOWED_HOSTS"].value == "mcp.corp.svc.cluster.local"
    assert "SEARXNG_API_KEY" not in internal_envs
    public_policy = _agenticTestResource(internal_items, "AgentgatewayPolicy", "model-shared-auth")
    assert any e in public_policy.spec.traffic.authorization.policy.matchExpressions {'"http://mcp.corp.svc.cluster.local:8080"' in e}
    internal_network = _agenticTestResource(internal_items, "NetworkPolicy", "agentic-pod-${_hash_a}").spec
    assert internal_network.egress[3].to == [{ipBlock: {cidr: "10.22.4.0/24"}}]
    assert internal_network.egress[4].to == [{ipBlock: {cidr: "10.22.5.0/24"}}]
    without_mcp = Recipe {**_test_recipe, externalMcp = []}
    without_items = agentic_resources(without_mcp, _test_rate, _test_deployments)
    without_guard = _agenticTestResource(without_items, "AgentgatewayPolicy", "model-shared-auth").spec.traffic.authorization.policy.matchExpressions
    assert any e in without_guard {'"mcp_servers" in b' in e and '[].with(allowed,' in e}
}

test_invalid_recipe_boundaries = lambda {
    image_err = runtime.catch(lambda { r = Recipe {**_test_recipe, image = "registry.example/agentic:latest"} })
    commit_err = runtime.catch(lambda { r = Recipe {**_test_recipe, upstreamCommit = "wrong"} })
    gateway_err = runtime.catch(lambda { r = Recipe {**_test_recipe, gatewayVersion = "v1.6.0"} })
    namespace_err = runtime.catch(lambda { r = Recipe {**_test_recipe, internalGatewayNamespace = "elsewhere"} })
    service_err = runtime.catch(lambda { r = Recipe {**_test_recipe, internalGatewayService = "other"} })
    port_err = runtime.catch(lambda { r = Recipe {**_test_recipe, internalGatewayTargetPort = 80} })
    ca_err = runtime.catch(lambda { r = Recipe {**_test_recipe, internalCAConfigMap = ""} })
    unreserved_ca = runtime.catch(lambda { r = Recipe {**_test_recipe, internalCAConfigMap = "untrusted-ca"} })
    wrong_gateway = runtime.catch(lambda { r = Recipe {**_test_recipe, internalGatewayPodLabels = {app: "internal-proxy", "gateway.networking.k8s.io/gateway-name": "other"}} })
    assert image_err != Undefined and commit_err != Undefined and gateway_err != Undefined and namespace_err != Undefined and service_err != Undefined and port_err != Undefined and ca_err != Undefined and unreserved_ca != Undefined and wrong_gateway != Undefined
    injected = runtime.catch(lambda {
        untrusted = _test_recipe | {items: [{kind: "Secret"}]}
        r: Recipe = untrusted
    })
    assert injected != Undefined
}

test_mcp_rejects_unbounded_ipv4 = lambda {
    r = Recipe {**_test_recipe}
    assert r.externalMcp[0].cidr == "203.0.113.8/32"
    assert runtime.catch(lambda { bad = Recipe {**_test_recipe, externalMcp = [{host: "mcp.example.test", cidr: "0.0.0.0/0", ports: [443, 8443]}]} }) != Undefined
}

test_mcp_rejects_unbounded_ipv6 = lambda {
    r = Recipe {**_test_recipe}
    assert r.externalMcp[0].cidr == "203.0.113.8/32"
    assert runtime.catch(lambda { bad = Recipe {**_test_recipe, externalMcp = [{host: "mcp.example.test", cidr: "::/0", ports: [443, 8443]}]} }) != Undefined
}

test_mcp_rejects_malformed_cidr = lambda {
    r = Recipe {**_test_recipe}
    assert r.externalMcp[0].cidr == "203.0.113.8/32"
    assert runtime.catch(lambda { bad = Recipe {**_test_recipe, externalMcp = [{host: "mcp.example.test", cidr: "not-a-network", ports: [443, 8443]}]} }) != Undefined
}

test_mcp_rejects_invalid_port = lambda {
    r = Recipe {**_test_recipe}
    assert r.externalMcp[0].ports == [443, 8443]
    assert runtime.catch(lambda { bad = Recipe {**_test_recipe, externalMcp = [{host: "mcp.example.test", cidr: "203.0.113.8/32", ports: [0]}]} }) != Undefined
}

test_internal_gateway_rejects_empty_selector = lambda {
    r = Recipe {**_test_recipe}
    assert r.internalGatewayPodLabels == _test_recipe.internalGatewayPodLabels
    assert runtime.catch(lambda { bad = Recipe {**_test_recipe, internalGatewayPodLabels = {}} }) != Undefined
}

test_mcp_rejects_localhost = lambda {
    origin = {host: "mcp.example.test", cidr: "203.0.113.8/32", ports: [443, 8443]}
    valid = McpDestination {**origin}
    assert valid.host == origin.host
    assert runtime.catch(lambda { d = McpDestination {**origin, host = "localhost"} }) != Undefined
}

test_mcp_rejects_loopback_cidr = lambda {
    origin = {host: "mcp.example.test", cidr: "203.0.113.8/32", ports: [443, 8443]}
    valid = McpDestination {**origin}
    assert valid.cidr == origin.cidr
    assert runtime.catch(lambda { d = McpDestination {**origin, cidr = "127.0.0.0/8"} }) != Undefined
}

test_mcp_rejects_link_local_cidr = lambda {
    origin = {host: "mcp.example.test", cidr: "203.0.113.8/32", ports: [443, 8443]}
    valid = McpDestination {**origin}
    assert valid.cidr == origin.cidr
    assert runtime.catch(lambda { d = McpDestination {**origin, cidr = "169.254.0.0/16"} }) != Undefined
}

test_mcp_rejects_ip_outside_cidr = lambda {
    origin = {host: "203.0.113.8", cidr: "203.0.113.8/32", ports: [443, 8443]}
    valid = McpDestination {**origin}
    assert valid.host == origin.host
    assert runtime.catch(lambda { d = McpDestination {**origin, host = "203.0.113.9"} }) != Undefined
}

test_mcp_rejects_agentic_pod_host = lambda {
    r = Recipe {**_test_recipe}
    items = agentic_resources(r, _test_rate, _test_deployments)
    assert len(items) > 0
    assert runtime.catch(lambda {
        bad = Recipe {**_test_recipe, externalMcp = [{host: "agentic-pod-${_hash_a}.models.svc.cluster.local", cidr: "203.0.113.8/32", ports: [443, 8443]}]}
        bad_items = agentic_resources(bad, _test_rate, _test_deployments)
    }) != Undefined
}

test_web_search_rejects_unknown_provider = lambda {
    brave = {provider: "brave", host: "search.example.test", cidrs: ["203.0.113.9/32"], apiKeySecret: "key"}
    r = Recipe {**_test_recipe, webSearch = brave}
    assert r.webSearch.provider == "brave"
    assert runtime.catch(lambda { bad = Recipe {**_test_recipe, webSearch = {**brave, provider = "other"}} }) != Undefined
}

test_web_search_rejects_missing_api_key = lambda {
    brave = {provider: "brave", host: "search.example.test", cidrs: ["203.0.113.9/32"], apiKeySecret: "key"}
    r = Recipe {**_test_recipe, webSearch = brave}
    assert r.webSearch.apiKeySecret == "key"
    assert runtime.catch(lambda { bad = Recipe {**_test_recipe, webSearch = {**brave, apiKeySecret = ""}} }) != Undefined
}

test_web_search_rejects_unbounded_cidr = lambda {
    searxng = {provider: "searxng", host: "search.example.test", cidrs: ["203.0.113.9/32"]}
    r = Recipe {**_test_recipe, webSearch = searxng}
    assert r.webSearch.cidrs == searxng.cidrs
    assert runtime.catch(lambda { bad = Recipe {**_test_recipe, webSearch = {**searxng, cidrs = ["0.0.0.0/0"]}} }) != Undefined
}

test_web_search_rejects_loopback_host = lambda {
    searxng = {provider: "searxng", host: "203.0.113.9", cidrs: ["203.0.113.9/32"]}
    r = Recipe {**_test_recipe, webSearch = searxng}
    assert r.webSearch.host == searxng.host
    assert runtime.catch(lambda { bad = Recipe {**_test_recipe, webSearch = {**searxng, host = "127.0.0.1"}} }) != Undefined
}

test_web_search_rejects_agentic_pod_host = lambda {
    searxng = {provider: "searxng", host: "search.example.test", cidrs: ["203.0.113.9/32"]}
    r = Recipe {**_test_recipe, webSearch = searxng}
    items = agentic_resources(r, _test_rate, _test_deployments)
    assert len(items) > 0
    assert runtime.catch(lambda {
        bad = Recipe {**_test_recipe, webSearch = {**searxng, host = "agentic-pod-${_hash_a}.models.svc.cluster.local"}}
        bad_items = agentic_resources(bad, _test_rate, _test_deployments)
    }) != Undefined
}

test_team_rejects_shared_database_secret = lambda {
    first = _team_instances[0]
    second = _team_instances[1]
    r = Recipe {**_test_recipe, mode = "team-shared", instances = [first, second]}
    assert len(r.instances) == 2
    assert runtime.catch(lambda { bad = Recipe {**_test_recipe, mode = "team-shared", instances = [first, {**second, databaseSecret = "db-a"}]} }) != Undefined
}

test_team_rejects_different_key_selectors = lambda {
    first = _team_instances[0]
    second = _team_instances[1]
    r = Recipe {**_test_recipe, mode = "team-shared", instances = [first, second]}
    assert len(r.instances) == 2
    assert runtime.catch(lambda { bad = Recipe {**_test_recipe, mode = "team-shared", instances = [first, {**second, keySelector = {"thalamus.dev/gateway-keys": "true", other: "different"}}]} }) != Undefined
}

test_team_rejects_duplicate_team = lambda {
    first = _team_instances[0]
    second = _team_instances[1]
    r = Recipe {**_test_recipe, mode = "team-shared", instances = [first, second]}
    assert len(r.instances) == 2
    assert runtime.catch(lambda { bad = Recipe {**_test_recipe, mode = "team-shared", instances = [first, {**second, team = _team_a}]} }) != Undefined
}

test_team_rejects_missing_budget = lambda {
    first = _team_instances[0]
    r = Recipe {**_test_recipe, mode = "team-shared", instances = [first]}
    items = agentic_resources(r, _test_rate, _test_deployments)
    assert len(items) > 0
    assert runtime.catch(lambda {
        bad = Recipe {**_test_recipe, mode = "team-shared", instances = [{**first, team = "66666666-6666-4666-8666-666666666666"}]}
        bad_items = agentic_resources(bad, _test_rate, _test_deployments)
    }) != Undefined
}
