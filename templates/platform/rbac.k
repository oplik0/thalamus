# These objects must be installed/changed only by a trusted admission administrator.
# The system namespace is separate from the dedicated workload namespace: the
# worker can list every Secret in the latter, irrespective of its label selector.
# Both observers can get any known ConfigMap name in the dedicated workload
# namespace for dynamically named candidate manifests. Their shared role cannot
# list or watch ConfigMaps; the worker has separate projection write access.
# Do not put sensitive configuration there; verify manifest trust separately.
# The static server role can get only explicitly named agentic Secrets.
# Per-session debug access is granted by worker-created roles limited to the
# exact server Secret name.
# Server and worker cluster-wide Gateway/HTTPRoute reads expose complete objects
# in every namespace, including spec values; administrators must accept disclosure.
# Node get exposes full metadata of any known Node, not just serving nodes.
# Live labels and allocatable capacity are evidence only after verification.
# The worker may also inspect the configured workload Namespace and the named
# admission policies and bindings; these cluster-scoped reads are name-limited.
#
# RBAC cannot constrain create by resourceNames, nor express a name prefix for
# dynamic run/debug objects. List/watch with resourceNames also requires clients
# to supply an exact metadata.name field selector. Independent admission must
# reserve thalamus-* in the workload namespace to the system worker and explicitly
# trusted administrators, restrict the worker's writes to its reserved objects,
# and protect these static RBAC objects from worker and untrusted-user changes.
# The worker can create Jobs and Secrets: admission must additionally enforce the
# trusted workload specifications, service accounts, and isolation requirements.
# Labels alone do not establish ownership or authorization.
import regex

schema Rbac:
    systemNamespace: str
    workloadNamespace: str
    projectionName: str
    leaseName: str
    # Optional pre-provisioned agentic Secrets used as runtime evidence.
    agenticSecretNames: [str] = []
    # Explicitly authorize reads in every referenced Gateway/Flux namespace.
    # Missing namespaces make runtime evidence unavailable, not negative.
    observationNamespaces: [str] = []
    # Set both for the optional debug gateway; endpoint reads stay name-scoped.
    debugGatewayNamespace: str = ""
    debugGatewayService: str = ""
    # Optional rate-limit service evidence; configure all four names together.
    rateLimitNamespace: str = ""
    rateLimitConfigMap: str = ""
    rateLimitService: str = ""
    rateLimitDeployment: str = ""

    items: [any] = [
        {
            apiVersion: "v1"
            kind: "ServiceAccount"
            metadata: {name: "thalamus-server", namespace: systemNamespace}
        },
        {
            apiVersion: "v1"
            kind: "ServiceAccount"
            metadata: {name: "thalamus-worker", namespace: systemNamespace}
        },
        {
            apiVersion: "v1"
            kind: "ServiceAccount"
            metadata: {name: "thalamus-runner", namespace: workloadNamespace}
            automountServiceAccountToken: False
        },
        {
            apiVersion: "v1"
            kind: "ServiceAccount"
            metadata: {name: "thalamus-debug", namespace: workloadNamespace}
            automountServiceAccountToken: False
        },
        {
            # Shared read-only identity for Flux-owned raw endpoint pickers.
            # Keeping RBAC administrator-owned avoids granting the catalog
            # reconciler permission to create roles or service accounts.
            apiVersion: "v1"
            kind: "ServiceAccount"
            metadata: {name: "thalamus-epp", namespace: workloadNamespace}
        },
        {
            apiVersion: "rbac.authorization.k8s.io/v1"
            kind: "Role"
            metadata: {name: "thalamus-epp", namespace: workloadNamespace}
            rules: [
                {apiGroups: [""], resources: ["pods"], verbs: ["get", "list", "watch"]},
                {apiGroups: ["inference.networking.k8s.io"], resources: ["inferencepools"], verbs: ["get", "list", "watch"]},
                {apiGroups: ["llm-d.ai"], resources: ["inferenceobjectives", "inferencemodelrewrites"], verbs: ["get", "list", "watch"]},
            ]
        },
        {
            apiVersion: "rbac.authorization.k8s.io/v1"
            kind: "RoleBinding"
            metadata: {name: "thalamus-epp", namespace: workloadNamespace}
            roleRef: {apiGroup: "rbac.authorization.k8s.io", kind: "Role", name: "thalamus-epp"}
            subjects: [{kind: "ServiceAccount", name: "thalamus-epp", namespace: workloadNamespace}]
        },
        {
            apiVersion: "rbac.authorization.k8s.io/v1"
            kind: "Role"
            metadata: {name: "thalamus-server", namespace: workloadNamespace}
            rules: [
                # Candidate manifest names change with each revision, so this
                # covers every ConfigMap by name in the dedicated namespace.
                {apiGroups: [""], resources: ["configmaps"], verbs: ["get"]},
                {apiGroups: [""], resources: ["pods", "services"], verbs: ["get", "list", "watch"]},
                {apiGroups: ["discovery.k8s.io"], resources: ["endpointslices"], verbs: ["list"]},
                {apiGroups: ["apps"], resources: ["deployments", "replicasets", "statefulsets"], verbs: ["get", "list", "watch"]},
                # Pooled drain evidence must detect active scaling controllers.
                {apiGroups: ["autoscaling"], resources: ["horizontalpodautoscalers"], verbs: ["list"]},
                {apiGroups: ["keda.sh"], resources: ["scaledobjects"], verbs: ["list"]},
                {apiGroups: ["batch"], resources: ["jobs"], verbs: ["get", "list", "watch"]},
                {apiGroups: ["source.toolkit.fluxcd.io"], resources: ["gitrepositories", "ocirepositories", "buckets", "helmrepositories", "helmcharts"], verbs: ["get", "list", "watch"]},
                {apiGroups: ["helm.toolkit.fluxcd.io"], resources: ["helmreleases"], verbs: ["get", "list", "watch"]},
                {apiGroups: ["kustomize.toolkit.fluxcd.io"], resources: ["kustomizations"], verbs: ["get", "list", "watch"]},
                {apiGroups: ["serving.kserve.io"], resources: ["llminferenceservices"], verbs: ["get", "list", "watch"]},
                {apiGroups: ["inference.networking.k8s.io"], resources: ["inferencepools"], verbs: ["get", "list", "watch"]},
                {apiGroups: ["gateway.networking.k8s.io"], resources: ["gateways", "httproutes"], verbs: ["get", "list", "watch"]},
                {apiGroups: ["agentgateway.dev"], resources: ["agentgatewaybackends", "agentgatewaypolicies"], verbs: ["get", "list", "watch"]},
            ] + ([{
                # Never render this rule with an empty resourceNames list:
                # Kubernetes would interpret it as access to every Secret.
                apiGroups: [""]
                resources: ["secrets"]
                resourceNames: agenticSecretNames
                verbs: ["get"]
            }] if len(agenticSecretNames) > 0 else [])
        },
        {
            apiVersion: "rbac.authorization.k8s.io/v1"
            kind: "RoleBinding"
            metadata: {name: "thalamus-server", namespace: workloadNamespace}
            roleRef: {apiGroup: "rbac.authorization.k8s.io", kind: "Role", name: "thalamus-server"}
            subjects: [
                {kind: "ServiceAccount", name: "thalamus-server", namespace: systemNamespace},
                {kind: "ServiceAccount", name: "thalamus-worker", namespace: systemNamespace},
            ]
        },
        {
            # Cross-namespace routes and Gateways establish route evidence;
            # Node get supports live hardware checks. No watch is granted here.
            apiVersion: "rbac.authorization.k8s.io/v1"
            kind: "ClusterRole"
            metadata: {name: "thalamus-server-gateway-observer"}
            rules: [
                {apiGroups: ["gateway.networking.k8s.io"], resources: ["httproutes"], verbs: ["list"]},
                {apiGroups: ["gateway.networking.k8s.io"], resources: ["gateways"], verbs: ["get", "list"]},
                {apiGroups: [""], resources: ["nodes"], verbs: ["get"]},
            ]
        },
        {
            apiVersion: "rbac.authorization.k8s.io/v1"
            kind: "ClusterRoleBinding"
            metadata: {name: "thalamus-server-gateway-observer"}
            roleRef: {apiGroup: "rbac.authorization.k8s.io", kind: "ClusterRole", name: "thalamus-server-gateway-observer"}
            subjects: [
                {kind: "ServiceAccount", name: "thalamus-server", namespace: systemNamespace},
                {kind: "ServiceAccount", name: "thalamus-worker", namespace: systemNamespace},
            ]
        },
        {
            # Inspection does not itself guarantee admission is active. Trusted
            # cluster administrators must protect the policy, binding and Namespace.
            apiVersion: "rbac.authorization.k8s.io/v1"
            kind: "ClusterRole"
            metadata: {name: "thalamus-worker-admission-reader"}
            rules: [
                {apiGroups: [""], resources: ["namespaces"], resourceNames: [workloadNamespace], verbs: ["get"]},
                {apiGroups: ["admissionregistration.k8s.io"], resources: ["validatingadmissionpolicies", "validatingadmissionpolicybindings"], resourceNames: ["live-ownership.thalamus.dev", "live-debug-rbac.thalamus.dev", "live-debug-pods.thalamus.dev", "live-worker-jobs.thalamus.dev"], verbs: ["get"]},
            ]
        },
        {
            apiVersion: "rbac.authorization.k8s.io/v1"
            kind: "ClusterRoleBinding"
            metadata: {name: "thalamus-worker-admission-reader"}
            roleRef: {apiGroup: "rbac.authorization.k8s.io", kind: "ClusterRole", name: "thalamus-worker-admission-reader"}
            subjects: [{kind: "ServiceAccount", name: "thalamus-worker", namespace: systemNamespace}]
        },
        {
            apiVersion: "rbac.authorization.k8s.io/v1"
            kind: "Role"
            metadata: {name: "thalamus-worker", namespace: workloadNamespace}
            rules: [
                # The worker currently follows these four specific upstream kinds.
                {apiGroups: ["source.toolkit.fluxcd.io"], resources: ["gitrepositories"], verbs: ["get", "list", "watch"]},
                {apiGroups: ["helm.toolkit.fluxcd.io"], resources: ["helmreleases"], verbs: ["get", "list", "watch"]},
                {apiGroups: ["serving.kserve.io"], resources: ["llminferenceservices"], verbs: ["get", "list", "watch"]},
                {apiGroups: ["inference.networking.k8s.io"], resources: ["inferencepools"], verbs: ["get", "list", "watch"]},
                # Debug isolation checks inspect policies in the workload namespace.
                {apiGroups: ["agentgateway.dev"], resources: ["agentgatewaypolicies"], verbs: ["get", "list"]},
                {apiGroups: ["gateway.networking.k8s.io"], resources: ["httproutes"], verbs: ["get"]},
                {apiGroups: ["batch"], resources: ["jobs"], verbs: ["get", "list", "create", "delete", "patch"]},
                # Pod names are dynamic; apply this grant only after the dedicated
                # fail-closed debug-pod DELETE admission is active. RBAC alone
                # cannot limit deletion to owned session Pods.
                {apiGroups: [""], resources: ["pods"], verbs: ["get", "list", "delete"]},
                # Resolve live serving and endpoint-picker Pod ownership.
                {apiGroups: ["apps"], resources: ["deployments", "replicasets", "statefulsets"], verbs: ["get"]},
                # The named object is checked before patching; create cannot be
                # restricted by name in RBAC and must be gated by admission.
                {apiGroups: [""], resources: ["configmaps"], resourceNames: [projectionName], verbs: ["get", "patch"]},
                {apiGroups: [""], resources: ["configmaps"], verbs: ["create"]},
                # Dynamic run credentials must be listed for cleanup and updated
                # when refreshed. Namespace-wide Secret listing is privileged.
                {apiGroups: [""], resources: ["secrets"], verbs: ["get", "list", "create", "update", "patch", "delete"]},
                {apiGroups: [""], resources: ["services"], verbs: ["get", "list", "create", "patch", "delete"]},
                {apiGroups: ["networking.k8s.io"], resources: ["networkpolicies"], verbs: ["get", "list", "create", "patch", "delete"]},
                # Dynamic debug RBAC must be admission-restricted to a single
                # session's -server Secret and the system server ServiceAccount.
                # Never permit worker mutation of static roles or bindings.
                {apiGroups: ["rbac.authorization.k8s.io"], resources: ["roles", "rolebindings"], verbs: ["get", "create", "delete"]},
                {apiGroups: [""], resources: ["serviceaccounts"], resourceNames: ["thalamus-debug"], verbs: ["get"]},
                {apiGroups: ["coordination.k8s.io"], resources: ["leases"], resourceNames: [leaseName], verbs: ["get", "update"]},
                # Used only if the initial Lease is missing; admission must limit
                # worker Lease creation because RBAC cannot constrain create.
                {apiGroups: ["coordination.k8s.io"], resources: ["leases"], verbs: ["create"]},
            ]
        },
        {
            apiVersion: "rbac.authorization.k8s.io/v1"
            kind: "RoleBinding"
            metadata: {name: "thalamus-worker", namespace: workloadNamespace}
            roleRef: {apiGroup: "rbac.authorization.k8s.io", kind: "Role", name: "thalamus-worker"}
            subjects: [{kind: "ServiceAccount", name: "thalamus-worker", namespace: systemNamespace}]
        },
        {
            apiVersion: "rbac.authorization.k8s.io/v1"
            kind: "Role"
            metadata: {name: "thalamus-debug", namespace: workloadNamespace}
            # Diagnostic objects only; never Secrets, ConfigMaps, or pods/log.
            # Pod and workload specs may still contain sensitive inline values.
            rules: [
                {apiGroups: [""], resources: ["pods", "services"], verbs: ["get", "list", "watch"]},
                {apiGroups: ["batch"], resources: ["jobs"], verbs: ["get", "list", "watch"]},
                {apiGroups: ["serving.kserve.io"], resources: ["llminferenceservices"], verbs: ["get", "list", "watch"]},
                {apiGroups: ["inference.networking.k8s.io"], resources: ["inferencepools"], verbs: ["get", "list", "watch"]},
            ]
        },
        {
            apiVersion: "rbac.authorization.k8s.io/v1"
            kind: "RoleBinding"
            metadata: {name: "thalamus-debug", namespace: workloadNamespace}
            roleRef: {apiGroup: "rbac.authorization.k8s.io", kind: "Role", name: "thalamus-debug"}
            subjects: [{kind: "ServiceAccount", name: "thalamus-debug", namespace: workloadNamespace}]
        },
        # No RoleBinding is given to the runner. Neither runner nor debug pods
        # receive a token by default; the debug role is for explicitly authorized
        # read access, not an instruction to mount credentials in debug Jobs.
        {
            apiVersion: "coordination.k8s.io/v1"
            kind: "Lease"
            metadata: {name: leaseName, namespace: workloadNamespace, labels: {"app.kubernetes.io/managed-by": "thalamus"}}
            # Omit spec: the worker supplies holder and timing when acquiring it.
        },
    ] + [{
        # Pod specs can contain inline sensitive values; grant only in explicitly
        # trusted namespaces and protect this Role independently of workload
        # admission. No Secret, log, watch, or cross-namespace write access.
        apiVersion: "rbac.authorization.k8s.io/v1"
        kind: "Role"
        metadata: {name: "thalamus-server-runtime-observer", namespace: n}
        rules: [
            {apiGroups: [""], resources: ["pods"], verbs: ["list"]},
            {apiGroups: [""], resources: ["services"], verbs: ["get"]},
            {apiGroups: ["apps"], resources: ["deployments", "replicasets", "statefulsets"], verbs: ["get"]},
            {apiGroups: ["agentgateway.dev"], resources: ["agentgatewaypolicies"], verbs: ["list"]},
            {apiGroups: ["kustomize.toolkit.fluxcd.io"], resources: ["kustomizations"], verbs: ["get"]},
            {apiGroups: ["helm.toolkit.fluxcd.io"], resources: ["helmreleases"], verbs: ["get"]},
            {apiGroups: ["source.toolkit.fluxcd.io"], resources: ["gitrepositories", "ocirepositories", "buckets", "helmcharts", "helmrepositories"], verbs: ["get"]},
        ]
    } for n in observationNamespaces if n != workloadNamespace] + [{
        apiVersion: "rbac.authorization.k8s.io/v1"
        kind: "RoleBinding"
        metadata: {name: "thalamus-server-runtime-observer", namespace: n}
        roleRef: {apiGroup: "rbac.authorization.k8s.io", kind: "Role", name: "thalamus-server-runtime-observer"}
        subjects: [
            {kind: "ServiceAccount", name: "thalamus-server", namespace: systemNamespace},
            {kind: "ServiceAccount", name: "thalamus-worker", namespace: systemNamespace},
        ]
    } for n in observationNamespaces if n != workloadNamespace] + ([{
        # The worker verifies the exact gateway Service and its Endpoints.
        apiVersion: "rbac.authorization.k8s.io/v1"
        kind: "Role"
        metadata: {name: "thalamus-worker-debug-gateway", namespace: debugGatewayNamespace}
        rules: [
            {apiGroups: [""], resources: ["services", "endpoints"], resourceNames: [debugGatewayService], verbs: ["get"]},
        ]
    }, {
        apiVersion: "rbac.authorization.k8s.io/v1"
        kind: "RoleBinding"
        metadata: {name: "thalamus-worker-debug-gateway", namespace: debugGatewayNamespace}
        roleRef: {apiGroup: "rbac.authorization.k8s.io", kind: "Role", name: "thalamus-worker-debug-gateway"}
        subjects: [{kind: "ServiceAccount", name: "thalamus-worker", namespace: systemNamespace}]
    }] if debugGatewayNamespace != "" else []) + ([{
        # Inspect only the configured rate limiter objects; this does not attest
        # their integrity or enforcement, which the worker must verify.
        apiVersion: "rbac.authorization.k8s.io/v1"
        kind: "Role"
        metadata: {name: "thalamus-worker-rate-limit", namespace: rateLimitNamespace}
        rules: [
            {apiGroups: [""], resources: ["configmaps"], resourceNames: [rateLimitConfigMap], verbs: ["get"]},
            {apiGroups: [""], resources: ["services", "endpoints"], resourceNames: [rateLimitService], verbs: ["get"]},
            {apiGroups: ["apps"], resources: ["deployments"], resourceNames: [rateLimitDeployment], verbs: ["get"]},
        ]
    }, {
        apiVersion: "rbac.authorization.k8s.io/v1"
        kind: "RoleBinding"
        metadata: {name: "thalamus-worker-rate-limit", namespace: rateLimitNamespace}
        roleRef: {apiGroup: "rbac.authorization.k8s.io", kind: "Role", name: "thalamus-worker-rate-limit"}
        subjects: [{kind: "ServiceAccount", name: "thalamus-worker", namespace: systemNamespace}]
    }] if rateLimitNamespace != "" else [])

    check:
        systemNamespace == "thalamus-system", "system namespace must be thalamus-system"
        workloadNamespace != systemNamespace and len(workloadNamespace) <= 63 and regex.match(workloadNamespace, "^[a-z0-9]([-a-z0-9]*[a-z0-9])?$"), "workload namespace must be a separate DNS label"
        len(projectionName) <= 63 and regex.match(projectionName, "^thalamus-[a-z0-9]([-a-z0-9]*[a-z0-9])?$"), "projection name must be a reserved DNS label"
        len(leaseName) <= 63 and regex.match(leaseName, "^thalamus-[a-z0-9]([-a-z0-9]*[a-z0-9])?$"), "lease name must be a reserved DNS label"
        all s in agenticSecretNames {len(s) <= 253 and regex.match(s, "^[a-z0-9]([-a-z0-9]{0,61}[a-z0-9])?([.][a-z0-9]([-a-z0-9]{0,61}[a-z0-9])?)*$")}, "agentic Secret names must be DNS subdomains"
        len({s: True for s in agenticSecretNames}) == len(agenticSecretNames), "agentic Secret names must be unique"
        all n in observationNamespaces {len(n) <= 63 and regex.match(n, "^[a-z0-9]([-a-z0-9]*[a-z0-9])?$")}, "observation namespaces must be DNS labels"
        len({n: True for n in observationNamespaces}) == len(observationNamespaces), "observation namespaces must be unique"
        (debugGatewayNamespace == "") == (debugGatewayService == ""), "debug gateway namespace and service must be configured together"
        debugGatewayNamespace == "" or (len(debugGatewayNamespace) <= 63 and regex.match(debugGatewayNamespace, "^[a-z0-9]([-a-z0-9]*[a-z0-9])?$")), "debug gateway namespace must be a DNS label"
        debugGatewayService == "" or (len(debugGatewayService) <= 63 and regex.match(debugGatewayService, "^[a-z0-9]([-a-z0-9]*[a-z0-9])?$")), "debug gateway service must be a DNS label"
        (rateLimitNamespace == "" and rateLimitConfigMap == "" and rateLimitService == "" and rateLimitDeployment == "") or (rateLimitNamespace != "" and rateLimitConfigMap != "" and rateLimitService != "" and rateLimitDeployment != ""), "rate-limit namespace and all object names must be configured together"
        rateLimitNamespace == "" or (len(rateLimitNamespace) <= 63 and regex.match(rateLimitNamespace, "^[a-z0-9]([-a-z0-9]*[a-z0-9])?$")), "rate-limit namespace must be a DNS label"
        rateLimitService == "" or (len(rateLimitService) <= 63 and regex.match(rateLimitService, "^[a-z0-9]([-a-z0-9]*[a-z0-9])?$")), "rate-limit service must be a DNS label"
        rateLimitConfigMap == "" or (len(rateLimitConfigMap) <= 253 and regex.match(rateLimitConfigMap, "^[a-z0-9]([-a-z0-9]{0,61}[a-z0-9])?([.][a-z0-9]([-a-z0-9]{0,61}[a-z0-9])?)*$")), "rate-limit ConfigMap must be a DNS subdomain"
        rateLimitDeployment == "" or (len(rateLimitDeployment) <= 253 and regex.match(rateLimitDeployment, "^[a-z0-9]([-a-z0-9]{0,61}[a-z0-9])?([.][a-z0-9]([-a-z0-9]{0,61}[a-z0-9])?)*$")), "rate-limit Deployment must be a DNS subdomain"
