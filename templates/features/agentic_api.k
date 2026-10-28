# Opt-in, Expected-only agentic-api composition. Operators must validate the exact
# image/commit, gateway version, CNI, secrets, database isolation and each enabled
# operation before publishing. Kubernetes List application is not atomic: install
# and verify policies and network isolation before attaching routes.
# Upstream: https://github.com/vllm-project/agentic-api/tree/27e60cf006cb771cf1961ea9909e64d812f5f3c8
# https://agentgateway.dev/docs/kubernetes/latest/reference/api/
# Only HTTP POST Responses with string input and no tools or compaction is exposed.
# Team state is shared by all users of one team, never individual-user isolation.
import json
import regex

schema Instance:
    name: str
    team: str = ""
    hostname: str
    databaseSecret: str
    keySelector: {str:str}
    check:
        name != "" and hostname != "" and databaseSecret != "", "instance identity, hostname and database Secret are required"
        len(keySelector) > 0, "public key selector is required"

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
    internalHostname: str
    internalBackend: str
    serviceKeySecret: str
    gatewayNamespaceLabels: {str:str}
    gatewayPodLabels: {str:str}
    instances: [Instance]

    # Credentials: serviceKeySecret has a single Authorization key containing the
    # raw bearer token; databaseSecret contains DATABASE_URL. Operator provisions
    # genuinely separate PostgreSQL databases and credentials per team. Secret names
    # alone do not prove separation. Restrict secret access and enable TLS in transit.
    items: [any] = [{
        apiVersion: "agentgateway.dev/v1alpha1"
        kind: "AgentgatewayPolicy"
        metadata: {name: "${name}-internal", namespace: namespace}
        spec: {
            targetRefs: [{group: "gateway.networking.k8s.io", kind: "HTTPRoute", name: "${name}-internal"}]
            traffic: {
                apiKeyAuthentication: {mode: "Strict", secretRef: {name: serviceKeySecret}}
                authorization: {action: "Require", policy: {matchExpressions: [
                    'request.headers.join()["x-thalamus-user"] != "" && request.headers.join()["x-thalamus-project"] != "" && request.headers.join()["x-thalamus-team"] != "" && request.headers.join()["x-thalamus-class"] != ""'
                ]}}
            }
        }
    }, {
        # The internal Gateway is dedicated to this recipe. Token usage comes
        # from the internal Responses backend; the public static backend emits
        # separate HTTP request metrics, not a second LLM token measurement.
        apiVersion: "agentgateway.dev/v1alpha1"
        kind: "AgentgatewayPolicy"
        metadata: {name: "${name}-agentic-metrics", namespace: internalGatewayNamespace}
        spec: {
            targetRefs: [{group: "gateway.networking.k8s.io", kind: "Gateway", name: internalGateway}]
            frontend: {metrics: {attributes: {add: [
                {name: "thalamus_team", expression: 'default(request.headers["x-thalamus-team"], "")'},
                {name: "thalamus_project", expression: 'default(request.headers["x-thalamus-project"], "")'},
                {name: "thalamus_request_kind", expression: '"agentic_internal"'},
            ]}}}
        }
    }, {
        apiVersion: "gateway.networking.k8s.io/v1"
        kind: "HTTPRoute"
        metadata: {name: "${name}-internal", namespace: namespace}
        spec: {
            parentRefs: [{name: internalGateway, namespace: internalGatewayNamespace}]
            hostnames: [internalHostname]
            rules: [{matches: [{method: "POST", path: {type: "Exact", value: "/v1/responses"}}], backendRefs: [{group: "agentgateway.dev", kind: "AgentgatewayBackend", name: internalBackend}]}]
        }
    }] + [{
        apiVersion: "agentgateway.dev/v1alpha1"
        kind: "AgentgatewayPolicy"
        metadata: {name: "${name}-${i.name}-public", namespace: namespace}
        spec: {
            targetRefs: [{group: "gateway.networking.k8s.io", kind: "HTTPRoute", name: "${name}-${i.name}"}]
            traffic: {
                apiKeyAuthentication: {mode: "Strict", configMapSelector: {matchLabels: i.keySelector}}
                authorization: {action: "Require", policy: {matchExpressions: [
                    'type(apiKey.user) == string && apiKey.user != "" && type(apiKey.project) == string && apiKey.project != "" && type(apiKey.team) == string && apiKey.team != "" && type(apiKey.class) == string && apiKey.class != ""' + (' && apiKey.team == ${json.encode(i.team)}' if mode == "team-shared" else ""),
                    'json(request.body).with(b, type(b) == map && b.keys().all(k, k in ["model", "input", "stream", "store"' + (', "previous_response_id"' if mode == "team-shared" else '') + ']) && type(b.model) == string && type(b.input) == string' + (' && b.store == false' if mode == "shared-stateless" else '') + ')'
                ]}}
                transformation: {request: {
                    remove: ["x-api-key", "connection"]
                    set: [
                        {name: "x-thalamus-user", value: "apiKey.user"},
                        {name: "x-thalamus-project", value: "apiKey.project"},
                        {name: "x-thalamus-team", value: "apiKey.team"},
                        {name: "x-thalamus-class", value: "apiKey.class"},
                    ]
                }}
            }
        }
    } for i in instances] + [{
        apiVersion: "agentgateway.dev/v1alpha1"
        kind: "AgentgatewayBackend"
        metadata: {name: "${name}-${i.name}", namespace: namespace}
        spec: {
            static: {host: "${name}-${i.name}.${namespace}.svc.cluster.local", port: 9000}
            policies: {auth: {secretRef: {name: serviceKeySecret}}}
        }
    } for i in instances] + [{
        apiVersion: "gateway.networking.k8s.io/v1"
        kind: "HTTPRoute"
        metadata: {name: "${name}-${i.name}", namespace: namespace}
        spec: {
            parentRefs: [{name: publicGateway, namespace: publicGatewayNamespace}]
            hostnames: [i.hostname]
            rules: [{matches: [{method: "POST", path: {type: "Exact", value: "/v1/responses"}}], backendRefs: [{group: "agentgateway.dev", kind: "AgentgatewayBackend", name: "${name}-${i.name}"}]}]
        }
    } for i in instances] + [{
        apiVersion: "apps/v1"
        kind: "Deployment"
        metadata: {name: "${name}-${i.name}", namespace: namespace}
        spec: {
            replicas: 1
            selector: {matchLabels: {"thalamus.dev/agentic-instance": "${name}-${i.name}"}}
            template: {
                metadata: {labels: {"thalamus.dev/agentic-instance": "${name}-${i.name}"}}
                spec: {
                    automountServiceAccountToken: False
                    containers: [{
                        name: "agentic-api"
                        image: image
                        ports: [{containerPort: 9000}]
                        env: [
                            {name: "LLM_API_BASE", value: "http://${internalHostname}"},
                            {name: "DATABASE_URL", valueFrom: {secretKeyRef: {name: i.databaseSecret, key: "DATABASE_URL"}}},
                            {name: "OPENAI_API_KEY", valueFrom: {secretKeyRef: {name: serviceKeySecret, key: "Authorization"}}},
                            {name: "SKIP_LLM_READY_CHECK", value: "true"},
                        ]
                        readinessProbe: {httpGet: {path: "/ready", port: 9000}}
                        livenessProbe: {httpGet: {path: "/health", port: 9000}}
                    }]
                }
            }
        }
    } for i in instances] + [{
        apiVersion: "v1"
        kind: "Service"
        metadata: {name: "${name}-${i.name}", namespace: namespace}
        spec: {selector: {"thalamus.dev/agentic-instance": "${name}-${i.name}"}, ports: [{port: 9000, targetPort: 9000}]}
    } for i in instances] + [{
        apiVersion: "networking.k8s.io/v1"
        kind: "NetworkPolicy"
        metadata: {name: "${name}-${i.name}", namespace: namespace}
        spec: {
            podSelector: {matchLabels: {"thalamus.dev/agentic-instance": "${name}-${i.name}"}}
            policyTypes: ["Ingress"]
            ingress: [{from: [{namespaceSelector: {matchLabels: gatewayNamespaceLabels}, podSelector: {matchLabels: gatewayPodLabels}}], ports: [{protocol: "TCP", port: 9000}]}]
        }
    } for i in instances]
    check:
        name != "" and namespace != "", "recipe identity is required"
        mode in ["shared-stateless", "team-shared"], "unsupported agentic mode"
        gatewayVersion == "v1.7.0", "agentgateway v1.7.0 is required"
        upstreamCommit == "27e60cf006cb771cf1961ea9909e64d812f5f3c8", "unsupported agentic-api source revision"
        len(image.split("@sha256:")) == 2 and len(image.split("@sha256:")[1]) == 64 and regex.match(image.split("@sha256:")[1], "^[0-9a-f]{64}$"), "immutable image digest is required"
        publicGateway != "" and publicGatewayNamespace != "" and internalGateway != "" and internalGatewayNamespace != "", "dedicated gateways are required"
        publicGateway != internalGateway or publicGatewayNamespace != internalGatewayNamespace, "internal and public gateways must be distinct"
        internalGatewayService != "" and internalHostname != "" and internalBackend != "" and serviceKeySecret != "", "internal route and service key are required"
        internalHostname == "${internalGatewayService}.${internalGatewayNamespace}.svc.cluster.local", "internal hostname must resolve to the dedicated gateway Service"
        len(gatewayNamespaceLabels) > 0 and len(gatewayPodLabels) > 0, "gateway ingress selectors are required"
        len(instances) > 0, "at least one instance is required"
        mode != "shared-stateless" or len(instances) == 1, "shared stateless requires exactly one instance"
        all i in instances {i.hostname != internalHostname}, "internal hostname must differ from every public hostname"
        len({i.name: True for i in instances}) == len(instances), "instance names must be unique"
        len({i.hostname: True for i in instances}) == len(instances), "instance hostnames must be unique"
        len({i.databaseSecret: True for i in instances}) == len(instances), "each instance requires a separate database Secret"
        mode != "team-shared" or ((all i in instances {i.team != ""}) and len({i.team: True for i in instances}) == len(instances)), "team instances require distinct team identities"
