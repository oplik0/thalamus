# Administrator-owned, single-primary Redis for the Envoy rate limiter.
# Supply a digest-pinned Redis image with TLS, redis-cli, /bin/sh and coreutils;
# the upstream agentgateway v1.7 e2e uses Redis 7.4.3. The auth Secret's `auth`
# key must be a 32+ character base64url/hex password (no username or colon).
# The same Secret also supplies RLS `url`; its host must match the TLS certificate
# SAN. The server TLS Secret contains tls.crt/tls.key; use a distinct CA Secret
# for the RLS so it never mounts the private key. Never render Secret contents.
# A single retained PVC is not HA. Protect/backup its durable volume: losing it
# resets rate-limit counters. AOF fsyncs every write; Redis evicts no counters.
# Secret rotation requires restarting Redis and RLS after updating both Secrets.
# NetworkPolicy assumes the trusted limiter selector and an enforcing CNI.
import json
import regex

schema Redis:
    namespace: str
    name: str = "example-redis"
    image: str
    authSecret: str
    tlsSecret: str
    storageClass: str
    storageCapacity: str = "10Gi"
    port: int = 6379
    limiterSelector: {str:str}
    adminUsers: [str] = []
    adminGroups: [str] = ["system:masters"]

    _labels = {"app.kubernetes.io/name": name, "app.kubernetes.io/component": "global-rate-limit-store"}
    _init_script = '''
        set -eu
        case "$REDIS_PASSWORD" in ''|*[!A-Za-z0-9_-]*) echo 'Redis auth must be base64url/hex' >&2; exit 1;; esac
        chars="$(printf %s "$REDIS_PASSWORD" | wc -c)"
        [ "$chars" -ge 32 ] || { echo 'Redis auth must be at least 32 characters' >&2; exit 1; }
        umask 077
        printf '%s\n' 'bind 0.0.0.0 ::' 'protected-mode yes' 'port 0' 'tls-port ${port}' 'tls-cert-file /tls/tls.crt' 'tls-key-file /tls/tls.key' 'tls-auth-clients no' 'unixsocket /tmp/redis.sock' 'unixsocketperm 700' 'dir /data' 'appendonly yes' 'appendfsync always' 'maxmemory 512mb' 'maxmemory-policy noeviction' 'save ""' > /runtime/redis.conf
        printf 'requirepass %s\n' "$REDIS_PASSWORD" >> /runtime/redis.conf
    '''
    _container_security = {allowPrivilegeEscalation: False, readOnlyRootFilesystem: True, capabilities: {drop: ["ALL"]}}
    _auth_env = {name: "REDIS_PASSWORD", valueFrom: {secretKeyRef: {name: authSecret, key: "auth"}}}
    _health_env = {name: "REDISCLI_AUTH", valueFrom: {secretKeyRef: {name: authSecret, key: "auth"}}}
    _probe = {exec: {command: ["redis-cli", "-e", "-s", "/tmp/redis.sock", "ping"]}, periodSeconds: 5, timeoutSeconds: 2, failureThreshold: 12}

    items: [any] = [{
        # The ordinary catalog writer can manage Services and NetworkPolicies,
        # so reserve this administrator-owned Redis name in those families.
        apiVersion: "admissionregistration.k8s.io/v1"
        kind: "ValidatingAdmissionPolicy"
        metadata: {name: "admin-redis-ownership.thalamus.dev"}
        spec: {
            failurePolicy: "Fail"
            matchConstraints: {
                matchPolicy: "Equivalent"
                namespaceSelector: {matchLabels: {"thalamus.dev/live-admission": "enforced"}}
                resourceRules: [
                    {apiGroups: [""], apiVersions: ["v1"], operations: ["CREATE", "UPDATE", "DELETE"], resources: ["services"], scope: "Namespaced"},
                    {apiGroups: ["apps"], apiVersions: ["v1"], operations: ["CREATE", "UPDATE", "DELETE"], resources: ["statefulsets"], scope: "Namespaced"},
                    {apiGroups: ["networking.k8s.io"], apiVersions: ["v1"], operations: ["CREATE", "UPDATE", "DELETE"], resources: ["networkpolicies"], scope: "Namespaced"},
                ]
            }
            validations: [{
                expression: 'request.namespace != ' + json.encode(namespace) + ' || request.name != ' + json.encode(name) + ' || request.userInfo.username in ' + json.encode(adminUsers) + ' || (has(request.userInfo.groups) && request.userInfo.groups.exists(g, g in ' + json.encode(adminGroups) + '))'
                message: "the Redis Service, StatefulSet and NetworkPolicy are administrator-owned"
            }]
        }
    }, {
        apiVersion: "admissionregistration.k8s.io/v1"
        kind: "ValidatingAdmissionPolicyBinding"
        metadata: {name: "admin-redis-ownership.thalamus.dev"}
        spec: {policyName: "admin-redis-ownership.thalamus.dev", validationActions: ["Deny"]}
    }, {
        apiVersion: "v1"
        kind: "Service"
        metadata: {name: name, namespace: namespace}
        spec: {
            clusterIP: "None"
            selector: _labels
            ports: [{name: "redis-tls", "port": port, targetPort: port, protocol: "TCP"}]
        }
    }, {
        apiVersion: "apps/v1"
        kind: "StatefulSet"
        metadata: {name: name, namespace: namespace}
        spec: {
            serviceName: name
            replicas: 1
            selector: {matchLabels: _labels}
            persistentVolumeClaimRetentionPolicy: {whenDeleted: "Retain", whenScaled: "Retain"}
            template: {
                metadata: {labels: _labels}
                spec: {
                    automountServiceAccountToken: False
                    securityContext: {runAsNonRoot: True, runAsUser: 10001, runAsGroup: 10001, fsGroup: 10001, seccompProfile: {type: "RuntimeDefault"}}
                    initContainers: [{
                        name: "configure"
                        image: image
                        command: ["/bin/sh", "-ec", _init_script]
                        env: [_auth_env]
                        securityContext: _container_security
                        volumeMounts: [{name: "runtime", mountPath: "/runtime"}]
                    }]
                    containers: [{
                        name: "redis"
                        image: image
                        command: ["redis-server", "/runtime/redis.conf"]
                        env: [_health_env]
                        ports: [{name: "redis-tls", containerPort: port, protocol: "TCP"}]
                        startupProbe: _probe
                        readinessProbe: _probe
                        livenessProbe: _probe
                        securityContext: _container_security
                        resources: {requests: {cpu: "100m", memory: "256Mi"}, limits: {cpu: "2", memory: "1Gi"}}
                        volumeMounts: [
                            {name: "runtime", mountPath: "/runtime", readOnly: True},
                            {name: "tls", mountPath: "/tls", readOnly: True},
                            {name: "data", mountPath: "/data"},
                            {name: "tmp", mountPath: "/tmp"},
                        ]
                    }]
                    volumes: [
                        {name: "runtime", emptyDir: {}},
                        {name: "tmp", emptyDir: {}},
                        {name: "tls", secret: {secretName: tlsSecret, items: [{key: "tls.crt", path: "tls.crt"}, {key: "tls.key", path: "tls.key"}]}},
                    ]
                }
            }
            volumeClaimTemplates: [{
                metadata: {name: "data"}
                spec: {accessModes: ["ReadWriteOnce"], storageClassName: storageClass, resources: {requests: {storage: storageCapacity}}}
            }]
        }
    }, {
        apiVersion: "networking.k8s.io/v1"
        kind: "NetworkPolicy"
        metadata: {name: name, namespace: namespace}
        spec: {
            podSelector: {matchLabels: _labels}
            policyTypes: ["Ingress", "Egress"]
            ingress: [{from: [{podSelector: {matchLabels: limiterSelector}}], ports: [{protocol: "TCP", "port": port}]}]
        }
    }]

    check:
        len(namespace) <= 63 and regex.match(namespace, "^[a-z0-9]([-a-z0-9]*[a-z0-9])?$"), "Redis namespace must be a DNS label"
        len(name) <= 63 and regex.match(name, "^[a-z0-9]([-a-z0-9]*[a-z0-9])?$"), "Redis name must be a DNS label"
        regex.match(image, "^.+@sha256:[0-9a-f]{64}$"), "Redis image must be digest pinned"
        authSecret != "" and tlsSecret != "" and storageClass != "", "Redis auth/TLS Secrets and storage class are required"
        regex.match(storageCapacity, "^[1-9][0-9]*(Mi|Gi|Ti)$") and port > 0 and port <= 65535, "Redis storage and port must be valid"
        len(limiterSelector) > 0, "Redis requires an explicit limiter Pod selector"
