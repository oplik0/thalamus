# Agentgateway Kubernetes v1.7.0 policy and Envoy RLS configuration.
# https://github.com/agentgateway/agentgateway/blob/v1.7.0/controller/api/v1alpha1/agentgateway/agentgateway_policy_types.go
# https://github.com/agentgateway/agentgateway/blob/v1.7.0/crates/agentgateway/src/http/remoteratelimit.rs
# https://github.com/agentgateway/agentgateway/blob/v1.7.0/controller/test/e2e/testdata/rate-limit/global/rate-limit-server.yaml
# Key ConfigMaps are mutable worker projections; no keys or ownership labels are
# rendered here. limits.k renders the matching Envoy rate-limit service and
# configuration once per profile. The platform may install Redis with
# persistent storage, or the operator may provide external Redis. Authentication
# and TLS Secrets must be provisioned by the operator.
# Isolation requires an enforcing CNI and protected namespace/pod labels.
# Rendering alone does not establish readiness or enforcement.
# A healthy RLS with an unknown domain can return OK; verify the loaded domain
# and actual enforcement, not only policy status. Token use is amended after the
# response asynchronously, so concurrent requests can overshoot and a failed
# amendment can lose accounting even when request failureMode is FailClosed.
# The upstream 3e085e5b RLS defines month as 30 days from the Unix epoch.
import crypto
import json
import net
import regex
import yaml

_uuid = "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$"
_dns = "^[a-z0-9]([-a-z0-9]*[a-z0-9])?$"
_dns_subdomain = "^[a-z0-9]([-a-z0-9]*[a-z0-9])?([.][a-z0-9]([-a-z0-9]*[a-z0-9])?)*$"

schema RateLimit:
    # Envoy RLS gRPC Service. The server must use the same domain/configuration.
    name: str
    namespace: str
    port: int
    domain: str
    # Both maps are derived from the same reviewed budget-caps.json snapshot.
    # Empty maps admit no teams but are valid for a fresh installation.
    teamCaps: {str:int}
    teamWindows: {str:str}
    check:
        len(name) <= 63 and regex.match(name, _dns) and len(namespace) <= 63 and regex.match(namespace, _dns), "rate limit Service name and namespace must be DNS labels"
        port > 0 and port <= 65535, "rate limit gRPC port must be valid"
        len(domain) > 0 and len(domain) <= 256, "rate limit domain must be a nonempty upstream ShortString"
        all team in teamCaps {regex.match(team, _uuid) and teamCaps[team] > 0 and teamCaps[team] <= 4294967295}, "team caps require UUIDs and positive uint32 token limits"
        len(teamWindows) == len(teamCaps) and (all team in teamCaps {team in teamWindows and teamWindows[team] in ["day", "month"]}), "each team cap requires its day or month window"

# Redis and its credentials are provisioned outside this renderer. REDIS_URL
# Secret key "url" must be host:port on redisPort and resolve to the selected
# same-namespace Redis Pods and/or approved CIDRs; key "auth" is a password or
# username:password accepted by RLS.
schema LimitDeployment:
    image: str
    gatewayName: str
    gatewayNamespace: str
    gatewayPodSelector: {str:str}
    # Optional trusted internal agentic Gateway in this limiter's namespace.
    agenticGatewayPodSelector?: {str:str}
    redisSecret: str
    redisPort: int
    redisCidrs: [str] = []
    redisPodSelector?: {str:str}
    redisCASecret?: str
    _redisSecretParts = redisSecret.split(".")
    _redisCAParts = redisCASecret.split(".") if redisCASecret != Undefined else []
    check:
        regex.match(image, r"^[^@\s]+@sha256:[0-9a-f]{64}$"), "rate limiter image requires an immutable sha256 digest"
        len(gatewayName) <= 63 and regex.match(gatewayName, _dns) and len(gatewayNamespace) <= 63 and regex.match(gatewayNamespace, _dns), "rate limiter gateway identity must use DNS labels"
        "gateway.networking.k8s.io/gateway-name" in gatewayPodSelector and gatewayPodSelector["gateway.networking.k8s.io/gateway-name"] == gatewayName, "gateway pod selector must bind the configured Gateway name"
        (len(agenticGatewayPodSelector) > 0 and "gateway.networking.k8s.io/gateway-name" in agenticGatewayPodSelector and len(agenticGatewayPodSelector["gateway.networking.k8s.io/gateway-name"]) <= 63 and regex.match(agenticGatewayPodSelector["gateway.networking.k8s.io/gateway-name"], _dns)) if agenticGatewayPodSelector != Undefined, "agentic Gateway selector requires a valid gateway-name label"
        len(redisSecret) <= 253 and regex.match(redisSecret, _dns_subdomain) and (all part in _redisSecretParts {len(part) <= 63}), "Redis Secret must be a DNS subdomain"
        redisPort > 0 and redisPort <= 65535, "Redis port must be valid"
        len(redisCidrs) > 0 or redisPodSelector != Undefined, "Redis requires a pod selector or bounded CIDRs"
        len(redisPodSelector) > 0 if redisPodSelector != Undefined, "Redis pod selector must not select every pod"
        all cidr in redisCidrs {len(net.parse_CIDR(cidr)) > 0 and net.parse_CIDR(cidr)["mask"] > 0 and cidr.split("/")[0] == net.parse_CIDR(cidr)["ip"]}, "Redis CIDRs must be canonical and bounded"
        (len(redisCASecret) <= 253 and regex.match(redisCASecret, _dns_subdomain) and (all part in _redisCAParts {len(part) <= 63})) if redisCASecret != Undefined, "Redis CA Secret must be a DNS subdomain"

# Reviewed catalog profile shared by all services in the same budget domain.
# The reference name identifies catalog data, not a Kubernetes resource.
schema LimitProfile:
    name: str
    service: str
    namespace: str
    port: int
    domain: str
    deployment?: LimitDeployment
    check:
        len(name) <= 63 and regex.match(name, _dns), "limit profile name must be a DNS label"
        len(service) <= 63 and regex.match(service, _dns) and len(namespace) <= 63 and regex.match(namespace, _dns), "rate limit Service name and namespace must be DNS labels"
        port > 0 and port <= 65535, "rate limit gRPC port must be valid"
        len(domain) > 0 and len(domain) <= 256, "rate limit domain must be a nonempty upstream ShortString"

# This is the same structure consumed by the worker's trusted budget snapshot.
# Zero-cap teams are deliberately excluded from authorization and rate maps.
schema BudgetCap:
    team_id: str
    max_tokens: int
    unit: str
    window: str
    # The worker treats omission as ["standard"]; do not merge that default
    # into an explicitly supplied list.
    allowed_classes?: [str]
    check:
        regex.match(team_id, _uuid), "budget cap team must be a UUID"
        max_tokens >= 0 and max_tokens <= 4294967295, "budget cap must fit uint32 tokens"
        unit == "tokens" and window in ["day", "month"], "budget cap unit/window is invalid"
        allowed_classes == Undefined or ("standard" in allowed_classes and len({c: True for c in allowed_classes}) == len(allowed_classes) and (all c in allowed_classes {len(c) <= 63 and regex.match(c, "^[a-z0-9][a-z0-9-]*$")})), "budget cap classes must be distinct and include standard"

schema BudgetCaps:
    teams: [BudgetCap]
    check:
        len({cap.team_id: True for cap in teams}) == len(teams), "budget cap teams must be unique"

from_catalog = lambda profile: LimitProfile, caps: BudgetCaps -> RateLimit {
    RateLimit {
        name: profile.service
        namespace: profile.namespace
        port: profile.port
        domain: profile.domain
        teamCaps: {cap.team_id: cap.max_tokens for cap in caps.teams if cap.max_tokens > 0}
        teamWindows: {cap.team_id: cap.window for cap in caps.teams if cap.max_tokens > 0}
    }
}

# A ConfigMap in the RLS namespace; never give it the worker's thalamus- prefix
# or managed-by label. A generic descriptor without rate_limit is an Envoy RLS
# whitelist. The explicit one-unit fallback is deliberately restrictive; the
# v1.7 proxy supplies the authoritative CEL limitOverride on every descriptor.
# Mount config.yaml under /data/ratelimit/config/ with RUNTIME_ROOT=/data and
# RUNTIME_SUBDIRECTORY=ratelimit. The upstream e2e server pins 3e085e5b.
limiter_config = lambda namespace: str, rateLimit: RateLimit -> any {
    assert namespace == rateLimit.namespace, "rate limit config must be in its Service namespace"
    configName = ("${rateLimit.domain}-ratelimit-config" if len(rateLimit.domain) <= 46 and regex.match(rateLimit.domain, _dns) and not regex.match(rateLimit.domain, "^thalamus-") else "gateway-ratelimit-${crypto.sha256(rateLimit.domain)[:32]}")
    {
        apiVersion: "v1"
        kind: "ConfigMap"
        metadata: {name: configName, namespace: namespace}
        data: {"config.yaml": yaml.encode({
            domain: rateLimit.domain
            descriptors: [{key: dimension, rate_limit: {unit: "day", requests_per_unit: 1}} for dimension in ["key_id", "project", "team", "benchmark_key"]]
        })}
    }
}

# Install once per Gateway under the gateway administrator's ownership, not
# once per service. Dimensions are bounded catalog/identity fields; API key,
# user, run and session IDs are deliberately not metric labels. Missing or
# projectless identity is left unattributed, not invented as a UUID.
metrics_policy = lambda namespace: str, gateway: str -> any {
    gatewayParts = gateway.split(".")
    assert len(namespace) <= 63 and regex.match(namespace, _dns) and len(gateway) <= 253 and regex.match(gateway, _dns_subdomain) and (all part in gatewayParts {len(part) <= 63}), "metrics Gateway name and namespace must be DNS names"
    policyName = ("gateway-usage-${gateway}" if len(gateway) <= 49 and regex.match(gateway, _dns) else "gateway-usage-${crypto.sha256(gateway)[:32]}")
    {
        apiVersion: "agentgateway.dev/v1alpha1"
        kind: "AgentgatewayPolicy"
        metadata: {name: policyName, namespace: namespace}
        spec: {
            targetRefs: [{group: "gateway.networking.k8s.io", kind: "Gateway", name: gateway}]
            frontend: {metrics: {attributes: {add: [
                {name: "team_id", expression: 'coalesce(apiKey.team, "")'},
                {name: "project_id", expression: 'coalesce(apiKey.project, "")'},
                {name: "service", expression: 'coalesce(metadata.thalamus_service, "")'},
                {name: "traffic_class", expression: 'coalesce(apiKey.class, "")'},
            ]}}}
        }
    }
}

schema Policy:
    namespace: str
    service: str
    route: str
    keySelector: {str:str}
    version: str
    rateLimit: RateLimit
    # Nonempty binds an isolated candidate chat or deployment benchmark route
    # to the run's immutable deployment. Public routes use the empty default.
    candidateDeployment: str = ""
    # Direct InferencePool completion routes cannot report LLM token usage.
    # They accept only benchmark run keys and enforce a separate Requests
    # counter from the projected request_budget, not a token ceiling.
    benchmark: bool = False
    # Metadata routes use the existing shared budget counters, debiting one
    # unit per request. This does not report or claim model token usage.
    requestRateLimit: bool = False
    # Reviewed objective names. Empty disables flow-control header assignment.
    trafficClasses: [str] = []
    # Only trusted agentic pods may receive the validated caller bearer; they
    # must connect over protected transport to an internal reauthenticating route.
    forwardBearer: bool = False
    # Only a public static agentic hop may defer charging to its internal AI hop.
    applyRateLimit: bool = True
    # Additional reviewed, route-specific Require expressions, e.g. tenant/body.
    extraRequirements: [str] = []

    # JSON syntax is also CEL syntax. Config-supplied identifiers are encoded,
    # never interpolated as raw CEL. Validation guarantees every limitOverride
    # evaluates to a positive uint32; v1.7 drops a descriptor on CEL errors.
    _caps = json.encode(rateLimit.teamCaps)
    _windows = json.encode(rateLimit.teamWindows)
    _classNames = json.encode(trafficClasses)
    _teamCap = '${_caps}[apiKey.team]'
    _keyLimit = '(apiKey.budget == null || apiKey.budget > ${_teamCap} ? ${_teamCap} : apiKey.budget)'
    _projectLimit = '(apiKey.project_budget == null || apiKey.project_budget > ${_teamCap} ? ${_teamCap} : apiKey.project_budget)'
    _unit = '${_windows}[apiKey.team]'

    # v1.7's Kubernetes Transform has no prefix/wildcard removal. Known
    # attribution/priority headers are removed or overwritten; reject unknown
    # names in these reserved namespaces rather than letting them pass through.
    _knownHeaders = '["x-thalamus-user", "x-thalamus-project", "x-thalamus-team", "x-thalamus-class", "x-thalamus-backend", "x-llm-d-inference-objective", "x-llm-d-inference-fairness-id"]'
    _requirements = [
        'type(apiKey.key_id) == string && apiKey.key_id.matches("${_uuid}")',
        'type(apiKey.user) == string && apiKey.user.matches("${_uuid}")',
        'type(apiKey.creator) == string && apiKey.creator.matches("${_uuid}")',
        'type(apiKey.team) == string && apiKey.team.matches("${_uuid}") && apiKey.team in ${_caps}',
        'type(apiKey.kind) == string && apiKey.kind in ["personal", "project", "job", "debug"]',
        '(apiKey.project == null && apiKey.kind == "personal") || (type(apiKey.project) == string && apiKey.project.matches("${_uuid}"))',
        'type(apiKey.allowed_services) == list && ${json.encode(service)} in apiKey.allowed_services',
        'apiKey.expiry == null || (type(apiKey.expiry) == string && request.startTime < timestamp(apiKey.expiry))',
        ('(apiKey.budget == null || (type(apiKey.budget) == int && apiKey.budget >= 0)) && (apiKey.project_budget == null || (type(apiKey.project_budget) == int && apiKey.project_budget >= 0))' if benchmark else '(apiKey.budget == null || (type(apiKey.budget) == int && apiKey.budget > 0)) && (apiKey.project_budget == null || (type(apiKey.project_budget) == int && apiKey.project_budget > 0))'),
        '(apiKey.project != null || apiKey.project_budget == null) && type(apiKey.team_budget) == int && apiKey.team_budget == ${_teamCap} && apiKey.budget_window == ${_unit}',
        'apiKey.kind != "job" || (type(apiKey.run_id) == string && apiKey.run_id.matches("${_uuid}") && type(apiKey.deployment) == string && apiKey.deployment != "" && apiKey.run_kind in ["validation", "benchmark"] && apiKey.expiry != null && apiKey.budget != null)',
        'apiKey.kind != "debug" || (type(apiKey.debug_session_id) == string && apiKey.debug_session_id.matches("${_uuid}") && type(apiKey.debug_model) == string && apiKey.debug_model == ${json.encode(service)} && apiKey.expiry != null && apiKey.budget != null && (request.method != "POST" || !(request.path in ["/v1/chat/completions", "/v1/completions", "/v1/responses", "/v1/embeddings", "/v1/messages", "/v1/messages/count_tokens"]) || json(request.body).model == apiKey.debug_model))',
        'request.headers.all(h, (!h.startsWith("x-thalamus-") && !h.startsWith("x-llm-d-inference-")) || h in ${_knownHeaders})',
    ] + (['apiKey.kind == "job" && apiKey.deployment == ${json.encode(candidateDeployment)}'] if candidateDeployment != "" else []) + (['apiKey.kind == "job" && apiKey.run_kind == "benchmark" && type(apiKey.request_budget) == int && apiKey.request_budget > 0 && apiKey.request_budget <= 1000000'] if benchmark else ['apiKey.kind != "job" || apiKey.run_kind == "validation"']) + (['type(apiKey.class) == string && apiKey.class in ${_classNames}'] if len(trafficClasses) > 0 else []) + extraRequirements

    _remove = ["x-api-key", "connection", "x-thalamus-backend"] + (["x-thalamus-user", "x-thalamus-project", "x-thalamus-team", "x-thalamus-class", "x-llm-d-inference-objective", "x-llm-d-inference-fairness-id"] if forwardBearer else (["x-thalamus-class", "x-llm-d-inference-objective", "x-llm-d-inference-fairness-id"] if len(trafficClasses) == 0 else []))
    _set = ([{name: "authorization", value: '"Bearer " + apiKey.key.unredacted()'}] if forwardBearer else [
        {name: "x-thalamus-user", value: "apiKey.user"},
        {name: "x-thalamus-project", value: "apiKey.project"},
        {name: "x-thalamus-team", value: "apiKey.team"},
    ] + ([
        {name: "x-thalamus-class", value: "apiKey.class"},
        {name: "x-llm-d-inference-objective", value: '${json.encode(service + "-")} + apiKey.class'},
        {name: "x-llm-d-inference-fairness-id", value: 'apiKey.project == null ? apiKey.team : apiKey.project'},
    ] if len(trafficClasses) > 0 else []))

    item: any = {
        apiVersion: "agentgateway.dev/v1alpha1"
        kind: "AgentgatewayPolicy"
        metadata: {name: "${route}-auth", namespace: namespace}
        spec: {
            targetRefs: [{group: "gateway.networking.k8s.io", kind: "HTTPRoute", name: route}]
            traffic: {
                apiKeyAuthentication: {mode: "Strict", configMapSelector: {matchLabels: keySelector}}
                authorization: {action: "Require", policy: {matchExpressions: _requirements}}
                transformation: {request: {remove: _remove, set: _set, metadata: {thalamus_service: json.encode(service)}}}
                if applyRateLimit:
                    rateLimit: {"global": {
                        backendRef: {name: rateLimit.name, namespace: rateLimit.namespace, port: rateLimit.port}
                        domain: rateLimit.domain
                        failureMode: "FailClosed"
                        descriptors: ([{
                            entries: [{name: "benchmark_key", expression: "apiKey.key_id"}]
                            unit: "Requests"
                            limitOverride: '{"unit":${_unit},"requestsPerUnit":apiKey.request_budget}'
                        }] if benchmark else [
                            {entries: [{name: "key_id", expression: "apiKey.key_id"}], unit: ("Requests" if requestRateLimit else "Tokens"), limitOverride: '{"unit":${_unit},"requestsPerUnit":${_keyLimit}}'},
                            # v1.7 skips a descriptor with a non-string entry.
                            # Only projectless personal keys have null project;
                            # key and team descriptors still apply to them.
                            {entries: [{name: "project", expression: "apiKey.project"}], unit: ("Requests" if requestRateLimit else "Tokens"), limitOverride: '{"unit":${_unit},"requestsPerUnit":${_projectLimit}}'},
                            {entries: [{name: "team", expression: "apiKey.team"}], unit: ("Requests" if requestRateLimit else "Tokens"), limitOverride: '{"unit":${_unit},"requestsPerUnit":${_teamCap}}'},
                        ])
                    }}
            }
        }
    }
    check:
        len(namespace) <= 63 and regex.match(namespace, _dns) and len(service) <= 63 and regex.match(service, _dns), "service and namespace must be DNS labels"
        len(route) <= 58 and regex.match(route, _dns) and not regex.match(route, "^thalamus-"), "route must fit a non-reserved policy name"
        version == "v1.7.0", "agentgateway v1.7.0 is required"
        "thalamus.dev/gateway-keys" in keySelector and keySelector["thalamus.dev/gateway-keys"] == "true", "key selector must include the worker projection label"
        candidateDeployment == "" or (len(candidateDeployment) <= 53 and regex.match(candidateDeployment, _dns) and route == ("${candidateDeployment}-benchmark" if benchmark else "${candidateDeployment}-candidate")), "candidate policy must target its deployment candidate route"
        not benchmark or candidateDeployment != "" or route == "${service}-benchmark", "public benchmark policy must target its service benchmark route"
        not benchmark or (applyRateLimit and not forwardBearer), "benchmark routes require independent request limiting and may not forward credentials"
        not requestRateLimit or (applyRateLimit and not forwardBearer and not benchmark), "request-budget routes require rate limiting, cannot forward credentials, and are not benchmark routes"
        len({c: True for c in trafficClasses}) == len(trafficClasses) and (all c in trafficClasses {len(service) + 1 + len(c) <= 63 and regex.match(c, _dns)}), "traffic classes must be distinct DNS labels fitting the service-scoped objective"
        applyRateLimit or forwardBearer, "only a credential-forwarding agentic hop may defer token rate limits"
        not forwardBearer or candidateDeployment == "", "candidate credentials may not be forwarded"
        all expression in extraRequirements {expression != ""}, "extra requirements must be nonempty CEL"
        len(_requirements) <= 256 and (all expression in _requirements {len(expression) <= 16384}), "authorization exceeds the v1.7 policy expression limits"
        len(_keyLimit) + len(_unit) + 64 <= 16384 and len(_projectLimit) + len(_unit) + 64 <= 16384 and len(_teamCap) + len(_unit) + 64 <= 16384, "rate limit CEL exceeds the v1.7 expression limit"
