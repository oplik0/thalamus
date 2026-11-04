# Install administrator-verified, pinned Flux source-controller and
# kustomize-controller versions and their v1 CRDs separately. The controllers
# must permit cross-namespace source and dependency references. Provision the
# system/workload namespaces and the optional Flux HTTPS Secret separately.
#
# CatalogSource belongs in the administrator-controlled system namespace. It
# follows the explicitly supplied catalog ref as mutable desired state; its
# artifact status records the observed commit. This is distinct from the
# application's pinned checkout, and neither proves they are at the same
# revision. Protect the source, Secret, ref and generated pool inputs with
# administrator review. Both reconcilers consume this one source artifact.
#
# The workload reconciler applies only rendered/inference using its namespaced
# account. The pool reconciler separately applies rendered/pools as an
# administrator-authorized cluster-scoped operation. Protect both Flux objects
# and RBAC; Flux does not render KCL and List application is not atomic.
#
# The optional HTTPS Secret already exists in the system namespace, with
# username and password keys, and is read by the Flux source-controller. It is
# never granted to either reconciler. The controller must be permitted to
# impersonate the dedicated service accounts. Do not give those accounts or
# their groups additional bindings: a namespace Role cannot revoke privileges
# granted elsewhere. Use one workload bundle per namespace.
#
# The workload reconciler is not an admission administrator. It remains subject
# to admission reservation of thalamus-* names and app.kubernetes.io/managed-by=
# thalamus on covered resources. Install and protect that enforcement. Its Role
# grants no direct Secret or RBAC access. Even serving resources can indirectly
# create workloads and expose credentials; review resource families, network
# policies and admission.
import net
import regex

_dns_label = r"^[a-z0-9]([-a-z0-9]*[a-z0-9])?$"
_dns_subdomain = r"^[a-z0-9]([-a-z0-9]*[a-z0-9])?(\.[a-z0-9]([-a-z0-9]*[a-z0-9])?)*$"
_verbs = ["get", "list", "watch", "create", "update", "patch", "delete"]

# Require explicit resource families and verbs. Wildcards, Secrets, RBAC, and
# impersonate/bind/escalate verbs cannot be added through extraRules.
# resourceNames is optional and has Kubernetes' usual limitations for create.
schema _ExtraRule:
    apiGroups: [str]
    resources: [str]
    verbs: [str]
    resourceNames?: [str]
    check:
        len(apiGroups) > 0 and (all group in apiGroups {group != "*" and group != "rbac.authorization.k8s.io"}), "extra rules require explicit non-RBAC API groups"
        len(resources) > 0 and (all resource in resources {regex.match(resource, r"^[a-z0-9][a-z0-9.-]*(/[a-z0-9][a-z0-9.-]*)?$") and resource.split("/")[0] not in ["secrets", "roles", "rolebindings", "clusterroles", "clusterrolebindings"]}), "extra rules require explicit non-Secret, non-RBAC resources"
        len(verbs) > 0 and (all verb in verbs {verb != "" and "*" not in verb and verb not in ["impersonate", "bind", "escalate"]}), "extra rules require explicit verbs other than impersonate, bind, or escalate"

schema CatalogSource:
    # This namespace and the source object must remain administrator-controlled.
    namespace: str
    # HTTPS without userinfo, query or fragment. Authentication is only by Secret.
    url: str
    # An explicit refs/heads/... or refs/tags/... ref, shared with the checkout.
    ref: str
    sourceName: str = "catalog-source"
    # Existing system-namespace Flux HTTPS Secret, or empty for public access.
    secretName: str = ""

    _sourceNameParts = sourceName.split(".")
    _secretNameParts = secretName.split(".")
    _refParts = ref.split("/")
    _authority = url[8:].split("/")[0]
    _host = _authority.split("]")[0][1:] if _authority.startswith("[") else _authority.split(":")[0]
    _portSuffix = (_authority.split("]")[1] if len(_authority.split("]")) == 2 else "") if _authority.startswith("[") else _authority[len(_host):]
    _port = _portSuffix[1:] if _portSuffix.startswith(":") else ""

    items: [any] = [{
        apiVersion: "source.toolkit.fluxcd.io/v1"
        kind: "GitRepository"
        metadata: {name: sourceName, namespace: namespace}
        spec: {
            interval: "5m"
            url: url
            ref: {name: ref}
            if secretName != "":
                secretRef: {name: secretName}
        }
    }, {
        # Runtime provenance must follow the cross-namespace source, without
        # access to other repositories or to the source credential.
        apiVersion: "rbac.authorization.k8s.io/v1"
        kind: "Role"
        metadata: {name: "catalog-source-observer", namespace: namespace}
        rules: [{apiGroups: ["source.toolkit.fluxcd.io"], resources: ["gitrepositories"], resourceNames: [sourceName], verbs: ["get"]}]
    }, {
        apiVersion: "rbac.authorization.k8s.io/v1"
        kind: "RoleBinding"
        metadata: {name: "catalog-source-observer", namespace: namespace}
        roleRef: {apiGroup: "rbac.authorization.k8s.io", kind: "Role", name: "catalog-source-observer"}
        subjects: [
            {kind: "ServiceAccount", name: "thalamus-server", namespace: namespace},
            {kind: "ServiceAccount", name: "thalamus-worker", namespace: namespace},
        ]
    }]

    check:
        len(namespace) <= 63 and regex.match(namespace, _dns_label), "source namespace must be a Kubernetes DNS label"
        len(sourceName) <= 253 and regex.match(sourceName, _dns_subdomain) and (all part in _sourceNameParts {len(part) <= 63}), "sourceName must be a Kubernetes DNS subdomain"
        secretName == "" or (len(secretName) <= 253 and regex.match(secretName, _dns_subdomain) and (all part in _secretNameParts {len(part) <= 63})), "secretName must be empty or a Kubernetes DNS subdomain"
        regex.match(url, r"^https://([A-Za-z0-9]([A-Za-z0-9-]*[A-Za-z0-9])?(\.[A-Za-z0-9]([A-Za-z0-9-]*[A-Za-z0-9])?)*\.?|\[[0-9A-Fa-f:.]+\])(:[0-9]{1,5})?(/([A-Za-z0-9._~!$&'()*+,;=:@/-]|%[0-9A-Fa-f]{2})*)?$"), "url must be credential-free HTTPS with no query or fragment"
        not _authority.startswith("[") or net.is_IPv6(_host), "bracketed URL host must be a valid IPv6 address"
        _port == "" or (regex.match(_port, r"^[0-9]{1,5}$") and int(_port) > 0 and int(_port) <= 65535), "URL port must be between 1 and 65535"
        regex.match(ref, r"^refs/(heads|tags)/[^\x00-\x20\x7f~^:?*\[\\]+$") and not ref.endswith(".") and ".." not in ref and "@{" not in ref and (all part in _refParts {part != "" and not part.startswith(".") and not part.endswith(".lock")}), "ref must be a valid full Git branch or tag ref"

schema GitOps:
    namespace: str
    # Reference the administrator-owned source, never a workload-owned source.
    sourceNamespace: str
    sourceName: str = "catalog-source"
    reconcilerName: str = "catalog-sync"
    # When configured, wait for the pool reconciliation of this same source.
    poolDependencyName: str = ""
    poolDependencyNamespace: str = ""
    # Kubernetes PolicyRules for operator-reviewed optional resource families.
    extraRules: [any] = []

    _extraRules: [_ExtraRule] = [_ExtraRule {**rule} for rule in extraRules]
    _sourceNameParts = sourceName.split(".")
    _reconcilerNameParts = reconcilerName.split(".")
    _poolDependencyNameParts = poolDependencyName.split(".")

    items: [any] = [{
        apiVersion: "v1"
        kind: "ServiceAccount"
        metadata: {name: "catalog-reconciler", namespace: namespace}
        # Flux uses impersonation rather than a Pod with this account's token.
        automountServiceAccountToken: False
    }, {
        apiVersion: "rbac.authorization.k8s.io/v1"
        kind: "Role"
        metadata: {name: "catalog-reconciler", namespace: namespace}
        rules: [
            {apiGroups: ["serving.kserve.io"], resources: ["llminferenceservices"], verbs: _verbs},
            {apiGroups: ["agentgateway.dev"], resources: ["agentgatewaybackends", "agentgatewaypolicies"], verbs: _verbs},
            {apiGroups: ["gateway.networking.k8s.io"], resources: ["httproutes"], verbs: _verbs},
            {apiGroups: [""], resources: ["configmaps", "services"], verbs: _verbs},
            {apiGroups: ["networking.k8s.io"], resources: ["networkpolicies"], verbs: _verbs},
        ] + _extraRules
    }, {
        apiVersion: "rbac.authorization.k8s.io/v1"
        kind: "RoleBinding"
        metadata: {name: "catalog-reconciler", namespace: namespace}
        roleRef: {apiGroup: "rbac.authorization.k8s.io", kind: "Role", name: "catalog-reconciler"}
        subjects: [{kind: "ServiceAccount", name: "catalog-reconciler", namespace: namespace}]
    }, {
        apiVersion: "kustomize.toolkit.fluxcd.io/v1"
        kind: "Kustomization"
        metadata: {name: reconcilerName, namespace: namespace}
        spec: {
            interval: "5m"
            sourceRef: {kind: "GitRepository", name: sourceName, namespace: sourceNamespace}
            path: "rendered/inference"
            if poolDependencyName != "":
                dependsOn: [{name: poolDependencyName, namespace: poolDependencyNamespace}]
            targetNamespace: namespace
            serviceAccountName: "catalog-reconciler"
            prune: True
            wait: True
        }
    }]

    check:
        len(namespace) <= 63 and regex.match(namespace, _dns_label), "namespace must be a Kubernetes DNS label"
        len(sourceNamespace) <= 63 and regex.match(sourceNamespace, _dns_label) and sourceNamespace != namespace, "source must be in a distinct administrator-controlled namespace"
        len(sourceName) <= 253 and regex.match(sourceName, _dns_subdomain) and (all part in _sourceNameParts {len(part) <= 63}), "sourceName must be a Kubernetes DNS subdomain"
        len(reconcilerName) <= 253 and regex.match(reconcilerName, _dns_subdomain) and (all part in _reconcilerNameParts {len(part) <= 63}), "reconcilerName must be a Kubernetes DNS subdomain"
        (poolDependencyName == "") == (poolDependencyNamespace == ""), "pool dependency needs both a name and namespace"
        poolDependencyName == "" or (len(poolDependencyName) <= 253 and regex.match(poolDependencyName, _dns_subdomain) and (all part in _poolDependencyNameParts {len(part) <= 63}) and len(poolDependencyNamespace) <= 63 and regex.match(poolDependencyNamespace, _dns_label)), "pool dependency must have a valid name and namespace"
