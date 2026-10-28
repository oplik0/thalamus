# Pinned upstream source and container contract:
# https://github.com/vllm-project/agentic-api/tree/27e60cf006cb771cf1961ea9909e64d812f5f3c8
# https://github.com/agentgateway/agentgateway/tree/v1.7.0
# The authenticated public gateway restores the verified caller bearer ONLY for
# this trusted pod; the dedicated internal gateway reauthenticates the projected
# key, checks its service, expiry and metadata, and regenerates identity headers.
# Public Gateway-to-Pod traffic requires encrypted cluster transport. Agentic
# Pods use HTTPS and an operator-provisioned CA to the private Gateway.
# Per-instance egress admits only selected DNS, gateway, database and explicit
# tool destinations. NetworkPolicies are additive; protect gateway/pod/ConfigMap
# writers and secret readers, enforce policies with the CNI, and configure public TLS.
# When the public Gateway runs in this namespace, this recipe admits its egress
# to agentic Pods; an external Gateway's source-side egress is operator-owned.
# The operator also owns isolated egress from the internal Gateway to its
# external backend, and any matching backend ingress policy.
# A Kubernetes List is not an atomic rollout: install and verify isolation and
# policies before enabling routes. Image digest/commit fields express operator
# intent, not evidence that the image was built from that commit.
#
# The pinned server always configures persistence. Shared stateless therefore
# only admits POST Responses with explicit store:false and no state references;
# its WebSocket and standalone compact endpoints cannot be safely admitted.
# Team-shared instances use separate databases and share ALL persisted state
# among members of the same team, not per-user isolation. Secret names alone do
# not prove database or credential isolation. DATABASE_URL must name a separately
# provisioned, TLS-protected PostgreSQL database in each instance Secret.
# Every deployment has its own private inference route to its rendered backend;
# only the `/agentic/<service>/<deployment>` routing prefix is removed.
# Gateway frontend metrics are operator-owned in the Gateway namespace. The
# workload catalog reconciler needs explicit apps/deployments extraRules for
# the agentic Deployment; this recipe does not broaden its permissions.
# WebSockets are excluded because this release cannot enforce API-key revocation
# or expiry on an already upgraded connection. Its HTTP operations remain usable.
# NetworkPolicy does not isolate a pod from its own node or localhost; protect
# node-local services and metadata endpoints as well.
# Configure both Gateways' frontend.http maxBufferSize >= 10Mi. The gateway
# buffers request bodies inspected by CEL (including uploads on those routes);
# response streaming is unchanged. The agentic server's default limit is 10Mi.
# Only administrator-controlled MCP DNS names are safe: upstream pins resolved
# addresses but does not reject a hostname resolving to loopback. DNS and CNI
# enforcement are external prerequisites, not consequences of the CIDR list.
import crypto
import json
import net
import regex
import templates.gateway.policy as gateway_policy

_unsafeToolCidrs = ["0.0.0.0/8", "127.0.0.0/8", "169.254.0.0/16", "224.0.0.0/4", "240.0.0.0/4", "::/128", "::1/128", "fe80::/10", "ff00::/8"]

_mcpHost = lambda host: str -> str {
    "[${host}]" if net.is_IPv6(host) else host
}

schema McpDestination:
    # Administrators own the DNS records, addresses, schemes and ports.
    # A caller's requested URL is not evidence of an approved destination.
    host: str
    scheme: str = "https"
    cidr: str
    ports: [int]
    check:
        len(net.parse_CIDR(cidr)) > 0 and net.parse_CIDR(cidr)["mask"] > 0, "MCP destination requires a valid bounded CIDR"
        cidr.split("/")[0] == net.parse_CIDR(cidr)["ip"], "MCP CIDR must use its canonical network address"
        len(ports) > 0 and (all port in ports {port > 0 and port <= 65535}), "MCP destination requires valid TCP ports"
        scheme in ["http", "https"], "MCP scheme must be http or https"
        net.is_IP(host) or regex.match(host, r"^[a-z0-9]([a-z0-9-]*[a-z0-9])?(\.[a-z0-9]([a-z0-9-]*[a-z0-9])?)*\.[a-z]([a-z0-9-]*[a-z0-9])?$"), "MCP host must be a lowercase DNS name or IP"
        not net.is_IP(host) or host == net.parse_IP(host), "MCP IP must use canonical notation"
        not host.endswith(".localhost") and host != "localhost", "MCP host must not name a loopback service"
        not host.startswith("::ffff:") and not cidr.startswith("::ffff:"), "IPv4-mapped IPv6 MCP destinations are not supported"
        not net.is_IP(host) or (net.is_IP_in_CIDR(host, cidr) and (all block in _unsafeToolCidrs {not net.is_IP_in_CIDR(host, block)})), "MCP IP must be nonlocal and inside its allowed CIDR"
        len(net.parse_CIDR(cidr)) > 0 and (all block in _unsafeToolCidrs {not net.is_IP_in_CIDR(net.parse_CIDR(cidr)["ip"], block) and not net.is_IP_in_CIDR(net.parse_CIDR(block)["ip"], cidr)}), "MCP CIDR must not include loopback, link-local or multicast addresses"

schema AgenticWebSearch:
    # Provider base URL and independently approved network destinations.
    # The Secret is provisioned by the operator, never rendered by this recipe.
    provider: str
    host: str
    scheme: str = "https"
    port: int = 443
    path: str = ""
    cidrs: [str]
    apiKeySecret: str = ""
    apiKeyKey: str = "API_KEY"
    check:
        provider in ["you", "brave", "tavily", "serply", "searxng"], "unsupported upstream web-search provider"
        scheme in ["http", "https"] and port > 0 and port <= 65535, "web-search scheme and TCP port must be valid"
        net.is_IP(host) or regex.match(host, r"^[a-z]([a-z0-9-]*[a-z0-9])?(\.[a-z0-9]([a-z0-9-]*[a-z0-9])?)*$"), "web-search host must be a lowercase DNS name or IP"
        not net.is_IP(host) or host == net.parse_IP(host), "web-search IP must use canonical notation"
        host != "localhost" and not host.endswith(".localhost") and not host.startswith("::ffff:"), "web-search host must not be loopback"
        regex.match(path, r"^(/[-a-zA-Z0-9._~!$&'()*+,;=:@/%]*)?$"), "web-search path must be a credential-free URL path"
        len(cidrs) > 0 and (all cidr in cidrs {len(net.parse_CIDR(cidr)) > 0 and net.parse_CIDR(cidr)["mask"] > 0 and cidr.split("/")[0] == net.parse_CIDR(cidr)["ip"] and not cidr.startswith("::ffff:") and (all block in _unsafeToolCidrs {not net.is_IP_in_CIDR(net.parse_CIDR(cidr)["ip"], block) and not net.is_IP_in_CIDR(net.parse_CIDR(block)["ip"], cidr)})}), "web-search requires bounded nonlocal CIDRs"
        not net.is_IP(host) or (any cidr in cidrs {net.is_IP_in_CIDR(host, cidr)}), "web-search IP must be inside an allowed CIDR"
        provider == "searxng" or apiKeySecret != "", "web-search provider requires an API key Secret"
        apiKeySecret == "" or apiKeyKey != "", "web-search Secret key is required"

schema Instance:
    name: str
    team: str = ""
    hostname: str
    databaseSecret: str
    databaseCidrs: [str]
    databasePort: int = 5432
    keySelector: {str:str}
    check:
        name != "" and hostname != "" and databaseSecret != "", "instance identity, hostname and database Secret are required"
        "thalamus.dev/gateway-keys" in keySelector and keySelector["thalamus.dev/gateway-keys"] == "true", "instance must select the trusted key projection"
        len(databaseCidrs) > 0 and (all cidr in databaseCidrs {len(net.parse_CIDR(cidr)) > 0 and net.parse_CIDR(cidr)["mask"] > 0}), "database requires valid bounded CIDRs"
        databasePort > 0 and databasePort <= 65535, "database TCP port must be valid"

schema Recipe:
    name: str
    namespace: str
    mode: str
    image: str
    upstreamCommit: str
    gatewayVersion: str
    publicGateway: str
    publicGatewayNamespace: str
    internalGateway: str
    internalGatewayNamespace: str
    internalGatewayService: str
    # The private Gateway serves HTTPS 443 in the workload namespace.
    internalGatewayTargetPort: int = 443
    internalHostname: str
    internalCAConfigMap: str
    gatewayNamespaceLabels: {str:str}
    gatewayPodLabels: {str:str}
    dnsNamespaceLabels: {str:str}
    dnsPodLabels: {str:str}
    internalGatewayPodLabels: {str:str}
    externalMcp: [McpDestination] = []
    webSearch?: AgenticWebSearch
    trafficClasses: [str] = []
    instances: [Instance]
    _internalCAConfigMapParts = internalCAConfigMap.split(".")
    check:
        name != "" and namespace != "", "recipe identity is required"
        mode in ["shared-stateless", "team-shared"], "unsupported agentic mode"
        gatewayVersion == "v1.7.0", "agentgateway v1.7.0 is required"
        upstreamCommit == "27e60cf006cb771cf1961ea9909e64d812f5f3c8", "unsupported agentic-api source revision"
        len(image.split("@sha256:")) == 2 and len(image.split("@sha256:")[1]) == 64 and regex.match(image.split("@sha256:")[1], "^[0-9a-f]{64}$"), "immutable image digest is required"
        publicGateway != "" and publicGatewayNamespace != "" and internalGateway != "" and internalGatewayNamespace != "", "dedicated gateways are required"
        publicGateway != internalGateway or publicGatewayNamespace != internalGatewayNamespace, "internal and public gateways must be distinct"
        internalGatewayNamespace == namespace and internalGatewayService == internalGateway, "private gateway Service must match its Gateway in the workload namespace"
        internalGatewayTargetPort == 443 and len(internalCAConfigMap) <= 253 and regex.match(internalCAConfigMap, "^thalamus-[a-z0-9]([-a-z0-9]*[a-z0-9])?([.][a-z0-9]([-a-z0-9]*[a-z0-9])?)*$") and (all part in _internalCAConfigMapParts {len(part) <= 63}), "private gateway requires HTTPS 443 and an admin-owned CA ConfigMap"
        internalHostname == "${internalGatewayService}.${internalGatewayNamespace}.svc.cluster.local", "internal hostname must resolve to the dedicated gateway Service"
        len(gatewayNamespaceLabels) > 0 and len(gatewayPodLabels) > 0, "gateway ingress selectors are required"
        len(dnsNamespaceLabels) > 0 and len(dnsPodLabels) > 0 and len(internalGatewayPodLabels) > 0, "DNS and internal gateway selectors are required"
        "gateway.networking.k8s.io/gateway-name" in internalGatewayPodLabels and internalGatewayPodLabels["gateway.networking.k8s.io/gateway-name"] == internalGateway, "private gateway pod selector must identify its Gateway"
        len(instances) > 0, "at least one instance is required"
        mode != "shared-stateless" or len(instances) == 1, "shared stateless requires exactly one instance"
        all i in instances {i.hostname != internalHostname and i.name != "internal"}, "instance cannot collide with the internal route"
        all i in instances {i.keySelector == instances[0].keySelector}, "all instances must use the same projected keys"
        all d in externalMcp {d.host != internalHostname and d.host != "${internalGatewayService}.${internalGatewayNamespace}" and d.host != "${internalGatewayService}.${internalGatewayNamespace}.svc" and (all i in instances {d.host != i.hostname})}, "MCP destination must not be an agentic or internal gateway host"
        (webSearch.host != internalHostname and webSearch.host != "${internalGatewayService}.${internalGatewayNamespace}" and webSearch.host != "${internalGatewayService}.${internalGatewayNamespace}.svc" and (all i in instances {webSearch.host != i.hostname})) if webSearch != Undefined, "web-search target must not be an agentic or internal gateway host"
        len({i.name: True for i in instances}) == len(instances), "instance names must be unique"
        len({i.hostname: True for i in instances}) == len(instances), "instance hostnames must be unique"
        len({i.databaseSecret: True for i in instances}) == len(instances), "each instance requires a separate database Secret"
        mode != "team-shared" or ((all i in instances {regex.match(i.team, "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")}) and len({i.team: True for i in instances}) == len(instances)), "team instances require distinct team UUIDs"

_agenticHash = lambda serviceName: str, instanceName: str, deploymentName: str -> str {
    crypto.sha256("${serviceName}/${instanceName}/${deploymentName}")[:32]
}

_agenticPod = lambda serviceName: str, instanceName: str, deploymentName: str -> str {
    "agentic-pod-${_agenticHash(serviceName, instanceName, deploymentName)}"
}

_instanceAnnotations = lambda serviceName: str, instance: Instance, mode: str -> {str:str} {
    {
        "thalamus.ai/service": serviceName
        "thalamus.ai/agentic-instance": instance.name
        "thalamus.ai/team": ("shared" if mode == "shared-stateless" else instance.team)
    }
}

_deploymentAnnotations = lambda serviceName: str, instance: Instance, deployment: any, mode: str -> {str:str} {
    _instanceAnnotations(serviceName, instance, mode) | {
        "thalamus.ai/deployment": deployment.name
        "thalamus.ai/revision": deployment.revision
    }
}

_mcpRequirement = lambda paths: [str], origins: [str] -> str {
    safeTool = 'type(t) == map && (!("type" in t) || t.type != "mcp" || ((!("server_url" in t) || t.server_url == null || (type(t.server_url) == string && allowed.exists(o, t.server_url == o || t.server_url.startsWith(o + "/") || t.server_url.startsWith(o + "?") || t.server_url.startsWith(o + "#")))) && (!("url" in t) || t.url == null || (type(t.url) == string && allowed.exists(o, t.url == o || t.url.startsWith(o + "/") || t.url.startsWith(o + "?") || t.url.startsWith(o + "#"))))))'
    safeServers = '!("mcp_servers" in b) || b.mcp_servers == null || (type(b.mcp_servers) == list && b.mcp_servers.all(s, type(s) == map && "type" in s && s.type == "url" && "url" in s && type(s.url) == string && allowed.exists(o, s.url == o || s.url.startsWith(o + "/") || s.url.startsWith(o + "?") || s.url.startsWith(o + "#"))))'
    'request.method != "POST" || !(request.path in ${json.encode(paths)}) || ${json.encode(origins)}.with(allowed, json(request.body).with(b, type(b) == map && (!("tools" in b) || b.tools == null || (type(b.tools) == list && b.tools.all(t, ${safeTool}))) && (${safeServers})))'
}

_statelessRequirement = lambda responsePath: str -> str {
    'request.path != ${json.encode(responsePath)} || request.method != "POST" || json(request.body).with(b, type(b) == map && b.store == false && (!("previous_response_id" in b) || b.previous_response_id == null) && (!("conversation" in b) || b.conversation == null) && (!("conversation_id" in b) || b.conversation_id == null))'
}

_noWebSocketRequirement = lambda responsePath: str -> str {
    'request.method != "GET" || (request.path != ${json.encode(responsePath)} && request.path != ${json.encode(responsePath + "/")})'
}

# Only this reviewed function can construct resources; catalog Recipe input
# contains intent and cannot supply or override rendered manifests.
agentic_resources = lambda recipe: Recipe, rateLimit: gateway_policy.RateLimit, deployments: [any] -> [any] {
    _serviceName = recipe.name
    namespace = recipe.namespace
    mode = recipe.mode
    image = recipe.image
    gatewayVersion = recipe.gatewayVersion
    publicGateway = recipe.publicGateway
    publicGatewayNamespace = recipe.publicGatewayNamespace
    internalGateway = recipe.internalGateway
    internalGatewayNamespace = recipe.internalGatewayNamespace
    internalHostname = recipe.internalHostname
    internalCAConfigMap = recipe.internalCAConfigMap
    gatewayNamespaceLabels = recipe.gatewayNamespaceLabels
    gatewayPodLabels = recipe.gatewayPodLabels
    dnsNamespaceLabels = recipe.dnsNamespaceLabels
    dnsPodLabels = recipe.dnsPodLabels
    internalGatewayPodLabels = recipe.internalGatewayPodLabels
    externalMcp = recipe.externalMcp
    webSearch = recipe.webSearch
    trafficClasses = recipe.trafficClasses
    instances = recipe.instances
    assert mode != "team-shared" or (all i in instances {i.team in rateLimit.teamCaps}), "each agentic team requires a configured team budget"
    assert len(deployments) > 0 and (any d in deployments {d.role == "stable" and d.weight > 0}), "agentic public routes require an active stable deployment"

    # Service names are generated from the complete instance/deployment binding.
    # Never admit those Services as request-declared MCP or web-search targets.
    _podNames = [_agenticPod(_serviceName, i.name, d.name) for i in instances for d in deployments]
    _selfHosts = ["${pod}.${namespace}" for pod in _podNames] + ["${pod}.${namespace}.svc" for pod in _podNames] + ["${pod}.${namespace}.svc.cluster.local" for pod in _podNames]
    assert all destination in externalMcp {destination.host not in _selfHosts}, "MCP destination must not be an agentic Service"
    assert webSearch == Undefined or webSearch.host not in _selfHosts, "web-search target must not be an agentic Service"

    # Both sides use the same mutable namespace-local key projection; the
    # private policy reauthenticates and regenerates identity headers.
    _keySelector = instances[0].keySelector
    _policyNamespace = namespace
    _policyRateLimit = rateLimit
    _policyClasses = trafficClasses
    _searchKeyNames = {you: "YOU_API_KEY", brave: "BRAVE_API_KEY", tavily: "TAVILY_API_KEY", serply: "SERPLY_API_KEY", searxng: "SEARXNG_API_KEY"}
    _searchEnv = ([
        {name: "AGENTIC_WEB_SEARCH_PROVIDER", value: webSearch.provider},
        {name: "AGENTIC_WEB_SEARCH_BASE_URL", value: "${webSearch.scheme}://${_mcpHost(webSearch.host)}:${webSearch.port}${webSearch.path}"},
    ] + ([{name: _searchKeyNames[webSearch.provider], valueFrom: {secretKeyRef: {name: webSearch.apiKeySecret, key: webSearch.apiKeyKey}}}] if webSearch.apiKeySecret != "" else []) if webSearch != Undefined else [])
    # The pinned server accepts loopback MCP hosts irrespective of its own
    # allowlist, so the reviewed public policy checks exact approved origins.
    _mcpOrigins = ["${m.scheme}://${_mcpHost(m.host)}:${port}" for m in externalMcp for port in m.ports] + ["${m.scheme}://${_mcpHost(m.host)}" for m in externalMcp if (m.scheme == "https" and 443 in m.ports) or (m.scheme == "http" and 80 in m.ports)]
    _mcpHosts = ",".join([m.host for m in externalMcp])
    _postPaths = ["/v1/chat/completions", "/v1/responses", "/v1/messages"]
    _publicMatches = [{method: "POST", path: {type: "Exact", value: apiPath}} for apiPath in _postPaths] + [{method: "GET", path: {type: "PathPrefix", value: "/v1/models"}}] + ([
        {method: "POST", path: {type: "Exact", value: "/v1/responses/compact"}},
        {method: "GET", path: {type: "PathPrefix", value: "/v1/responses"}},
        {method: "DELETE", path: {type: "PathPrefix", value: "/v1/responses"}},
        {method: "POST", path: {type: "PathPrefix", value: "/v1/conversations"}},
        {method: "GET", path: {type: "PathPrefix", value: "/v1/conversations"}},
        {method: "DELETE", path: {type: "PathPrefix", value: "/v1/conversations"}},
    ] if mode == "team-shared" else [])

    [gateway_policy.Policy {
        namespace = _policyNamespace
        service = _serviceName
        route = "${d.name}-agentic-internal"
        keySelector = _keySelector
        version = gatewayVersion
        rateLimit = _policyRateLimit
        trafficClasses = _policyClasses
        extraRequirements = [('apiKey.kind == "job" && apiKey.deployment == ${json.encode(d.name)}' if d.role == "candidate" else 'apiKey.kind != "job" || apiKey.deployment == ${json.encode(d.name)}')]
    }.item for d in deployments] + [{
        apiVersion: "gateway.networking.k8s.io/v1"
        kind: "HTTPRoute"
        metadata: {
            name: "${d.name}-agentic-internal"
            namespace: namespace
            annotations: {"thalamus.ai/service": _serviceName, "thalamus.ai/deployment": d.name, "thalamus.ai/revision": d.revision}
        }
        spec: {
            parentRefs: [{group: "gateway.networking.k8s.io", kind: "Gateway", name: internalGateway, namespace: internalGatewayNamespace}]
            hostnames: [internalHostname]
            rules: [{
                matches: [{method: "POST", path: {type: "Exact", value: "/agentic/${_serviceName}/${d.name}${apiPath}"}}]
                filters: [{type: "URLRewrite", urlRewrite: {path: {type: "ReplaceFullPath", replaceFullPath: apiPath}}}]
                backendRefs: [{group: "agentgateway.dev", kind: "AgentgatewayBackend", name: "${d.name}-backend"}]
            } for apiPath in _postPaths]
        }
    } for d in deployments] + [gateway_policy.Policy {
        namespace = _policyNamespace
        service = _serviceName
        route = "${d.name}-agentic-models"
        keySelector = _keySelector
        version = gatewayVersion
        rateLimit = _policyRateLimit
        trafficClasses = _policyClasses
        requestRateLimit = True
        extraRequirements = [('apiKey.kind == "job" && apiKey.deployment == ${json.encode(d.name)}' if d.role == "candidate" else 'apiKey.kind != "job" || apiKey.deployment == ${json.encode(d.name)}')]
    }.item for d in deployments] + [{
        apiVersion: "gateway.networking.k8s.io/v1"
        kind: "HTTPRoute"
        metadata: {
            name: "${d.name}-agentic-models"
            namespace: namespace
            annotations: {"thalamus.ai/service": _serviceName, "thalamus.ai/deployment": d.name, "thalamus.ai/revision": d.revision}
        }
        spec: {
            parentRefs: [{group: "gateway.networking.k8s.io", kind: "Gateway", name: internalGateway, namespace: internalGatewayNamespace}]
            hostnames: [internalHostname]
            rules: [{
                matches: [{method: "GET", path: {type: "Exact", value: "/agentic/${_serviceName}/${d.name}/v1/models"}}]
                filters: [{type: "URLRewrite", urlRewrite: {path: {type: "ReplaceFullPath", replaceFullPath: "/v1/models"}}}]
                backendRefs: [{group: "inference.networking.k8s.io", kind: "InferencePool", name: "${d.name}-inference-pool"}]
            }]
        }
    } for d in deployments] + [gateway_policy.Policy {
        namespace = _policyNamespace
        service = _serviceName
        route = "${_serviceName}-${i.name}"
        keySelector = i.keySelector
        version = gatewayVersion
        rateLimit = _policyRateLimit
        trafficClasses = _policyClasses
        applyRateLimit = False
        forwardBearer = True
        extraRequirements = [_mcpRequirement(_postPaths + ["/v1/responses/compact"], _mcpOrigins)] + (['apiKey.team == ${json.encode(i.team)}', _noWebSocketRequirement("/v1/responses")] if mode == "team-shared" else [_statelessRequirement("/v1/responses")])
    }.item for i in instances] + [gateway_policy.Policy {
        namespace = _policyNamespace
        service = _serviceName
        route = "agentic-cand-${_agenticHash(_serviceName, i.name, d.name)}"
        keySelector = i.keySelector
        version = gatewayVersion
        rateLimit = _policyRateLimit
        trafficClasses = _policyClasses
        applyRateLimit = False
        forwardBearer = True
        extraRequirements = [
            'apiKey.kind == "job" && apiKey.deployment == ${json.encode(d.name)}',
            _mcpRequirement(["/candidates/${d.name}${apiPath}" for apiPath in _postPaths], _mcpOrigins),
        ] + (['apiKey.team == ${json.encode(i.team)}', _noWebSocketRequirement("/candidates/${d.name}/v1/responses")] if mode == "team-shared" else [_statelessRequirement("/candidates/${d.name}/v1/responses")])
    }.item for i in instances for d in deployments if d.role == "candidate"] + [{
        apiVersion: "agentgateway.dev/v1alpha1"
        kind: "AgentgatewayBackend"
        metadata: {
            name: "agentic-backend-${_agenticHash(_serviceName, i.name, d.name)}"
            namespace: namespace
            annotations: _deploymentAnnotations(_serviceName, i, d, mode)
        }
        spec: {
            static: {host: "${_agenticPod(_serviceName, i.name, d.name)}.${namespace}.svc.cluster.local", port: 9000}
            policies: {transformation: {response: {set: [{name: "x-thalamus-backend", value: json.encode("${_serviceName}/${d.name}/${d.revision}")}]}}}
        }
    } for i in instances for d in deployments] + [{
        apiVersion: "gateway.networking.k8s.io/v1"
        kind: "HTTPRoute"
        metadata: {
            name: "${_serviceName}-${i.name}"
            namespace: namespace
            annotations: _instanceAnnotations(_serviceName, i, mode) | {"thalamus.ai/agentic-role": "public"}
        }
        spec: {
            parentRefs: [{group: "gateway.networking.k8s.io", kind: "Gateway", name: publicGateway, namespace: publicGatewayNamespace}]
            hostnames: [i.hostname]
            rules: [{
                matches: _publicMatches
                backendRefs: [{group: "agentgateway.dev", kind: "AgentgatewayBackend", name: "agentic-backend-${_agenticHash(_serviceName, i.name, d.name)}", weight: d.weight} for d in deployments if d.role == "stable" and d.weight > 0]
            }]
        }
    } for i in instances] + [{
        apiVersion: "gateway.networking.k8s.io/v1"
        kind: "HTTPRoute"
        metadata: {
            name: "agentic-cand-${_agenticHash(_serviceName, i.name, d.name)}"
            namespace: namespace
            annotations: _deploymentAnnotations(_serviceName, i, d, mode) | {"thalamus.ai/agentic-role": "candidate"}
        }
        spec: {
            parentRefs: [{group: "gateway.networking.k8s.io", kind: "Gateway", name: publicGateway, namespace: publicGatewayNamespace}]
            hostnames: [i.hostname]
            rules: [{
                matches: [{method: "POST", path: {type: "Exact", value: "/candidates/${d.name}${apiPath}"}}]
                filters: [{type: "URLRewrite", urlRewrite: {path: {type: "ReplaceFullPath", replaceFullPath: apiPath}}}]
                backendRefs: [{group: "agentgateway.dev", kind: "AgentgatewayBackend", name: "agentic-backend-${_agenticHash(_serviceName, i.name, d.name)}"}]
            } for apiPath in _postPaths]
        }
    } for i in instances for d in deployments if d.role == "candidate"] + [{
        apiVersion: "apps/v1"
        kind: "Deployment"
        metadata: {name: _agenticPod(_serviceName, i.name, d.name), namespace: namespace, annotations: _deploymentAnnotations(_serviceName, i, d, mode)}
        spec: {
            replicas: 1
            strategy: {type: "RollingUpdate"}
            selector: {matchLabels: {"thalamus.dev/agentic-instance": _agenticPod(_serviceName, i.name, d.name)}}
            template: {
                metadata: {labels: {"thalamus.dev/agentic-instance": _agenticPod(_serviceName, i.name, d.name)}, annotations: _deploymentAnnotations(_serviceName, i, d, mode)}
                spec: {
                    automountServiceAccountToken: False
                    volumes: [{name: "internal-ca", configMap: {name: internalCAConfigMap, items: [{key: "ca.crt", path: "ca.crt"}]}}]
                    containers: [{
                        name: "agentic-api"
                        image: image
                        ports: [{containerPort: 9000}]
                        env: [
                            {name: "LLM_API_BASE", value: "https://${internalHostname}/agentic/${_serviceName}/${d.name}"},
                            {name: "SSL_CERT_FILE", value: "/var/run/agentic-ca/ca.crt"},
                            {name: "DATABASE_URL", valueFrom: {secretKeyRef: {name: i.databaseSecret, key: "DATABASE_URL"}}},
                            {name: "SKIP_LLM_READY_CHECK", value: "true"},
                            {name: "AGENTIC_MCP_ALLOWED_HOSTS", value: _mcpHosts},
                        ] + _searchEnv
                        volumeMounts: [{name: "internal-ca", mountPath: "/var/run/agentic-ca", readOnly: True}]
                        securityContext: {runAsNonRoot: True, allowPrivilegeEscalation: False, capabilities: {drop: ["ALL"]}, seccompProfile: {type: "RuntimeDefault"}}
                        startupProbe: {httpGet: {path: "/health", port: 9000}, periodSeconds: 5, failureThreshold: 60}
                        readinessProbe: {httpGet: {path: "/ready", port: 9000}, periodSeconds: 5, failureThreshold: 30}
                        livenessProbe: {httpGet: {path: "/health", port: 9000}, periodSeconds: 10}
                    }]
                }
            }
        }
    } for i in instances for d in deployments] + [{
        apiVersion: "v1"
        kind: "Service"
        metadata: {name: _agenticPod(_serviceName, i.name, d.name), namespace: namespace, annotations: _deploymentAnnotations(_serviceName, i, d, mode)}
        spec: {selector: {"thalamus.dev/agentic-instance": _agenticPod(_serviceName, i.name, d.name)}, ports: [{port: 9000, targetPort: 9000}]}
    } for i in instances for d in deployments] + [{
        apiVersion: "networking.k8s.io/v1"
        kind: "NetworkPolicy"
        metadata: {name: _agenticPod(_serviceName, i.name, d.name), namespace: namespace}
        spec: {
            podSelector: {matchLabels: {"thalamus.dev/agentic-instance": _agenticPod(_serviceName, i.name, d.name)}}
            policyTypes: ["Ingress", "Egress"]
            ingress: [{from: [{namespaceSelector: {matchLabels: gatewayNamespaceLabels}, podSelector: {matchLabels: gatewayPodLabels}}], ports: [{protocol: "TCP", port: 9000}]}]
            egress: [
                {to: [{namespaceSelector: {matchLabels: dnsNamespaceLabels}, podSelector: {matchLabels: dnsPodLabels}}], ports: [{protocol: "UDP", port: 53}, {protocol: "TCP", port: 53}]},
                {to: [{namespaceSelector: {matchLabels: {"kubernetes.io/metadata.name": internalGatewayNamespace}}, podSelector: {matchLabels: internalGatewayPodLabels}}], ports: [{protocol: "TCP", port: 443}]},
                {to: [{ipBlock: {"cidr": databaseCidr}} for databaseCidr in i.databaseCidrs], ports: [{protocol: "TCP", port: i.databasePort}]},
            ] + [{to: [{ipBlock: {cidr: m.cidr}}], ports: [{protocol: "TCP", "port": mcpPort} for mcpPort in m.ports]} for m in externalMcp] + ([{to: [{ipBlock: {"cidr": searchCidr}} for searchCidr in webSearch.cidrs], ports: [{protocol: "TCP", port: webSearch.port}]}] if webSearch != Undefined else [])
        }
    } for i in instances for d in deployments] + ([{
        apiVersion: "networking.k8s.io/v1"
        kind: "NetworkPolicy"
        metadata: {name: "agentic-egress-${crypto.sha256(_serviceName)[:32]}", namespace: namespace}
        spec: {
            podSelector: {matchLabels: gatewayPodLabels}
            policyTypes: ["Egress"]
            egress: [{
                to: [{podSelector: {matchLabels: {"thalamus.dev/agentic-instance": _agenticPod(_serviceName, i.name, d.name)}}} for i in instances for d in deployments]
                ports: [{protocol: "TCP", port: 9000}]
            }]
        }
    }] if publicGatewayNamespace == namespace else [])
}
