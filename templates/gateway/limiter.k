# Deploy the upstream Envoy global RLS and its one-domain config. Redis may be
# platform-installed or external; its TLS/auth Secrets are operator-provisioned.
# NetworkPolicy requires an enforcing CNI and protected gateway/pod labels;
# other policies are additive. Node-local DNS needs separate operator egress.
# Source contract: envoyproxy/ratelimit 3e085e5b and agentgateway v1.7.0 e2e.
import crypto
import regex

limiter_resources = lambda profile: LimitProfile, rateLimit: RateLimit -> [any] {
    assert profile.deployment != Undefined, "a managed rate limiter requires deployment settings"
    assert profile.service == rateLimit.name and profile.namespace == rateLimit.namespace and profile.port == rateLimit.port and profile.domain == rateLimit.domain, "resolved rate limit must match its catalog profile"
    assert not regex.match(profile.service, "^thalamus-"), "static rate limiter resources must not use the worker-reserved prefix"
    assert profile.port != 8080, "RLS gRPC port must differ from its HTTP health port 8080"
    deployment = profile.deployment
    config = limiter_config(profile.namespace, rateLimit)
    labels = {"app.kubernetes.io/name": profile.service, "app.kubernetes.io/component": "global-rate-limiter"}
    [config, {
        apiVersion: "apps/v1"
        kind: "Deployment"
        metadata: {name: profile.service, namespace: profile.namespace}
        spec: {
            replicas: 2
            selector: {matchLabels: labels}
            template: {
                metadata: {labels: labels, annotations: {"checksum/config": crypto.sha256(config.data["config.yaml"])}}
                spec: {
                    automountServiceAccountToken: False
                    securityContext: {runAsNonRoot: True, runAsUser: 10001, runAsGroup: 10001, seccompProfile: {type: "RuntimeDefault"}}
                    containers: [{
                        name: "ratelimit"
                        image: deployment.image
                        # The pinned upstream image does not set an entrypoint.
                        command: ["/bin/ratelimit"]
                        ports: [{name: "grpc", containerPort: profile.port}, {name: "health", containerPort: 8080}]
                        env: [
                            {name: "RUNTIME_ROOT", value: "/data"},
                            {name: "RUNTIME_SUBDIRECTORY", value: "ratelimit"},
                            {name: "RUNTIME_IGNOREDOTFILES", value: "true"},
                            {name: "RUNTIME_WATCH_ROOT", value: "false"},
                            {name: "USE_STATSD", value: "false"},
                            {name: "GRPC_PORT", value: "${profile.port}"},
                            {name: "DEBUG_HOST", value: "127.0.0.1"},
                            {name: "DEBUG_PORT", value: "0"},
                            {name: "REDIS_SOCKET_TYPE", value: "tcp"},
                            {name: "REDIS_URL", valueFrom: {secretKeyRef: {name: deployment.redisSecret, key: "url"}}},
                            {name: "REDIS_AUTH", valueFrom: {secretKeyRef: {name: deployment.redisSecret, key: "auth"}}},
                            {name: "REDIS_TLS", value: "true"},
                            {name: "HEALTHY_WITH_AT_LEAST_ONE_CONFIG_LOADED", value: "true"},
                            {name: "REDIS_HEALTH_CHECK_ACTIVE_CONNECTION", value: "true"},
                        ] + ([{name: "REDIS_TLS_CACERT", value: "/etc/redis-ca/ca.crt"}] if deployment.redisCASecret != Undefined else [])
                        readinessProbe: {httpGet: {path: "/healthcheck", port: 8080}, periodSeconds: 5}
                        securityContext: {allowPrivilegeEscalation: False, readOnlyRootFilesystem: True, capabilities: {drop: ["ALL"]}}
                        volumeMounts: [{name: "config", mountPath: "/data/ratelimit/config", readOnly: True}] + ([{name: "redis-ca", mountPath: "/etc/redis-ca", readOnly: True}] if deployment.redisCASecret != Undefined else [])
                    }]
                    volumes: [{name: "config", configMap: {name: config.metadata.name}}] + ([{name: "redis-ca", secret: {secretName: deployment.redisCASecret, items: [{key: "ca.crt", path: "ca.crt"}]}}] if deployment.redisCASecret != Undefined else [])
                }
            }
        }
    }, {
        apiVersion: "v1"
        kind: "Service"
        metadata: {name: profile.service, namespace: profile.namespace}
        spec: {
            selector: labels
            ports: [{name: "grpc", port: profile.port, targetPort: profile.port, protocol: "TCP", appProtocol: "kubernetes.io/h2c"}]
        }
    }, {
        apiVersion: "networking.k8s.io/v1"
        kind: "NetworkPolicy"
        metadata: {name: profile.service, namespace: profile.namespace}
        spec: {
            podSelector: {matchLabels: labels}
            policyTypes: ["Ingress", "Egress"]
            ingress: [{
                from: [{namespaceSelector: {matchLabels: {"kubernetes.io/metadata.name": deployment.gatewayNamespace}}, podSelector: {matchLabels: deployment.gatewayPodSelector}}] + ([{podSelector: {matchLabels: deployment.agenticGatewayPodSelector}}] if deployment.agenticGatewayPodSelector != Undefined else [])
                ports: [{protocol: "TCP", port: profile.port}]
            }]
            egress: [{
                to: [{namespaceSelector: {matchLabels: {"kubernetes.io/metadata.name": "kube-system"}}, podSelector: {matchLabels: {"k8s-app": "kube-dns"}}}]
                ports: [{protocol: "UDP", port: 53}, {protocol: "TCP", port: 53}]
            }, {
                to: ([{podSelector: {matchLabels: deployment.redisPodSelector}}] if deployment.redisPodSelector != Undefined else []) + [{ipBlock: {cidr: redisCidr}} for redisCidr in deployment.redisCidrs]
                ports: [{protocol: "TCP", port: deployment.redisPort}]
            }]
        }
    }]
}
