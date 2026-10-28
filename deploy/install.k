# Render from the repository root with KCL 0.13.2:
#   kcl deploy/install.k -D config_file=operator-install.yaml
# Relative config_file paths resolve from deploy/; absolute paths also work.
# Keep the operator's configuration untracked and free of credential values.
# This is a fresh-cluster installer, not an operator or a deployment attestation.
# Install the pinned upstream controllers and CRDs first; provision the external
# PostgreSQL database, S3 bucket, OIDC issuer, DNS/TLS, registry and Git repository.
# Namespace-scoped Secrets must exist before their Pods start. See the schema below
# for required inputs; credentials are referenced, never supplied to this renderer.
# Install namespaces and Secrets, then admission/network and bootstrap resources.
# Wait for the bootstrap Job to succeed, then let the server apply its migrations.
# The worker's init container waits for server /health, which follows migrations.
# The Job has a finite retry limit; if it fails, fix the cause and use a new
# bootstrapRevision. Rotate database credentials by updating Secrets, running a
# new revision of that Job, and restarting both Deployments.
# Provision the workload Redis auth Secret (`url` and `auth`), server TLS Secret
# (`tls.crt` and `tls.key`) and separate RLS CA Secret (`ca.crt`). The URL host
# must be the Redis Service DNS name and match the TLS certificate SAN. The auth
# password must be at least 32 base64url/hex characters. The operator-provided
# storage class must create a durable RWO volume accessible to UID/GID 10001.
# Wait for Redis and the RLS in rendered/inference to become ready and verify the
# loaded RLS domain before exposing traffic. Back up the Redis PVC; losing the
# single Redis volume resets budget counters. Reconcile rendered/pools through
# the separate administrator-owned pool GitOps path before pooled services.
# PostgreSQL must be direct or session-affine (not transaction/statement pooling):
# leadership and debug collectors use session advisory locks. Size connections
# for the server/worker pools and a dedicated connection per active collector.
# NetworkPolicies require an enforcing CNI and operator-supplied egress rules. The
# systemEgress rules MUST cover PostgreSQL, Git, Kubernetes API, OIDC,
# Prometheus, S3, inference gateway, and optional configured endpoints.
# runnerEgress MUST cover the inference gateway and S3 upload endpoints.
# Gateway proxy egress must include the controller's xDS endpoint; serving proxy
# egress must also cover RLS, endpoint pickers and model backends. Supply
# per-service policies in the inference namespace: an empty set deliberately
# denies serving traffic. Policy selectors and pod ports must match the selected
# Gateway controller and CNI, including any Service DNAT behavior.
# The optional agentic-internal Gateway terminates HTTPS on a private ClusterIP;
# its namespace-local TLS Secret needs a SAN for the generated Service FQDN.
# Agentic pods must trust that CA, and the public-Gateway-to-agentic hop requires
# a separately verified encrypted cluster transport: the pinned app speaks HTTP.
import file
import json
import regex
import yaml
import templates.gateway.policy as gateway_policy
import templates.platform.database as database
import templates.platform.flux_mcp as flux_mcp
import templates.platform.gitops as gitops
import templates.platform.pool_gitops as pool_gitops
import templates.platform.rbac as rbac
import templates.platform.redis as redis
import templates.platform.security as security

# Secret keys in thalamus-system:
# database: url, username, password (URL, user and password must identify the
#   same dedicated PostgreSQL database and application role).
# databaseAdmin: url (existing database; an administrator with CREATEROLE and
#   database/schema ownership privileges).
# oidc: issuer, client-id, client-secret; signing: session-signing-key (>=32 bytes).
# s3: access-key-id, secret-access-key. Optional gitAuth: username, password
# (a read-only Git credential); optional databaseCA, oidcCA and s3CA: ca.crt (PEM);
# optional workflows: config (shared server/worker JSON, may contain forge credentials);
# optional keycloakAdmin: client-id, client-secret (read-only realm user access).
# Extra environment credentials must use Secret refs. The control TLS Secret is
# in gateway.namespace; the serving TLS Secret is in workloadNamespace.
schema Secrets:
    database: str
    databaseAdmin: str
    oidc: str
    signing: str
    s3: str
    gitAuth: str = ""
    oidcCA: str = ""
    databaseCA: str = ""
    s3CA: str = ""
    keycloakAdmin: str = ""
    workflows: str = ""
    check:
        database != "" and databaseAdmin != "" and oidc != "" and signing != "" and s3 != "", "all required Secret names must be provided"

schema Images:
    # Use an image built from this repository's runtime Dockerfile, containing
    # thalamus-server, thalamus-worker, curl, git and CA certificates.
    thalamus: str
    # A PostgreSQL client image containing /bin/sh and psql 15 or newer.
    postgresClient: str
    check:
        regex.match(thalamus, "^.+@sha256:[0-9a-f]{64}$"), "thalamus image must be SHA-256 digest pinned"
        regex.match(postgresClient, "^.+@sha256:[0-9a-f]{64}$"), "PostgreSQL client image must be SHA-256 digest pinned"

schema Catalog:
    # HTTPS URL without credentials; auth, if needed, comes from gitAuth Secret.
    url: str
    # A reachable branch/tag containing commit; complete commit identity is
    # checked after fetching. For a moving branch, change commit and redeploy.
    ref: str
    commit: str
    path: str = "catalog"
    # Must also be gitops.service_schema_path in the workflows Secret.
    serviceSchemaPath: str = "catalog/service.schema.json"
    _path_parts = path.split("/")
    _schema_parts = serviceSchemaPath.split("/")
    check:
        regex.match(url, "^https://[A-Za-z0-9.-]+(:[0-9]+)?/[^[:space:]@]+$"), "catalog URL must be credential-free HTTPS"
        regex.match(ref, "^refs/(heads|tags)/[A-Za-z0-9._/-]+$"), "catalog ref must be a branch or tag ref"
        regex.match(commit, "^([0-9a-f]{40}|[0-9a-f]{64})$"), "catalog commit must be a full hexadecimal Git object ID"
        regex.match(path, "^[A-Za-z0-9._/-]+$") and (all part in _path_parts {part != "" and part != "." and part != ".."}), "catalog path must contain only normal relative components"
        regex.match(serviceSchemaPath, "^[A-Za-z0-9._/-]+$") and (all part in _schema_parts {part != "" and part != "." and part != ".."}), "service schema must contain only normal relative components"

schema Gateway:
    # If create is false, the named HTTPS Gateway/listener must already allow
    # HTTPRoutes from thalamus-system; otherwise this emits the Gateway as well.
    create: bool = True
    name: str
    namespace: str
    className: str
    hostname: str
    tlsSecret: str
    listener: str = "https"
    # Namespace and labels of the actual gateway data-plane Pods for ingress.
    proxyNamespace: str
    proxyLabels: {str:str}
    # Client peer selectors and proxy pod port are explicit operator inputs.
    # proxyEgress must allow the Gateway controller's xDS connection.
    proxyPort: int = 443
    proxyIngress: [any] = []
    # Optional Prometheus ingress peers for the proxy's metrics port.
    metricsPort: int = 15020
    metricsIngress: [any] = []
    proxyEgress: [any] = []
    check:
        name != "" and namespace != "" and className != "" and tlsSecret != "" and listener != "" and proxyNamespace != "", "gateway identity and TLS Secret are required"
        regex.match(hostname, "^[a-z0-9]([a-z0-9.-]*[a-z0-9])?$"), "a lowercase DNS hostname is required"
        len(proxyLabels) > 0 and proxyPort > 0 and proxyPort <= 65535 and metricsPort > 0 and metricsPort <= 65535, "gateway Pod selector and ports are required"

schema ServingGateway:
    # Administrator-owned data-plane Gateway, distinct from the API Gateway.
    # It always lives in the managed workload namespace, so Same permits only
    # that namespace's HTTPRoutes. Empty hostname accepts all route hostnames;
    # otherwise use a DNS name or a single-label wildcard covered by tlsSecret.
    name: str = "inference-serving"
    className: str
    hostname: str
    tlsSecret: str
    listener: str = "https"
    port: int = 443
    # The selected controller must label proxy Pods as configured here. The
    # target pod port may differ from the Gateway listener Service port.
    proxyLabels: {str:str}
    proxyPort: int = 443
    proxyIngress: [any]
    metricsPort: int = 15020
    metricsIngress: [any] = []
    proxyEgress: [any] = []
    check:
        len(name) <= 63 and regex.match(name, "^[a-z0-9]([-a-z0-9]*[a-z0-9])?$") and not name.startswith("thalamus-"), "serving Gateway must use an unreserved DNS label"
        className != "" and listener != "" and tlsSecret != "", "serving Gateway class, listener and TLS Secret are required"
        hostname == "" or regex.match(hostname, "^([*][.])?[a-z0-9]([a-z0-9.-]*[a-z0-9])?$"), "serving hostname must be empty, a DNS name, or a leading wildcard"
        port > 0 and port <= 65535 and proxyPort > 0 and proxyPort <= 65535 and metricsPort > 0 and metricsPort <= 65535, "serving Gateway ports must be valid"
        "gateway.networking.k8s.io/gateway-name" in proxyLabels and proxyLabels["gateway.networking.k8s.io/gateway-name"] == name and len(proxyIngress) > 0, "serving proxy selector must include the Gateway name and explicit client peers"

schema AgenticGateway:
    # An administrator-owned ClusterIP Gateway. The pinned controller creates
    # Service/ConfigMap names from the Gateway name, so it must not use the
    # worker-reserved thalamus- prefix. Its TLS Secret must cover the Service
    # FQDN; agentic pods independently mount a CA bundle that trusts that cert.
    enabled: bool = False
    name: str = "agentic-internal"
    className: str = ""
    tlsSecret: str = ""
    # Public PEM bundle: private Gateway trust plus any external public roots
    # needed by the app. Immutable ConfigMaps rotate under a new name.
    caConfigMap: str = "thalamus-agentic-ca"
    caBundle: str = ""
    proxyLabels: {str:str} = {}
    proxyPort: int = 443
    metricsPort: int = 15020
    metricsIngress: [any] = []
    proxyEgress: [any] = []
    check:
        not enabled or (len(name) <= 63 and regex.match(name, "^[a-z0-9]([-a-z0-9]*[a-z0-9])?$") and not name.startswith("thalamus-")), "internal Gateway must use an unreserved DNS label"
        not enabled or (className != "" and tlsSecret != "" and proxyPort > 0 and proxyPort <= 65535 and metricsPort > 0 and metricsPort <= 65535 and len(proxyEgress) > 0), "internal Gateway needs class, TLS Secret, Pod ports and explicit xDS/RLS egress"
        not enabled or ("gateway.networking.k8s.io/gateway-name" in proxyLabels and proxyLabels["gateway.networking.k8s.io/gateway-name"] == name), "internal Gateway proxy selector must include its Gateway name"
        not enabled or (len(caConfigMap) <= 63 and regex.match(caConfigMap, "^thalamus-[a-z0-9]([-a-z0-9]*[a-z0-9])?$") and "-----BEGIN CERTIFICATE-----" in caBundle and "-----END CERTIFICATE-----" in caBundle), "internal Gateway needs a reserved CA ConfigMap name and PEM bundle"

schema S3:
    endpoint: str
    bucket: str
    region: str
    runSecretPrefix: str = "thalamus-upload"
    check:
        regex.match(endpoint, "^https://[^/@[:space:]]+/?$"), "S3 endpoint must be a credential-free HTTPS origin"
        regex.match(bucket, "^[a-z0-9][a-z0-9-]{1,61}[a-z0-9]$") and regex.match(region, "^[a-z0-9]([a-z0-9-]*[a-z0-9])?$"), "S3 bucket and region must be DNS labels"
        regex.match(runSecretPrefix, "^thalamus-[a-z0-9-]+$") and len(runSecretPrefix) <= 26, "S3 run Secret prefix must be reserved and at most 26 characters"

schema Usage:
    prometheusUrl: str
    inputCounter: str
    outputCounter: str
    stepSeconds: int
    startDate: str
    retentionDays: int
    check:
        prometheusUrl != "" and inputCounter != "" and outputCounter != "", "usage Prometheus source and counters are required"
        stepSeconds > 0 and retentionDays > 0 and retentionDays <= 36500, "usage step and retention must be positive"
        regex.match(startDate, "^[0-9]{4}-[0-9]{2}-[0-9]{2}$"), "usage start date must be YYYY-MM-DD"

schema Network:
    # Native NetworkPolicy egress-rule objects, including `to` and `ports`.
    systemEgress: [any]
    runnerEgress: [any]
    # Additional COMPLETE NetworkPolicy resources for serving and other pods.
    # They must be in workloadNamespace and must not weaken debug isolation.
    workloadPolicies: [any] = []
    # Exact trusted installer/administrator usernames or groups. system:masters
    # remains an emergency bypass; cluster-admin RBAC alone is not a username.
    adminUsers: [str] = []
    adminGroups: [str] = ["system:masters"]
    check:
        len(systemEgress) > 0 and len(runnerEgress) > 0, "explicit system and runner egress rules are required"

schema GitOpsOptions:
    # The source and its optional credential are shared by both reconcilers,
    # even when only poolGitOps is enabled. It uses catalog.url and catalog.ref.
    enabled: bool = False
    sourceName: str = "catalog-source"
    reconcilerName: str = "catalog-sync"
    # Existing Flux HTTPS credential Secret in thalamus-system.
    secretName: str = ""
    extraRules: [any] = []

schema PoolGitOpsOptions:
    # Cluster-scoped resources require a separate administrator-controlled
    # reconciler; its input is rendered/pools in the shared catalog source.
    enabled: bool = False
    reconcilerName: str = "pool-sync"

schema RedisOptions:
    # Auth Secret: url and auth. TLS server Secret: tls.crt and tls.key.
    # RLS uses a distinct CA Secret with ca.crt in its reviewed limit profile.
    name: str = "example-redis"
    # Official redis:7.4.3 OCI index, resolved from Docker Hub; override only
    # with a reviewed TLS-capable image containing redis-server and redis-cli.
    image: str = "docker.io/library/redis@sha256:236e397c1d5ab7a94adaf1a51eec3ca8333b05fafcd6d423c6c7cc5987e519a0"
    authSecret: str
    tlsSecret: str
    storageClass: str
    storageCapacity: str = "10Gi"
    port: int = 6379
    limiterSelector: {str:str}

schema FluxMcpOptions:
    # Optional upstream Flux Operator MCP; Flux/its namespace must already exist.
    # An external, equivalently restricted provider may instead be used.
    enabled: bool = False
    namespace: str = "flux-system"
    imageRepository: str = "ghcr.io/controlplaneio-fluxcd/flux-operator-mcp"
    imageDigest: str = ""
    apiEgress: [any] = []
    check:
        not enabled or (regex.match(imageDigest, "^[0-9a-f]{64}$") and len(apiEgress) > 0), "Flux MCP needs a verified image digest and Kubernetes API egress"

schema MetricsGateway:
    # Admin-owned, shared inference Gateway; install its policy exactly once.
    namespace: str
    name: str

schema Install:
    workloadNamespace: str = "thalamus-inference"
    images: Images
    secrets: Secrets
    catalog: Catalog
    gateway: Gateway
    servingGateway: ServingGateway
    agenticGateway: AgenticGateway = AgenticGateway {}
    s3: S3
    usage: Usage
    network: Network
    redis: RedisOptions
    gitops: GitOpsOptions = GitOpsOptions {}
    poolGitOps: PoolGitOpsOptions = PoolGitOpsOptions {}
    fluxMcp: FluxMcpOptions = FluxMcpOptions {}
    metricsGateways: [MetricsGateway] = []
    prometheusUrl: str
    # Trusted operator runner definitions. Every runner is digest pinned and
    # restricted to the tokenless workload-namespace thalamus-runner SA.
    runners: {str:any}
    # Service/runner test options only: no candidate inventory, observed endpoint,
    # backend identity or future revision belongs in administrator configuration.
    validationConfigs: {str:any} = {}
    # Shared RuntimeConfig with actual installed API versions, ownership and
    # public route bindings. Worker derives candidate and benchmark routes live.
    runtimeConfig: {str:any}
    # Shared exact OIDC group mapping for server and optional Keycloak collector.
    groupMapping: {str:any} = {}
    # Debug authority and UI configuration are operator-reviewed together;
    # omitting both disables debug rather than inventing an enforcement claim.
    debugConfig: {str:any} = {}
    debugServerConfig: {str:any} = {}
    projectionName: str = "thalamus-keys"
    projectionLabels: {str:str} = {"thalamus.dev/gateway-keys": "true"}
    leaseName: str = "thalamus-worker-lease"
    bootstrapRevision: str
    serverReplicas: int = 1
    workerReplicas: int = 1
    # Optional Kubernetes env entries and volumes, e.g. OIDC resource-server
    # credentials, GitOps workflows and writable worktrees. Use secretKeyRef for
    # credentials; never overlay /workspace/repository or /tmp.
    serverExtraEnv: [any] = []
    serverExtraVolumes: [any] = []
    serverExtraVolumeMounts: [any] = []
    workerExtraEnv: [any] = []
    workerExtraVolumes: [any] = []
    workerExtraVolumeMounts: [any] = []
    # Additional namespaces containing referenced Gateways or Flux Kustomizations.
    observationNamespaces: [str] = []
    # Administrator-approved DB/search Secrets the observer may GET by name
    # only to prove immutability; never derive this list from catalog intent.
    agenticSecretNames: [str] = []
    check:
        regex.match(workloadNamespace, "^[a-z0-9]([-a-z0-9]*[a-z0-9])?$") and workloadNamespace != "thalamus-system", "workload namespace must be a distinct DNS label"
        prometheusUrl != "" and len(runners) > 0 and len(projectionLabels) > 0, "Prometheus, runners and projection labels are required"
        all r in runners {runners[r].service_account == "thalamus-runner" and runners[r].kind in ["validation", "benchmark"] and runners[r].route_target in ["candidate", "public"] and regex.match(runners[r].image, "^.+@sha256:[0-9a-f]{64}$") and len(runners[r].key_secret) <= 26 and regex.match(runners[r].key_secret, "^thalamus-[a-z0-9]([-a-z0-9]*[a-z0-9])?$")}, "runners must be digest pinned and use tokenless thalamus-runner, a route target and reserved key Secret prefix"
        all service in validationConfigs {all r in validationConfigs[service] {r in runners and validationConfigs[service][r].kind == runners[r].kind and validationConfigs[service][r].route_target == runners[r].route_target}}, "service runner options must match the trusted runner kind and route target"
        regex.match(projectionName, "^thalamus-[a-z0-9-]+$") and regex.match(leaseName, "^thalamus-[a-z0-9-]+$"), "projection and Lease must use reserved names"
        regex.match(bootstrapRevision, "^[a-z0-9]([-a-z0-9]*[a-z0-9])?$") and len(bootstrapRevision) <= 30, "bootstrapRevision must be a short DNS label"
        runtimeConfig["namespace"] == workloadNamespace, "runtime observation namespace must equal the worker workload namespace"
        all s in agenticSecretNames {not s.startswith("thalamus-") and s != redis.authSecret and s != redis.tlsSecret and s != servingGateway.tlsSecret and (not agenticGateway.enabled or s != agenticGateway.tlsSecret) and (gateway.namespace != workloadNamespace or s != gateway.tlsSecret)}, "observer dependency Secrets must not include reserved gateway keys, Redis credentials, or Gateway TLS Secrets"
        (len(debugConfig) == 0 and len(debugServerConfig) == 0) or (len(debugConfig) > 0 and len(debugServerConfig) > 0 and debugServerConfig["namespace"] == workloadNamespace and debugServerConfig["checkout_revision"] == catalog.commit and debugConfig["checkout_revision"] == catalog.commit and debugConfig["allowed_models"] == debugServerConfig["allowed_models"] and debugServerConfig["duration_seconds"] <= debugConfig["max_duration_seconds"] and debugServerConfig["model_budget"] <= debugConfig["max_model_tokens"] and debugServerConfig["tool_budget"] <= debugConfig["max_tool_calls"] and debugConfig["proxy_namespace"] == "thalamus-system" and debugConfig["proxy_pod_labels"] == {"app.kubernetes.io/name": "thalamus-server"}), "debug configurations must share workload namespace, checkout, allowed models and bounds"
        len(debugConfig) == 0 or (all service in debugConfig["model_policy_namespaces"] {debugConfig["model_policy_namespaces"][service] == workloadNamespace}), "debug model policies and projected keys must use the configured workload namespace"
        all p in network.workloadPolicies {p.metadata.namespace == workloadNamespace}, "serving network policies must target the workload namespace"
        gateway.namespace != workloadNamespace or gateway.name != servingGateway.name, "control and serving Gateways must be distinct resources"
        not agenticGateway.enabled or (agenticGateway.name != servingGateway.name and (gateway.namespace != workloadNamespace or gateway.name != agenticGateway.name)), "internal, public and control Gateways must be distinct"
        not agenticGateway.enabled or agenticGateway.caConfigMap != projectionName, "immutable agentic CA ConfigMap must differ from the mutable key projection"
        gateway.proxyNamespace != workloadNamespace or gateway.proxyLabels != servingGateway.proxyLabels, "control and serving proxy selectors must be distinct"
        gateway.proxyNamespace not in ["thalamus-system", workloadNamespace] or len(gateway.proxyIngress) > 0, "managed control proxy requires explicit client peers"
        len({n: True for n in observationNamespaces}) == len(observationNamespaces), "observation namespaces must be unique"
        len({"${g.namespace}/${g.name}": True for g in metricsGateways}) == len(metricsGateways), "each Gateway metrics policy must be emitted only once"
        not fluxMcp.enabled or len(debugServerConfig) == 0 or debugServerConfig["flux_mcp_url"] == "http://flux-operator-mcp.${fluxMcp.namespace}.svc:9090/mcp", "debug server Flux MCP URL must target the installed provider"
        serverReplicas > 0 and workerReplicas > 0, "replica counts must be positive"

_config_file = option("config_file", required=True)
_config: Install = yaml.decode(file.read(_config_file))
_system = "thalamus-system"
_git_env = [
    {name: "THALAMUS_GIT_URL", value: _config.catalog.url},
    {name: "THALAMUS_GIT_REF", value: _config.catalog.ref},
    {name: "THALAMUS_GIT_COMMIT", value: _config.catalog.commit},
    {name: "THALAMUS_CATALOG_PATH", value: _config.catalog.path},
    {name: "THALAMUS_SERVICE_SCHEMA_PATH", value: _config.catalog.serviceSchemaPath},
] + ([
    {name: "THALAMUS_GIT_USERNAME", valueFrom: {secretKeyRef: {name: _config.secrets.gitAuth, key: "username"}}},
    {name: "THALAMUS_GIT_PASSWORD", valueFrom: {secretKeyRef: {name: _config.secrets.gitAuth, key: "password"}}},
] if _config.secrets.gitAuth != "" else [])

# This init container owns the writeable emptyDir; applications see it read-only.
# An ordinary clone is intentional: the selected commit can be older than the
# advertised branch tip, and the worker needs .git to read tracked budget caps.
_checkout = {
    name: "catalog-checkout"
    image: _config.images.thalamus
    imagePullPolicy: "IfNotPresent"
    command: ["/bin/sh", "-ec"]
    args: ['''
        export HOME=/tmp GIT_TERMINAL_PROMPT=0
        cat > /tmp/thalamus-git-askpass <<'EOF'
#!/bin/sh
case "$1" in
  *sername*) printf '%s' "$THALAMUS_GIT_USERNAME" ;;
  *assword*) printf '%s' "$THALAMUS_GIT_PASSWORD" ;;
  *) exit 1 ;;
esac
EOF
        chmod 700 /tmp/thalamus-git-askpass
        export GIT_ASKPASS=/tmp/thalamus-git-askpass
        mkdir -p /workspace/repository
        git_format=sha1
        if [ "$(printf '%s' "$THALAMUS_GIT_COMMIT" | wc -c)" -eq 64 ]; then git_format=sha256; fi
        git -C /workspace/repository init --object-format="$git_format"
        git -C /workspace/repository remote remove origin 2>/dev/null || true
        git -C /workspace/repository remote add origin "$THALAMUS_GIT_URL"
        git -C /workspace/repository -c protocol.file.allow=never -c protocol.ext.allow=never fetch --no-tags origin "$THALAMUS_GIT_REF"
        git -C /workspace/repository checkout --detach --force "$THALAMUS_GIT_COMMIT"
        test "$(git -C /workspace/repository rev-parse --verify 'HEAD^{commit}')" = "$THALAMUS_GIT_COMMIT"
        test -d "/workspace/repository/$THALAMUS_CATALOG_PATH/services"
        git -C /workspace/repository cat-file -e "HEAD:$THALAMUS_CATALOG_PATH/budget-caps.json"
        git -C /workspace/repository cat-file -e "HEAD:$THALAMUS_SERVICE_SCHEMA_PATH"
        test -f "/workspace/repository/$THALAMUS_SERVICE_SCHEMA_PATH"
    ''']
    env: _git_env
    volumeMounts: [{name: "catalog", mountPath: "/workspace/repository"}, {name: "tmp", mountPath: "/tmp"}]
    securityContext: {readOnlyRootFilesystem: True, allowPrivilegeEscalation: False, capabilities: {drop: ["ALL"]}}
}

_db_env = {name: "DATABASE_URL", valueFrom: {secretKeyRef: {name: _config.secrets.database, key: "url"}}}
_common_pod_security = {runAsNonRoot: True, runAsUser: 10001, runAsGroup: 10001, fsGroup: 10001, seccompProfile: {type: "RuntimeDefault"}}
_container_security = {readOnlyRootFilesystem: True, allowPrivilegeEscalation: False, capabilities: {drop: ["ALL"]}}
# The worker adds this CA to each owned result-upload Secret for runner Pods.
# Runner upload clients add it to their default roots; never disable TLS checks.
_s3_volumes = ([{name: "s3-ca", secret: {secretName: _config.secrets.s3CA, items: [{key: "ca.crt", path: "ca.pem"}], defaultMode: 292}}] if _config.secrets.s3CA != "" else [])
_s3_mounts = ([{name: "s3-ca", mountPath: "/var/run/thalamus/s3-ca", readOnly: True}] if _config.secrets.s3CA != "" else [])
_s3_env = ([{name: "THALAMUS_S3_CA_FILE", value: "/var/run/thalamus/s3-ca/ca.pem"}] if _config.secrets.s3CA != "" else [])
_oidc_volumes = ([{name: "oidc-ca", secret: {secretName: _config.secrets.oidcCA, items: [{key: "ca.crt", path: "ca.pem"}], defaultMode: 292}}] if _config.secrets.oidcCA != "" else [])
_oidc_mounts = ([{name: "oidc-ca", mountPath: "/var/run/thalamus/oidc-ca", readOnly: True}] if _config.secrets.oidcCA != "" else [])
_oidc_env = ([{name: "THALAMUS_OIDC_CA_FILE", value: "/var/run/thalamus/oidc-ca/ca.pem"}] if _config.secrets.oidcCA != "" else [])
# If databaseCA is set, reference /var/run/thalamus/db-ca/ca.pem from the
# runtime PostgreSQL URL's sslrootcert and require sslmode=verify-full.
_db_volumes = ([{name: "db-ca", secret: {secretName: _config.secrets.databaseCA, items: [{key: "ca.crt", path: "ca.pem"}], defaultMode: 292}}] if _config.secrets.databaseCA != "" else [])
_db_mounts = ([{name: "db-ca", mountPath: "/var/run/thalamus/db-ca", readOnly: True}] if _config.secrets.databaseCA != "" else [])
_group_env = [{name: "THALAMUS_GROUP_MAPPING", value: json.encode(_config.groupMapping)}]
_common_volumes = [{name: "catalog", emptyDir: {}}, {name: "tmp", emptyDir: {}}] + _s3_volumes + _oidc_volumes + _db_volumes
_common_mounts = [{name: "catalog", mountPath: "/workspace/repository", readOnly: True}, {name: "tmp", mountPath: "/tmp"}] + _s3_mounts + _oidc_mounts + _db_mounts
_catalog_dir = "/workspace/repository/${_config.catalog.path}"
_access_env = [
    {name: "THALAMUS_ACCESS_REPOSITORY", value: "/workspace/repository"},
    {name: "THALAMUS_ACCESS_CATALOG_PATH", value: _config.catalog.path},
    {name: "THALAMUS_ACCESS_COMMIT", value: _config.catalog.commit},
]
_catalog_auth_env = ([
    {name: "GIT_TERMINAL_PROMPT", value: "0"},
    {name: "GIT_ASKPASS", value: "/tmp/thalamus-git-askpass"},
    {name: "THALAMUS_GIT_USERNAME", valueFrom: {secretKeyRef: {name: _config.secrets.gitAuth, key: "username"}}},
    {name: "THALAMUS_GIT_PASSWORD", valueFrom: {secretKeyRef: {name: _config.secrets.gitAuth, key: "password"}}},
] if _config.secrets.gitAuth != "" else [])
_s3_common_env = [
    {name: "THALAMUS_S3_ENDPOINT", value: _config.s3.endpoint},
    {name: "THALAMUS_S3_BUCKET", value: _config.s3.bucket},
    {name: "THALAMUS_S3_REGION", value: _config.s3.region},
    {name: "THALAMUS_S3_ACCESS_KEY_ID", valueFrom: {secretKeyRef: {name: _config.secrets.s3, key: "access-key-id"}}},
    {name: "THALAMUS_S3_SECRET_ACCESS_KEY", valueFrom: {secretKeyRef: {name: _config.secrets.s3, key: "secret-access-key"}}},
]
_server_env = [
    _db_env,
    {name: "THALAMUS_BIND", value: "0.0.0.0:8080"},
    {name: "THALAMUS_UI_DIR", value: "/app/ui"},
    {name: "THALAMUS_PUBLIC_ORIGIN", value: "https://${_config.gateway.hostname}"},
    {name: "THALAMUS_CATALOG_DIR", value: _catalog_dir},
    {name: "THALAMUS_PROMETHEUS_URL", value: _config.prometheusUrl},
    {name: "THALAMUS_K8S_NAMESPACE", value: _config.workloadNamespace},
    {name: "THALAMUS_GATEWAY_CONFIGMAP", value: _config.projectionName},
    {name: "THALAMUS_RUNNERS_JSON", value: json.encode(_config.runners)},
    {name: "THALAMUS_VALIDATION_CONFIGS_JSON", value: json.encode(_config.validationConfigs)},
    {name: "THALAMUS_OIDC_ISSUER", valueFrom: {secretKeyRef: {name: _config.secrets.oidc, key: "issuer"}}},
    {name: "THALAMUS_OIDC_CLIENT_ID", valueFrom: {secretKeyRef: {name: _config.secrets.oidc, key: "client-id"}}},
    {name: "THALAMUS_OIDC_CLIENT_SECRET", valueFrom: {secretKeyRef: {name: _config.secrets.oidc, key: "client-secret"}}},
    {name: "THALAMUS_SESSION_SIGNING_KEY", valueFrom: {secretKeyRef: {name: _config.secrets.signing, key: "session-signing-key"}}},
] + _access_env + _catalog_auth_env + _s3_common_env + _s3_env + _oidc_env + _group_env + [{name: "THALAMUS_RUNTIME_CONFIG", value: json.encode(_config.runtimeConfig)}] + ([{name: "THALAMUS_DEBUG_SERVER_CONFIG", value: json.encode(_config.debugServerConfig)}] if len(_config.debugServerConfig) > 0 else []) + ([{name: "THALAMUS_WORKFLOWS_CONFIG", valueFrom: {secretKeyRef: {name: _config.secrets.workflows, key: "config"}}}] if _config.secrets.workflows != "" else []) + _config.serverExtraEnv
_worker_env = [
    _db_env,
    {name: "THALAMUS_CATALOG_DIR", value: _catalog_dir},
    {name: "THALAMUS_OIDC_ISSUER", valueFrom: {secretKeyRef: {name: _config.secrets.oidc, key: "issuer"}}},
    {name: "THALAMUS_K8S_NAMESPACE", value: _config.workloadNamespace},
    {name: "THALAMUS_GATEWAY_CONFIGMAP", value: _config.projectionName},
    {name: "THALAMUS_GATEWAY_SELECTOR_LABELS", value: json.encode(_config.projectionLabels)},
    {name: "THALAMUS_WORKER_ID", valueFrom: {fieldRef: {fieldPath: "metadata.name"}}},
    {name: "THALAMUS_LEASE_NAME", value: _config.leaseName},
    {name: "THALAMUS_WORKER_HEALTH_ADDR", value: "0.0.0.0:8081"},
    {name: "THALAMUS_RUNNERS_JSON", value: json.encode(_config.runners)},
    {name: "THALAMUS_VALIDATION_CONFIGS_JSON", value: json.encode(_config.validationConfigs)},
    {name: "THALAMUS_S3_RUN_SECRET_PREFIX", value: _config.s3.runSecretPrefix},
    {name: "THALAMUS_USAGE_PROMETHEUS_URL", value: _config.usage.prometheusUrl},
    {name: "THALAMUS_USAGE_INPUT_COUNTER", value: _config.usage.inputCounter},
    {name: "THALAMUS_USAGE_OUTPUT_COUNTER", value: _config.usage.outputCounter},
    {name: "THALAMUS_USAGE_STEP_SECONDS", value: "${_config.usage.stepSeconds}"},
    {name: "THALAMUS_USAGE_START_DATE", value: _config.usage.startDate},
    {name: "THALAMUS_USAGE_RETENTION_DAYS", value: "${_config.usage.retentionDays}"},
] + _access_env + _catalog_auth_env + _s3_common_env + _s3_env + _oidc_env + _group_env + [{name: "THALAMUS_RUNTIME_CONFIG", value: json.encode(_config.runtimeConfig)}] + ([{name: "THALAMUS_DEBUG_CONFIG_JSON", value: json.encode(_config.debugConfig)}] if len(_config.debugConfig) > 0 else []) + ([{name: "THALAMUS_WORKFLOWS_CONFIG", valueFrom: {secretKeyRef: {name: _config.secrets.workflows, key: "config"}}}] if _config.secrets.workflows != "" else []) + ([
    {name: "THALAMUS_IDP_KEYCLOAK_CLIENT_ID", valueFrom: {secretKeyRef: {name: _config.secrets.keycloakAdmin, key: "client-id"}}},
    {name: "THALAMUS_IDP_KEYCLOAK_CLIENT_SECRET", valueFrom: {secretKeyRef: {name: _config.secrets.keycloakAdmin, key: "client-secret"}}},
] if _config.secrets.keycloakAdmin != "" else []) + _config.workerExtraEnv
_database = database.Database {namespace = _system, postgresImage = _config.images.postgresClient, adminSecret = _config.secrets.databaseAdmin, runtimeSecret = _config.secrets.database, bootstrapRevision = _config.bootstrapRevision, caSecret = _config.secrets.databaseCA}
_rbac = rbac.Rbac {systemNamespace = _system, workloadNamespace = _config.workloadNamespace, projectionName = _config.projectionName, leaseName = _config.leaseName, observationNamespaces = _config.observationNamespaces, agenticSecretNames = _config.agenticSecretNames, debugGatewayNamespace = (_config.debugConfig["gateway_service_namespace"] if len(_config.debugConfig) > 0 else ""), debugGatewayService = (_config.debugConfig["gateway_service_name"] if len(_config.debugConfig) > 0 else ""), rateLimitNamespace = (_config.debugConfig["rate_limit_namespace"] if len(_config.debugConfig) > 0 else ""), rateLimitConfigMap = (_config.debugConfig["rate_limit_configmap"] if len(_config.debugConfig) > 0 else ""), rateLimitService = (_config.debugConfig["rate_limit_service"] if len(_config.debugConfig) > 0 else ""), rateLimitDeployment = (_config.debugConfig["rate_limit_deployment"] if len(_config.debugConfig) > 0 else "")}
_security = security.Security {systemNamespace = _system, workloadNamespace = _config.workloadNamespace, gatewayNamespace = _config.gateway.proxyNamespace, gatewayPodLabels = _config.gateway.proxyLabels, systemEgress = _config.network.systemEgress, runnerEgress = _config.network.runnerEgress, workloadPolicies = _config.network.workloadPolicies, adminUsers = _config.network.adminUsers, adminGroups = _config.network.adminGroups, runnerImages = [_config.runners[r].image for r in _config.runners], runnerTemplates = [{image: _config.runners[r].image, command: _config.runners[r].command} for r in _config.runners], debugImages = ([_config.debugConfig["opencode_image"], _config.debugConfig["checkout_image"]] if len(_config.debugConfig) > 0 else [])}
_catalog_source = (gitops.CatalogSource {namespace = _system, url = _config.catalog.url, ref = _config.catalog.ref, sourceName = _config.gitops.sourceName, secretName = _config.gitops.secretName} if _config.gitops.enabled or _config.poolGitOps.enabled else Undefined)
_gitops = (gitops.GitOps {namespace = _config.workloadNamespace, sourceNamespace = _system, sourceName = _config.gitops.sourceName, reconcilerName = _config.gitops.reconcilerName, poolDependencyName = (_config.poolGitOps.reconcilerName if _config.poolGitOps.enabled else ""), poolDependencyNamespace = (_system if _config.poolGitOps.enabled else ""), extraRules = _config.gitops.extraRules} if _config.gitops.enabled else Undefined)
_pool_gitops = (pool_gitops.PoolGitOps {systemNamespace = _system, sourceName = _config.gitops.sourceName, reconcilerName = _config.poolGitOps.reconcilerName} if _config.poolGitOps.enabled else Undefined)
_redis = redis.Redis {namespace = _config.workloadNamespace, name = _config.redis.name, image = _config.redis.image, authSecret = _config.redis.authSecret, tlsSecret = _config.redis.tlsSecret, storageClass = _config.redis.storageClass, storageCapacity = _config.redis.storageCapacity, port = _config.redis.port, limiterSelector = _config.redis.limiterSelector, adminUsers = _config.network.adminUsers, adminGroups = _config.network.adminGroups}
_flux_mcp = (flux_mcp.FluxMcp {namespace = _config.fluxMcp.namespace, systemNamespace = _system, workloadNamespace = _config.workloadNamespace, observationNamespaces = [_config.workloadNamespace], apiEgress = _config.fluxMcp.apiEgress, imageRepository = _config.fluxMcp.imageRepository, imageDigest = _config.fluxMcp.imageDigest} if _config.fluxMcp.enabled else Undefined)
_proxy_dns_egress = {
    to: [{
        namespaceSelector: {matchLabels: {"kubernetes.io/metadata.name": "kube-system"}}
        podSelector: {matchLabels: {"k8s-app": "kube-dns"}}
    }]
    ports: [{protocol: "UDP", port: 53}, {protocol: "TCP", port: 53}]
}

apiVersion = "v1"
kind = "List"
items = [
    {apiVersion: "v1", kind: "Namespace", metadata: {name: _system}},
    {apiVersion: "v1", kind: "Namespace", metadata: {name: _config.workloadNamespace, labels: {"thalamus.dev/live-admission": "enforced"}}},
] + ([{apiVersion: "v1", kind: "Namespace", metadata: {name: _config.gateway.namespace}}] if _config.gateway.create and _config.gateway.namespace != _system and _config.gateway.namespace != _config.workloadNamespace else []) + _rbac.items + _security.items + _database.items + _redis.items + (_catalog_source.items if _config.gitops.enabled or _config.poolGitOps.enabled else []) + (_gitops.items if _config.gitops.enabled else []) + (_pool_gitops.items if _config.poolGitOps.enabled else []) + (_flux_mcp.items if _config.fluxMcp.enabled else []) + [gateway_policy.metrics_policy(g.namespace, g.name) for g in _config.metricsGateways] + [
    {
        apiVersion: "apps/v1"
        kind: "Deployment"
        metadata: {name: "thalamus-server", namespace: _system}
        spec: {
            replicas: _config.serverReplicas
            selector: {matchLabels: {"app.kubernetes.io/name": "thalamus-server"}}
            template: {
                metadata: {labels: {"app.kubernetes.io/name": "thalamus-server"}}
                spec: {
                    serviceAccountName: "thalamus-server"
                    automountServiceAccountToken: True
                    securityContext: _common_pod_security
                    initContainers: [_checkout]
                    containers: [{
                        name: "server", image: _config.images.thalamus, imagePullPolicy: "IfNotPresent"
                        command: ["thalamus-server"], env: _server_env
                        ports: [{name: "http", containerPort: 8080}]
                        readinessProbe: {httpGet: {path: "/health", port: "http"}, periodSeconds: 10}
                        livenessProbe: {httpGet: {path: "/health", port: "http"}, initialDelaySeconds: 20, periodSeconds: 20}
                        securityContext: _container_security
                        resources: {requests: {cpu: "100m", memory: "128Mi"}, limits: {cpu: "2", memory: "1Gi"}}
                        volumeMounts: _common_mounts + _config.serverExtraVolumeMounts
                    }]
                    volumes: _common_volumes + _config.serverExtraVolumes
                }
            }
        }
    },
    {
        apiVersion: "apps/v1"
        kind: "Deployment"
        metadata: {name: "thalamus-worker", namespace: _system}
        spec: {
            replicas: _config.workerReplicas
            selector: {matchLabels: {"app.kubernetes.io/name": "thalamus-worker"}}
            template: {
                metadata: {labels: {"app.kubernetes.io/name": "thalamus-worker"}}
                spec: {
                    serviceAccountName: "thalamus-worker"
                    automountServiceAccountToken: True
                    securityContext: _common_pod_security
                    initContainers: [_checkout, {
                        name: "wait-for-server-migrations", image: _config.images.thalamus
                        command: ["/bin/sh", "-ec"]
                        args: ["until curl --fail --silent --show-error --max-time 2 http://thalamus-server.thalamus-system.svc:80/health >/dev/null; do sleep 5; done"]
                        securityContext: _container_security
                    }]
                    containers: [{
                        name: "worker", image: _config.images.thalamus, imagePullPolicy: "IfNotPresent"
                        command: ["thalamus-worker"], env: _worker_env
                        ports: [{name: "health", containerPort: 8081}]
                        readinessProbe: {httpGet: {path: "/readyz", port: "health"}, periodSeconds: 10}
                        livenessProbe: {httpGet: {path: "/livez", port: "health"}, initialDelaySeconds: 20, periodSeconds: 20}
                        securityContext: _container_security
                        resources: {requests: {cpu: "100m", memory: "128Mi"}, limits: {cpu: "2", memory: "1Gi"}}
                        volumeMounts: _common_mounts + _config.workerExtraVolumeMounts
                    }]
                    volumes: _common_volumes + _config.workerExtraVolumes
                }
            }
        }
    },
    {
        apiVersion: "v1", kind: "Service", metadata: {name: "thalamus-server", namespace: _system}
        spec: {selector: {"app.kubernetes.io/name": "thalamus-server"}, ports: [{name: "http", port: 80, targetPort: 8080}]}
    },
    {
        apiVersion: "gateway.networking.k8s.io/v1", kind: "HTTPRoute", metadata: {name: "thalamus-server", namespace: _system}
        spec: {
            parentRefs: [{name: _config.gateway.name, namespace: _config.gateway.namespace, sectionName: _config.gateway.listener}]
            hostnames: [_config.gateway.hostname]
            rules: [{backendRefs: [{name: "thalamus-server", port: 80}]}]
        }
    },
    {
        # Owned by the platform administrator, not by individual services.
        apiVersion: "gateway.networking.k8s.io/v1", kind: "Gateway"
        metadata: {name: _config.servingGateway.name, namespace: _config.workloadNamespace}
        spec: {
            gatewayClassName: _config.servingGateway.className
            listeners: [{
                name: _config.servingGateway.listener, port: _config.servingGateway.port, protocol: "HTTPS"
                tls: {mode: "Terminate", certificateRefs: [{kind: "Secret", name: _config.servingGateway.tlsSecret}]}
                allowedRoutes: {namespaces: {from: "Same"}}
            } | ({hostname: _config.servingGateway.hostname} if _config.servingGateway.hostname != "" else {})]
        }
    },
    {
        apiVersion: "networking.k8s.io/v1", kind: "NetworkPolicy"
        metadata: {name: "live-serving-gateway-proxy", namespace: _config.workloadNamespace}
        spec: {
            podSelector: {matchLabels: _config.servingGateway.proxyLabels}
            policyTypes: ["Ingress", "Egress"]
            ingress: [{from: _config.servingGateway.proxyIngress, ports: [{protocol: "TCP", port: _config.servingGateway.proxyPort}]}] + ([{from: _config.servingGateway.metricsIngress, ports: [{protocol: "TCP", port: _config.servingGateway.metricsPort}]}] if len(_config.servingGateway.metricsIngress) > 0 else [])
            egress: [_proxy_dns_egress] + _config.servingGateway.proxyEgress
        }
    },
] + ([{
    apiVersion: "v1", kind: "ConfigMap"
    metadata: {name: _config.agenticGateway.caConfigMap, namespace: _config.workloadNamespace}
    immutable: True
    data: {"ca.crt": _config.agenticGateway.caBundle}
}, {
    apiVersion: "agentgateway.dev/v1alpha1", kind: "AgentgatewayParameters"
    metadata: {name: _config.agenticGateway.name, namespace: _config.workloadNamespace}
    spec: {service: {spec: {type: "ClusterIP"}}}
}, {
    apiVersion: "gateway.networking.k8s.io/v1", kind: "Gateway"
    metadata: {name: _config.agenticGateway.name, namespace: _config.workloadNamespace}
    spec: {
        gatewayClassName: _config.agenticGateway.className
        infrastructure: {parametersRef: {group: "agentgateway.dev", kind: "AgentgatewayParameters", name: _config.agenticGateway.name}}
        listeners: [{
            name: "https", port: 443, protocol: "HTTPS"
            hostname: "${_config.agenticGateway.name}.${_config.workloadNamespace}.svc.cluster.local"
            tls: {mode: "Terminate", certificateRefs: [{kind: "Secret", name: _config.agenticGateway.tlsSecret}]}
            allowedRoutes: {namespaces: {from: "Same"}}
        }]
    }
}, {
    apiVersion: "networking.k8s.io/v1", kind: "NetworkPolicy"
    metadata: {name: "live-agentic-gateway-proxy", namespace: _config.workloadNamespace}
    spec: {
        podSelector: {matchLabels: _config.agenticGateway.proxyLabels}
        policyTypes: ["Ingress", "Egress"]
        ingress: [{
            from: [{podSelector: {matchExpressions: [{key: "thalamus.dev/agentic-instance", operator: "Exists"}]}}]
            ports: [{protocol: "TCP", port: 443}] + ([{protocol: "TCP", port: _config.agenticGateway.proxyPort}] if _config.agenticGateway.proxyPort != 443 else [])
        }] + ([{from: _config.agenticGateway.metricsIngress, ports: [{protocol: "TCP", port: _config.agenticGateway.metricsPort}]}] if len(_config.agenticGateway.metricsIngress) > 0 else [])
        egress: [_proxy_dns_egress] + _config.agenticGateway.proxyEgress
    }
}] if _config.agenticGateway.enabled else []) + ([{
    apiVersion: "gateway.networking.k8s.io/v1", kind: "Gateway"
    metadata: {name: _config.gateway.name, namespace: _config.gateway.namespace}
    spec: {
        gatewayClassName: _config.gateway.className
        listeners: [{
            name: _config.gateway.listener, hostname: _config.gateway.hostname, port: 443, protocol: "HTTPS"
            tls: {mode: "Terminate", certificateRefs: [{kind: "Secret", name: _config.gateway.tlsSecret}]}
            allowedRoutes: {namespaces: {from: "Selector", selector: {matchLabels: {"kubernetes.io/metadata.name": _system}}}}
        }]
    }
}] if _config.gateway.create else []) + ([{
    apiVersion: "networking.k8s.io/v1", kind: "NetworkPolicy"
    metadata: {name: "live-control-gateway-proxy", namespace: _config.gateway.proxyNamespace}
    spec: {
        podSelector: {matchLabels: _config.gateway.proxyLabels}
        policyTypes: ["Ingress", "Egress"]
        ingress: [{from: _config.gateway.proxyIngress, ports: [{protocol: "TCP", port: _config.gateway.proxyPort}]}] + ([{from: _config.gateway.metricsIngress, ports: [{protocol: "TCP", port: _config.gateway.metricsPort}]}] if len(_config.gateway.metricsIngress) > 0 else [])
        egress: [_proxy_dns_egress, {
            to: [{
                namespaceSelector: {matchLabels: {"kubernetes.io/metadata.name": _system}}
                podSelector: {matchLabels: {"app.kubernetes.io/name": "thalamus-server"}}
            }]
            ports: [{protocol: "TCP", port: 8080}]
        }] + _config.gateway.proxyEgress
    }
}] if _config.gateway.proxyNamespace in [_system, _config.workloadNamespace] else [])
