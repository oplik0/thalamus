# Kubernetes 1.30+ ValidatingAdmissionPolicy and NetworkPolicy composition.
# systemEgress and runnerEgress are Kubernetes NetworkPolicy egress rules, not
# endpoint strings. workloadPolicies are complete operator-supplied serving
# NetworkPolicies; with none, serving ingress and egress remain denied. Network
# policies are additive: an operator-supplied or pre-existing policy can widen
# these allowances, including for runners/debug pods if it selects them.
#
# Enforcement requires a compatible CNI and a dedicated workload namespace.
# Protect namespace labels, admission policies/bindings, pod and controller
# creation, and the trusted selectors with RBAC. NetworkPolicy does not isolate
# host-network traffic; node-local DNS needs separately supplied egress rules.
# Install and verify admission and network enforcement before starting workloads:
# applying a Kubernetes List is not an atomic security transition. Runtime
# debug pods receive only their per-session egress policies, never a shared one.
# Runner images and exact image/command pairs are trusted operator inputs; an
# empty list denies those Jobs. Debug images are also allowlisted, but debug
# commands contain dynamic launch data and are not authenticated by this
# policy. A stolen worker credential can therefore alter a debug command: the
# launch verifier is not a substitute for protecting the worker credential.
#
# The admission binding protects namespaces labeled
# thalamus.dev/live-admission=enforced. Explicit admin users/groups are powerful
# bootstrap/emergency overrides, not ordinary RBAC roles. Protect their identity
# and group membership and add the installer's exact identity before binding.
# Controller identities assume kube-controller-manager uses individual service
# accounts; clusters using a shared controller identity must configure cleanup
# accordingly. Status subresources need their own metadata guard because they can
# otherwise alter ownership metadata even when the main resource is protected.
import json

_dns_egress = {
    to: [{
        namespaceSelector: {matchLabels: {"kubernetes.io/metadata.name": "kube-system"}}
        podSelector: {matchLabels: {"k8s-app": "kube-dns"}}
    }]
    ports: [{protocol: "UDP", port: 53}, {protocol: "TCP", port: 53}]
}

# Check both sides of admission, including generateName before the API server
# assigns a concrete name. Variables are lazy, so CREATE/DELETE safely evaluate
# the missing old/new object as unreserved.
_ownership_variables = [
    {name: "newName", expression: 'object != null && has(object.metadata) && has(object.metadata.name) && object.metadata.name.startsWith("thalamus-")'},
    {name: "newGenerateName", expression: 'object != null && has(object.metadata) && has(object.metadata.generateName) && object.metadata.generateName.startsWith("thalamus-")'},
    {name: "newManaged", expression: 'object != null && has(object.metadata) && has(object.metadata.labels) && "app.kubernetes.io/managed-by" in object.metadata.labels && object.metadata.labels["app.kubernetes.io/managed-by"] == "thalamus"'},
    {name: "oldName", expression: 'oldObject != null && has(oldObject.metadata) && has(oldObject.metadata.name) && oldObject.metadata.name.startsWith("thalamus-")'},
    {name: "oldGenerateName", expression: 'oldObject != null && has(oldObject.metadata) && has(oldObject.metadata.generateName) && oldObject.metadata.generateName.startsWith("thalamus-")'},
    {name: "oldManaged", expression: 'oldObject != null && has(oldObject.metadata) && has(oldObject.metadata.labels) && "app.kubernetes.io/managed-by" in oldObject.metadata.labels && oldObject.metadata.labels["app.kubernetes.io/managed-by"] == "thalamus"'},
    {name: "newReserved", expression: 'variables.newName || variables.newGenerateName || variables.newManaged'},
    {name: "oldReserved", expression: 'variables.oldName || variables.oldGenerateName || variables.oldManaged'},
    # These explicit fields are common to all covered built-in objects. Status
    # requests and GC cleanup must not alter ownership or object identity.
    # Finalizers may change, and managedFields is API-server bookkeeping.
    {name: "stableMetadata", expression: """
        (has(object.metadata.name) == has(oldObject.metadata.name) && (!has(object.metadata.name) || object.metadata.name == oldObject.metadata.name)) &&
        (has(object.metadata.generateName) == has(oldObject.metadata.generateName) && (!has(object.metadata.generateName) || object.metadata.generateName == oldObject.metadata.generateName)) &&
        (has(object.metadata.namespace) == has(oldObject.metadata.namespace) && (!has(object.metadata.namespace) || object.metadata.namespace == oldObject.metadata.namespace)) &&
        (has(object.metadata.uid) == has(oldObject.metadata.uid) && (!has(object.metadata.uid) || object.metadata.uid == oldObject.metadata.uid)) &&
        (has(object.metadata.resourceVersion) == has(oldObject.metadata.resourceVersion) && (!has(object.metadata.resourceVersion) || object.metadata.resourceVersion == oldObject.metadata.resourceVersion)) &&
        (has(object.metadata.generation) == has(oldObject.metadata.generation) && (!has(object.metadata.generation) || object.metadata.generation == oldObject.metadata.generation)) &&
        (has(object.metadata.creationTimestamp) == has(oldObject.metadata.creationTimestamp) && (!has(object.metadata.creationTimestamp) || object.metadata.creationTimestamp == oldObject.metadata.creationTimestamp)) &&
        (has(object.metadata.deletionTimestamp) == has(oldObject.metadata.deletionTimestamp) && (!has(object.metadata.deletionTimestamp) || object.metadata.deletionTimestamp == oldObject.metadata.deletionTimestamp)) &&
        (has(object.metadata.deletionGracePeriodSeconds) == has(oldObject.metadata.deletionGracePeriodSeconds) && (!has(object.metadata.deletionGracePeriodSeconds) || object.metadata.deletionGracePeriodSeconds == oldObject.metadata.deletionGracePeriodSeconds)) &&
        (has(object.metadata.selfLink) == has(oldObject.metadata.selfLink) && (!has(object.metadata.selfLink) || object.metadata.selfLink == oldObject.metadata.selfLink)) &&
        (has(object.metadata.labels) == has(oldObject.metadata.labels) && (!has(object.metadata.labels) || object.metadata.labels == oldObject.metadata.labels)) &&
        (has(object.metadata.annotations) == has(oldObject.metadata.annotations) && (!has(object.metadata.annotations) || object.metadata.annotations == oldObject.metadata.annotations)) &&
        (has(object.metadata.ownerReferences) == has(oldObject.metadata.ownerReferences) && (!has(object.metadata.ownerReferences) || object.metadata.ownerReferences == oldObject.metadata.ownerReferences))
    """},
]

schema Security:
    systemNamespace: str
    workloadNamespace: str
    gatewayNamespace: str
    gatewayPodLabels: {str:str}
    systemEgress: [any] = []
    runnerEgress: [any] = []
    workloadPolicies: [any] = []
    runnerImages: [str] = []
    runnerTemplates: [any] = []
    debugImages: [str] = []
    adminUsers: [str] = []
    adminGroups: [str] = ["system:masters"]

    # CEL map literals otherwise infer one value type from the first entry and
    # reject the command list. Preserve exact image/command pairs while allowing
    # each map value to have its own runtime type.
    _runnerTemplateExpression = "[" + ", ".join(['{"image": dyn(' + json.encode(t.image) + '), "command": dyn(' + json.encode(t.command) + ')}' for t in runnerTemplates]) + "]"

    items: [any] = [{
        apiVersion: "admissionregistration.k8s.io/v1"
        kind: "ValidatingAdmissionPolicy"
        metadata: {name: "live-ownership.thalamus.dev"}
        spec: {
            failurePolicy: "Fail"
            matchConstraints: {
                matchPolicy: "Equivalent"
                namespaceSelector: {matchLabels: {"thalamus.dev/live-admission": "enforced"}}
                resourceRules: [
                    {apiGroups: [""], apiVersions: ["v1"], operations: ["CREATE", "UPDATE", "DELETE"], resources: ["configmaps", "secrets", "services"], scope: "Namespaced"},
                    {apiGroups: ["batch"], apiVersions: ["v1"], operations: ["CREATE", "UPDATE", "DELETE"], resources: ["jobs"], scope: "Namespaced"},
                    {apiGroups: ["networking.k8s.io"], apiVersions: ["v1"], operations: ["CREATE", "UPDATE", "DELETE"], resources: ["networkpolicies"], scope: "Namespaced"},
                    {apiGroups: ["coordination.k8s.io"], apiVersions: ["v1"], operations: ["CREATE", "UPDATE", "DELETE"], resources: ["leases"], scope: "Namespaced"},
                    {apiGroups: ["rbac.authorization.k8s.io"], apiVersions: ["v1"], operations: ["CREATE", "UPDATE", "DELETE"], resources: ["roles", "rolebindings"], scope: "Namespaced"},
                ]
            }
            variables: _ownership_variables + [
                {name: "worker", expression: 'request.userInfo.username == ' + json.encode("system:serviceaccount:" + systemNamespace + ":thalamus-worker")},
                {name: "admin", expression: '(request.userInfo.username in ' + json.encode(adminUsers) + ') || (has(request.userInfo.groups) && request.userInfo.groups.exists(g, g in ' + json.encode(adminGroups) + '))'},
                {name: "ttl", expression: 'request.userInfo.username == "system:serviceaccount:kube-system:ttl-after-finished-controller"'},
                {name: "gc", expression: 'request.userInfo.username == "system:serviceaccount:kube-system:generic-garbage-collector"'},
                # A persistent, random session nonce is required on all
                # worker-owned debug resources, including staged RBAC grants.
                {name: "newDebug", expression: 'object != null && has(object.metadata) && ((has(object.metadata.name) && object.metadata.name.startsWith("thalamus-debug-")) || (has(object.metadata.generateName) && object.metadata.generateName.startsWith("thalamus-debug-")))'},
                {name: "oldDebug", expression: 'oldObject != null && has(oldObject.metadata) && ((has(oldObject.metadata.name) && oldObject.metadata.name.startsWith("thalamus-debug-")) || (has(oldObject.metadata.generateName) && oldObject.metadata.generateName.startsWith("thalamus-debug-")))'},
                {name: "newDebugNonceValid", expression: 'object != null && has(object.metadata) && has(object.metadata.annotations) && "thalamus.dev/ownership-nonce" in object.metadata.annotations && object.metadata.annotations["thalamus.dev/ownership-nonce"] != "00000000-0000-0000-0000-000000000000" && object.metadata.annotations["thalamus.dev/ownership-nonce"].matches("^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")'},
                {name: "oldDebugNonceValid", expression: 'oldObject != null && has(oldObject.metadata) && has(oldObject.metadata.annotations) && "thalamus.dev/ownership-nonce" in oldObject.metadata.annotations && oldObject.metadata.annotations["thalamus.dev/ownership-nonce"] != "00000000-0000-0000-0000-000000000000" && oldObject.metadata.annotations["thalamus.dev/ownership-nonce"].matches("^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")'},
                # Foreground TTL cleanup requires GC to remove an existing
                # finalizer via a root Job UPDATE, not a /finalizers subresource.
                # Compare the Job's spec/status explicitly. dyn field selection
                # keeps this expression valid for the other matched kinds, whose
                # requests never enter the Job-only branch.
                {name: "gcFinalizerRemoval", expression: """
                    variables.gc && request.operation == "UPDATE" && request.resource.group == "batch" && request.resource.resource == "jobs" &&
                    variables.oldReserved && variables.newReserved && has(oldObject.metadata.deletionTimestamp) && has(oldObject.metadata.finalizers) && size(oldObject.metadata.finalizers) > 0 && variables.stableMetadata &&
                    (has(object.apiVersion) == has(oldObject.apiVersion) && (!has(object.apiVersion) || object.apiVersion == oldObject.apiVersion)) &&
                    (has(object.kind) == has(oldObject.kind) && (!has(object.kind) || object.kind == oldObject.kind)) &&
                    has(dyn(object).spec) && has(dyn(oldObject).spec) && dyn(object).spec == dyn(oldObject).spec &&
                    (has(dyn(object).status) == has(dyn(oldObject).status) && (!has(dyn(object).status) || dyn(object).status == dyn(oldObject).status)) &&
                    (!has(object.metadata.finalizers) || (size(object.metadata.finalizers) < size(oldObject.metadata.finalizers) && object.metadata.finalizers.all(f, f in oldObject.metadata.finalizers)))
                """},
            ]
            validations: [{
                expression: """
                    variables.worker ?
                    (request.operation == "CREATE" ? (variables.newManaged && (variables.newName || (variables.newGenerateName && (!has(object.metadata.name) || object.metadata.name == "")))) :
                    (request.operation == "DELETE" ? (variables.oldName && variables.oldManaged) :
                    (variables.oldName && variables.oldManaged && variables.newName && variables.newManaged && (!variables.oldGenerateName || variables.newGenerateName)))) :
                    ((!variables.oldReserved && !variables.newReserved) ||
                    (variables.admin && !variables.ttl && !variables.gc) ||
                    ((variables.ttl || variables.gc) && request.operation == "DELETE") || variables.gcFinalizerRemoval)
                """
                message: "worker writes require a thalamus- name and managed-by=thalamus; reserved objects are otherwise restricted"
            }, {
                expression: """
                    !variables.worker || (request.operation == "CREATE" ? (!variables.newDebug || variables.newDebugNonceValid) :
                    (request.operation == "DELETE" ? (!variables.oldDebug || variables.oldDebugNonceValid) :
                    ((!variables.oldDebug && !variables.newDebug) || (variables.oldDebug && variables.newDebug && variables.oldDebugNonceValid && variables.newDebugNonceValid && object.metadata.annotations["thalamus.dev/ownership-nonce"] == oldObject.metadata.annotations["thalamus.dev/ownership-nonce"]))))
                """
                message: "worker debug resources must retain their non-nil session ownership nonce"
            }]
        }
    }, {
        apiVersion: "admissionregistration.k8s.io/v1"
        kind: "ValidatingAdmissionPolicyBinding"
        metadata: {name: "live-ownership.thalamus.dev"}
        spec: {policyName: "live-ownership.thalamus.dev", validationActions: ["Deny"]}
    }, {
        # The worker may only grant the server access to one session's
        # password Secret. These grants precede the Job and have no owner refs;
        # the worker must explicitly remove them during session cleanup.
        apiVersion: "admissionregistration.k8s.io/v1"
        kind: "ValidatingAdmissionPolicy"
        metadata: {name: "live-debug-rbac.thalamus.dev"}
        spec: {
            failurePolicy: "Fail"
            matchConstraints: {
                matchPolicy: "Equivalent"
                namespaceSelector: {matchLabels: {"thalamus.dev/live-admission": "enforced"}}
                resourceRules: [{
                    apiGroups: ["rbac.authorization.k8s.io"]
                    apiVersions: ["v1"]
                    operations: ["CREATE", "UPDATE", "DELETE"]
                    resources: ["roles", "rolebindings"]
                    scope: "Namespaced"
                }]
            }
            variables: [
                {name: "worker", expression: 'request.userInfo.username == ' + json.encode("system:serviceaccount:" + systemNamespace + ":thalamus-worker")},
                {name: "target", expression: 'request.operation == "DELETE" ? oldObject : object'},
                {name: "metadataValid", expression: """
                    variables.target != null && has(variables.target.metadata) && has(variables.target.metadata.name) && has(variables.target.metadata.labels) &&
                    "app.kubernetes.io/name" in variables.target.metadata.labels && variables.target.metadata.labels["app.kubernetes.io/name"] == "thalamus-debug" &&
                    "app.kubernetes.io/managed-by" in variables.target.metadata.labels && variables.target.metadata.labels["app.kubernetes.io/managed-by"] == "thalamus" &&
                    "thalamus.dev/debug-session" in variables.target.metadata.labels &&
                    variables.target.metadata.labels["thalamus.dev/debug-session"].matches("^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$") &&
                    variables.target.metadata.name == "thalamus-debug-" + variables.target.metadata.labels["thalamus.dev/debug-session"] &&
                    (!has(variables.target.metadata.ownerReferences) || size(variables.target.metadata.ownerReferences) == 0)
                """},
                # Use dynamic field selection only to accommodate the two
                # distinct resource kinds; each branch is guarded by its kind
                # and the size checks precede the lazy indexed variables.
                {name: "rule", expression: 'dyn(variables.target).rules[0]'},
                {name: "roleValid", expression: """
                    has(dyn(variables.target).rules) && size(dyn(variables.target).rules) == 1 &&
                    has(variables.rule.apiGroups) && variables.rule.apiGroups == [""] &&
                    has(variables.rule.resources) && variables.rule.resources == ["secrets"] &&
                    has(variables.rule.resourceNames) && variables.rule.resourceNames == [variables.target.metadata.name + "-server"] &&
                    has(variables.rule.verbs) && variables.rule.verbs == ["get"] &&
                    (!has(variables.rule.nonResourceURLs) || size(variables.rule.nonResourceURLs) == 0)
                """},
                {name: "roleRef", expression: 'dyn(variables.target).roleRef'},
                {name: "subject", expression: 'dyn(variables.target).subjects[0]'},
                {name: "bindingValid", expression: """
                    has(dyn(variables.target).roleRef) && has(variables.roleRef.apiGroup) && variables.roleRef.apiGroup == "rbac.authorization.k8s.io" &&
                    has(variables.roleRef.kind) && variables.roleRef.kind == "Role" && has(variables.roleRef.name) && variables.roleRef.name == variables.target.metadata.name &&
                    has(dyn(variables.target).subjects) && size(dyn(variables.target).subjects) == 1 &&
                    has(variables.subject.kind) && variables.subject.kind == "ServiceAccount" && has(variables.subject.name) && variables.subject.name == "thalamus-server" &&
                    has(variables.subject.namespace) && variables.subject.namespace == ${json.encode(systemNamespace)} &&
                    (!has(variables.subject.apiGroup) || variables.subject.apiGroup == "")
                """},
            ]
            validations: [{
                expression: '!variables.worker || (request.operation in ["CREATE", "DELETE"] && variables.metadataValid && (request.resource.resource == "roles" ? variables.roleValid : variables.bindingValid))'
                message: "worker RBAC is limited to immutable per-session server Secret grants"
            }]
        }
    }, {
        apiVersion: "admissionregistration.k8s.io/v1"
        kind: "ValidatingAdmissionPolicyBinding"
        metadata: {name: "live-debug-rbac.thalamus.dev"}
        spec: {policyName: "live-debug-rbac.thalamus.dev", validationActions: ["Deny"]}
    }, {
        # Job/GC controllers still need their ordinary Pod permissions. This
        # policy limits only the worker's otherwise namespace-wide Pod RBAC.
        # The worker must additionally verify the persisted session nonce,
        # actual Job UID and Pod UID/resourceVersion deletion preconditions.
        apiVersion: "admissionregistration.k8s.io/v1"
        kind: "ValidatingAdmissionPolicy"
        metadata: {name: "live-debug-pods.thalamus.dev"}
        spec: {
            failurePolicy: "Fail"
            matchConstraints: {
                matchPolicy: "Equivalent"
                namespaceSelector: {matchLabels: {"thalamus.dev/live-admission": "enforced"}}
                resourceRules: [{
                    apiGroups: [""]
                    apiVersions: ["v1"]
                    operations: ["CREATE", "UPDATE", "DELETE"]
                    resources: ["pods"]
                    scope: "Namespaced"
                }]
            }
            variables: [
                {name: "worker", expression: 'request.userInfo.username == ' + json.encode("system:serviceaccount:" + systemNamespace + ":thalamus-worker")},
                {name: "session", expression: 'oldObject.metadata.labels["thalamus.dev/debug-session"]'},
            ]
            validations: [{
                expression: """
                    !variables.worker || (request.operation == "DELETE" && request.namespace == ${json.encode(workloadNamespace)} &&
                    oldObject != null && has(oldObject.metadata) && has(oldObject.metadata.name) && has(oldObject.metadata.labels) &&
                    "app.kubernetes.io/managed-by" in oldObject.metadata.labels && oldObject.metadata.labels["app.kubernetes.io/managed-by"] == "thalamus" &&
                    "app.kubernetes.io/name" in oldObject.metadata.labels && oldObject.metadata.labels["app.kubernetes.io/name"] == "thalamus-debug" &&
                    "thalamus.dev/debug-session" in oldObject.metadata.labels && variables.session.matches("^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$") &&
                    oldObject.metadata.name.startsWith("thalamus-debug-" + variables.session + "-") && size(oldObject.metadata.name) > size("thalamus-debug-" + variables.session + "-") &&
                    has(oldObject.metadata.annotations) && "thalamus.dev/ownership-nonce" in oldObject.metadata.annotations &&
                    oldObject.metadata.annotations["thalamus.dev/ownership-nonce"] != "00000000-0000-0000-0000-000000000000" &&
                    oldObject.metadata.annotations["thalamus.dev/ownership-nonce"].matches("^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$") &&
                    has(oldObject.metadata.ownerReferences) && oldObject.metadata.ownerReferences.exists(r, has(r.apiVersion) && r.apiVersion == "batch/v1" && has(r.kind) && r.kind == "Job" && has(r.name) && r.name == "thalamus-debug-" + variables.session && has(r.uid) && r.uid != "" && has(r.controller) && r.controller))
                """
                message: "worker may only delete an owned debug session Job Pod"
            }]
        }
    }, {
        apiVersion: "admissionregistration.k8s.io/v1"
        kind: "ValidatingAdmissionPolicyBinding"
        metadata: {name: "live-debug-pods.thalamus.dev"}
        spec: {policyName: "live-debug-pods.thalamus.dev", validationActions: ["Deny"]}
    }, {
        # Only the worker is constrained here. Admission on jobs/status and the
        # guarded GC finalizer exception remain separate so controllers can
        # complete and collect Jobs.
        apiVersion: "admissionregistration.k8s.io/v1"
        kind: "ValidatingAdmissionPolicy"
        metadata: {name: "live-worker-jobs.thalamus.dev"}
        spec: {
            failurePolicy: "Fail"
            matchConstraints: {
                matchPolicy: "Equivalent"
                namespaceSelector: {matchLabels: {"thalamus.dev/live-admission": "enforced"}}
                resourceRules: [{
                    apiGroups: ["batch"]
                    apiVersions: ["v1"]
                    operations: ["CREATE", "UPDATE"]
                    resources: ["jobs"]
                    scope: "Namespaced"
                }]
            }
            variables: [
                {name: "worker", expression: 'request.userInfo.username == ' + json.encode("system:serviceaccount:" + systemNamespace + ":thalamus-worker")},
                {name: "isRun", expression: 'has(object.metadata) && has(object.metadata.name) && object.metadata.name.startsWith("thalamus-run-")'},
                {name: "isDebug", expression: 'has(object.metadata) && has(object.metadata.name) && object.metadata.name.startsWith("thalamus-debug-")'},
                {name: "idKey", expression: 'variables.isRun ? "thalamus.io/run-id" : "thalamus.dev/debug-session"'},
                {name: "nonceKey", expression: 'variables.isRun ? "thalamus.io/ownership-nonce" : "thalamus.dev/ownership-nonce"'},
                {name: "sessionId", expression: 'object.metadata.labels[variables.idKey]'},
                {name: "pod", expression: 'object.spec.template.spec'},
                {name: "allContainers", expression: 'variables.pod.containers + (has(variables.pod.initContainers) ? variables.pod.initContainers : [])'},
                {name: "runnerImages", expression: json.encode(runnerImages)},
                {name: "runnerTemplates", expression: _runnerTemplateExpression},
                {name: "debugImages", expression: json.encode(debugImages)},
                {name: "identityValid", expression: """
                    (variables.isRun || variables.isDebug) && has(object.metadata.labels) && variables.idKey in object.metadata.labels &&
                    variables.sessionId.matches("^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$") &&
                    object.metadata.name == (variables.isRun ? "thalamus-run-" : "thalamus-debug-") + variables.sessionId &&
                    has(object.metadata.annotations) && variables.nonceKey in object.metadata.annotations &&
                    object.metadata.annotations[variables.nonceKey] != "00000000-0000-0000-0000-000000000000" &&
                    object.metadata.annotations[variables.nonceKey].matches("^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$") &&
                    has(object.spec) && has(object.spec.template) && has(object.spec.template.metadata) && has(object.spec.template.metadata.labels) &&
                    "app.kubernetes.io/managed-by" in object.spec.template.metadata.labels && object.spec.template.metadata.labels["app.kubernetes.io/managed-by"] == "thalamus" &&
                    variables.idKey in object.spec.template.metadata.labels && object.spec.template.metadata.labels[variables.idKey] == variables.sessionId &&
                    (variables.isRun || (has(object.spec.template.metadata.annotations) && variables.nonceKey in object.spec.template.metadata.annotations && object.spec.template.metadata.annotations[variables.nonceKey] == object.metadata.annotations[variables.nonceKey])) &&
                    has(object.spec.template.spec)
                """},
                {name: "sameOwnedJob", expression: """
                    request.operation == "CREATE" || (oldObject != null && has(oldObject.metadata) && has(oldObject.metadata.name) && oldObject.metadata.name == object.metadata.name &&
                    has(oldObject.metadata.uid) && has(object.metadata.uid) && oldObject.metadata.uid == object.metadata.uid &&
                    has(oldObject.metadata.annotations) && variables.nonceKey in oldObject.metadata.annotations && oldObject.metadata.annotations[variables.nonceKey] == object.metadata.annotations[variables.nonceKey])
                """},
                {name: "podValid", expression: """
                    has(variables.pod.serviceAccountName) && variables.pod.serviceAccountName == (variables.isRun ? "thalamus-runner" : "thalamus-debug") &&
                    has(variables.pod.automountServiceAccountToken) && !variables.pod.automountServiceAccountToken &&
                    (!has(variables.pod.hostNetwork) || !variables.pod.hostNetwork) && (!has(variables.pod.hostPID) || !variables.pod.hostPID) && (!has(variables.pod.hostIPC) || !variables.pod.hostIPC) &&
                    has(variables.pod.restartPolicy) && variables.pod.restartPolicy == "Never" && (!has(variables.pod.imagePullSecrets) || size(variables.pod.imagePullSecrets) == 0) &&
                    has(variables.pod.securityContext) && has(variables.pod.securityContext.runAsNonRoot) && variables.pod.securityContext.runAsNonRoot &&
                    (!has(variables.pod.securityContext.runAsUser) || variables.pod.securityContext.runAsUser != 0) &&
                    has(variables.pod.securityContext.seccompProfile) && has(variables.pod.securityContext.seccompProfile.type) && variables.pod.securityContext.seccompProfile.type == "RuntimeDefault" &&
                    has(variables.pod.containers) && size(variables.pod.containers) == 1 &&
                    (variables.isRun ? (variables.pod.containers[0].name == "runner" && (!has(variables.pod.initContainers) || size(variables.pod.initContainers) == 0)) :
                    (variables.pod.containers[0].name == "opencode" && has(variables.pod.initContainers) && size(variables.pod.initContainers) == 1 && variables.pod.initContainers[0].name == "checkout")) &&
                    (!has(variables.pod.ephemeralContainers) || size(variables.pod.ephemeralContainers) == 0)
                """},
                {name: "containersValid", expression: """
                    variables.allContainers.all(c, has(c.image) && c.image.matches("^.+@sha256:[0-9a-f]{64}$") &&
                    (variables.isRun ? (c.image in variables.runnerImages && has(c.command) && variables.runnerTemplates.exists(t, t.image == c.image && t.command == c.command) && (!has(c.args) || size(c.args) == 0)) : c.image in variables.debugImages) &&
                    has(c.securityContext) && (!has(c.securityContext.privileged) || !c.securityContext.privileged) &&
                    has(c.securityContext.allowPrivilegeEscalation) && !c.securityContext.allowPrivilegeEscalation &&
                    has(c.securityContext.readOnlyRootFilesystem) && c.securityContext.readOnlyRootFilesystem &&
                    (!has(c.securityContext.runAsNonRoot) || c.securityContext.runAsNonRoot) && (!has(c.securityContext.runAsUser) || c.securityContext.runAsUser != 0) &&
                    (!has(c.securityContext.seccompProfile) || (has(c.securityContext.seccompProfile.type) && c.securityContext.seccompProfile.type == "RuntimeDefault")) &&
                    has(c.securityContext.capabilities) && has(c.securityContext.capabilities.drop) && "ALL" in c.securityContext.capabilities.drop &&
                    (!has(c.securityContext.capabilities.add) || size(c.securityContext.capabilities.add) == 0) &&
                    (!has(c.envFrom) || size(c.envFrom) == 0) &&
                    (!has(c.env) || c.env.all(e, (!variables.isRun || e.name in ["THALAMUS_RUN_ID", "THALAMUS_RUN_REQUEST", "THALAMUS_S3_CA_FILE", "THALAMUS_VALIDATION_CONFIG", "THALAMUS_BENCHMARK_CONFIG", "THALAMUS_RUNNER_IMAGE", "HOME", "HF_HOME", "THALAMUS_API_KEY"]) &&
                    (!has(e.valueFrom) || !has(e.valueFrom.secretKeyRef) || (has(e.valueFrom.secretKeyRef.name) &&
                    (variables.isRun ? (e.valueFrom.secretKeyRef.name.startsWith("thalamus-") && e.valueFrom.secretKeyRef.name.endsWith("-" + variables.sessionId)) :
                    e.valueFrom.secretKeyRef.name in [object.metadata.name + "-server", object.metadata.name + "-model", object.metadata.name + "-mcp"]))))))
                """},
                # Only emptyDir and run-scoped projected Secrets are required by
                # the actual templates; this excludes hostPath, CSI and projected
                # service-account tokens rather than relying on token automount.
                {name: "volumesValid", expression: """
                    !has(variables.pod.volumes) || variables.pod.volumes.all(v, !has(v.hostPath) && (has(v.emptyDir) || has(v.projected)) &&
                    (!has(v.projected) || (variables.isRun && has(v.projected.sources) && size(v.projected.sources) > 0 &&
                    v.projected.sources.all(s, has(s.secret) && has(s.secret.name) && s.secret.name.startsWith("thalamus-") && s.secret.name.endsWith("-" + variables.sessionId)))))
                """},
            ]
            validations: [{
                expression: '!variables.worker || (request.namespace == ' + json.encode(workloadNamespace) + ' && variables.identityValid && variables.sameOwnedJob && variables.podValid && variables.containersValid && variables.volumesValid)'
                message: "worker Jobs must use approved run/debug templates, tokenless accounts and session-scoped Secrets"
            }]
        }
    }, {
        apiVersion: "admissionregistration.k8s.io/v1"
        kind: "ValidatingAdmissionPolicyBinding"
        metadata: {name: "live-worker-jobs.thalamus.dev"}
        spec: {policyName: "live-worker-jobs.thalamus.dev", validationActions: ["Deny"]}
    }, {
        apiVersion: "admissionregistration.k8s.io/v1"
        kind: "ValidatingAdmissionPolicy"
        metadata: {name: "live-status-ownership.thalamus.dev"}
        spec: {
            failurePolicy: "Fail"
            matchConstraints: {
                matchPolicy: "Equivalent"
                namespaceSelector: {matchLabels: {"thalamus.dev/live-admission": "enforced"}}
                resourceRules: [
                    {apiGroups: [""], apiVersions: ["v1"], operations: ["UPDATE"], resources: ["services/status"], scope: "Namespaced"},
                    {apiGroups: ["batch"], apiVersions: ["v1"], operations: ["UPDATE"], resources: ["jobs/status"], scope: "Namespaced"},
                ]
            }
            variables: _ownership_variables + [
                {name: "worker", expression: 'request.userInfo.username == ' + json.encode("system:serviceaccount:" + systemNamespace + ":thalamus-worker")},
                {name: "cleanupController", expression: 'request.userInfo.username in ["system:serviceaccount:kube-system:ttl-after-finished-controller", "system:serviceaccount:kube-system:generic-garbage-collector"]'},
            ]
            # Job/service controllers can update status, but no principal can
            # forge or remove ownership through a status endpoint. Finalizers
            # and server-managed bookkeeping are not ownership metadata.
            validations: [{
                expression: '(!variables.worker || variables.oldReserved) && ((!variables.oldReserved && !variables.newReserved) || (variables.oldReserved && variables.newReserved && variables.stableMetadata && !variables.cleanupController))'
                message: "status updates must preserve reserved ownership metadata"
            }]
        }
    }, {
        apiVersion: "admissionregistration.k8s.io/v1"
        kind: "ValidatingAdmissionPolicyBinding"
        metadata: {name: "live-status-ownership.thalamus.dev"}
        spec: {policyName: "live-status-ownership.thalamus.dev", validationActions: ["Deny"]}
    }, {
        apiVersion: "networking.k8s.io/v1"
        kind: "NetworkPolicy"
        metadata: {name: "live-default-deny", namespace: systemNamespace}
        spec: {podSelector: {}, policyTypes: ["Ingress", "Egress"]}
    }, {
        apiVersion: "networking.k8s.io/v1"
        kind: "NetworkPolicy"
        metadata: {name: "live-system-egress", namespace: systemNamespace}
        spec: {
            podSelector: {}
            policyTypes: ["Egress"]
            egress: [_dns_egress] + systemEgress
        }
    }, {
        apiVersion: "networking.k8s.io/v1"
        kind: "NetworkPolicy"
        metadata: {name: "live-server-access", namespace: systemNamespace}
        spec: {
            podSelector: {matchLabels: {"app.kubernetes.io/name": "thalamus-server"}}
            policyTypes: ["Ingress", "Egress"]
            ingress: [{
                from: [{namespaceSelector: {matchLabels: {"kubernetes.io/metadata.name": gatewayNamespace}}, podSelector: {matchLabels: gatewayPodLabels}}]
                ports: [{protocol: "TCP", port: 8080}]
            }, {
                from: [{podSelector: {matchLabels: {"app.kubernetes.io/name": "thalamus-worker"}}}]
                ports: [{protocol: "TCP", port: 8080}]
            }]
            egress: [{
                to: [{
                    namespaceSelector: {matchLabels: {"kubernetes.io/metadata.name": workloadNamespace}}
                    podSelector: {matchLabels: {"app.kubernetes.io/name": "thalamus-debug"}, matchExpressions: [{key: "thalamus.dev/debug-session", operator: "Exists"}]}
                }]
                ports: [{protocol: "TCP", port: 4096}]
            }]
        }
    }, {
        apiVersion: "networking.k8s.io/v1"
        kind: "NetworkPolicy"
        metadata: {name: "live-worker-server", namespace: systemNamespace}
        spec: {
            podSelector: {matchLabels: {"app.kubernetes.io/name": "thalamus-worker"}}
            policyTypes: ["Egress"]
            egress: [{
                to: [{podSelector: {matchLabels: {"app.kubernetes.io/name": "thalamus-server"}}}]
                ports: [{protocol: "TCP", port: 8080}]
            }]
        }
    }, {
        apiVersion: "networking.k8s.io/v1"
        kind: "NetworkPolicy"
        metadata: {name: "live-default-deny", namespace: workloadNamespace}
        spec: {podSelector: {}, policyTypes: ["Ingress", "Egress"]}
    }, {
        apiVersion: "networking.k8s.io/v1"
        kind: "NetworkPolicy"
        metadata: {name: "live-runner-egress", namespace: workloadNamespace}
        spec: {
            podSelector: {matchExpressions: [{key: "thalamus.io/run-id", operator: "Exists"}]}
            policyTypes: ["Egress"]
            egress: [_dns_egress] + runnerEgress
        }
    }] + workloadPolicies

    check:
        systemNamespace != "" and workloadNamespace != "" and gatewayNamespace != "", "system, workload and gateway namespaces are required"
        systemNamespace != workloadNamespace, "system and workload namespaces must be distinct"
        len(gatewayPodLabels) > 0, "gateway pod selector must not select every pod"
        "system:masters" in adminGroups, "system:masters must retain the emergency override"
