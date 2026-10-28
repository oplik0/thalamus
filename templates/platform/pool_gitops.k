# This is a separate administrator-owned reconciliation path for the pool's
# cluster-scoped ResourceFlavor, ClusterQueue and PriorityClass objects. Install
# administrator-verified, pinned Flux controllers and their v1 CRDs, Kueue, and
# the system namespace separately. This template installs none of them.
#
# The source is the shared, administrator-controlled catalog GitRepository in
# the system namespace. Flux applies rendered/pools from that same artifact; it
# does not render KCL. Protect the source and ref, and require administrator
# approval for pool catalog, rendered pool output, and referenced Kustomize
# inputs. Ordinary catalog writers must not be able to redirect this source or
# authorize cluster-scoped changes. Pruning can delete pool resources removed
# from the ref or when this Kustomization is deleted.
#
# The kustomize-controller must be allowed to impersonate the dedicated account.
# Protect this Kustomization, service account, and RBAC from untrusted changes,
# and do not grant the account or its groups additional permissions. The
# ClusterRole allows writes to *all* objects in these three cluster-scoped
# resource families, not only this pool's names; RBAC cannot restrict create by
# name. An admission policy is needed if narrower name-level isolation is wanted.
# One bundle may be installed per cluster because the role names are fixed.
import regex

_pool_dns_label = r"^[a-z0-9]([-a-z0-9]*[a-z0-9])?$"
_pool_dns_subdomain = r"^[a-z0-9]([-a-z0-9]*[a-z0-9])?(\.[a-z0-9]([-a-z0-9]*[a-z0-9])?)*$"
_pool_verbs = ["get", "list", "watch", "create", "update", "patch", "delete"]

schema PoolGitOps:
    systemNamespace: str
    # Reference the protected system-namespace catalog GitRepository.
    sourceName: str = "catalog-source"
    reconcilerName: str = "pool-sync"

    _sourceNameParts = sourceName.split(".")
    _reconcilerNameParts = reconcilerName.split(".")

    items: [any] = [{
        apiVersion: "v1"
        kind: "ServiceAccount"
        metadata: {name: "pool-reconciler", namespace: systemNamespace}
        # The Flux controller impersonates this account; it needs no Pod token.
        automountServiceAccountToken: False
    }, {
        apiVersion: "rbac.authorization.k8s.io/v1"
        kind: "ClusterRole"
        metadata: {name: "pool-reconciler"}
        rules: [
            {apiGroups: ["kueue.x-k8s.io"], resources: ["resourceflavors", "clusterqueues"], verbs: _pool_verbs},
            {apiGroups: ["scheduling.k8s.io"], resources: ["priorityclasses"], verbs: _pool_verbs},
        ]
    }, {
        apiVersion: "rbac.authorization.k8s.io/v1"
        kind: "ClusterRoleBinding"
        metadata: {name: "pool-reconciler"}
        roleRef: {apiGroup: "rbac.authorization.k8s.io", kind: "ClusterRole", name: "pool-reconciler"}
        subjects: [{kind: "ServiceAccount", name: "pool-reconciler", namespace: systemNamespace}]
    }, {
        apiVersion: "kustomize.toolkit.fluxcd.io/v1"
        kind: "Kustomization"
        metadata: {name: reconcilerName, namespace: systemNamespace}
        spec: {
            interval: "5m"
            sourceRef: {kind: "GitRepository", name: sourceName, namespace: systemNamespace}
            path: "rendered/pools"
            # No targetNamespace: this path contains cluster-scoped resources.
            serviceAccountName: "pool-reconciler"
            prune: True
            wait: True
        }
    }]

    check:
        len(systemNamespace) <= 63 and regex.match(systemNamespace, _pool_dns_label), "systemNamespace must be a Kubernetes DNS label"
        len(sourceName) <= 253 and regex.match(sourceName, _pool_dns_subdomain) and (all part in _sourceNameParts {len(part) <= 63}), "sourceName must be a Kubernetes DNS subdomain"
        len(reconcilerName) <= 253 and regex.match(reconcilerName, _pool_dns_subdomain) and (all part in _reconcilerNameParts {len(part) <= 63}), "reconcilerName must be a Kubernetes DNS subdomain"
