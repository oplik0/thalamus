# Run from the repository root: kcl test ./templates/platform
# These fixtures check desired resources; site credentials and Flux installation
# are independent administrator responsibilities.
import runtime

_catalog_source_fixture = CatalogSource {
    namespace: "thalamus-system"
    url: "https://git.example.invalid/team/catalog.git"
    ref: "refs/heads/reviewed"
    sourceName: "shared-catalog"
    secretName: "flux-readonly"
}
_catalog_workload_fixture = GitOps {
    namespace: "thalamus-inference"
    sourceNamespace: "thalamus-system"
    sourceName: "shared-catalog"
    reconcilerName: "inference-sync"
    poolDependencyName: "pool-sync"
    poolDependencyNamespace: "thalamus-system"
}
_catalog_pool_fixture = PoolGitOps {
    systemNamespace: "thalamus-system"
    sourceName: "shared-catalog"
    reconcilerName: "pool-sync"
}

_catalog_find = lambda items: [any], kind: str, name: str -> any {
    matches = [item for item in items if item.kind == kind and item.metadata.name == name]
    assert len(matches) == 1, "expected one catalog resource"
    matches[0]
}

test_catalog_reconcilers_share_protected_source = lambda {
    source = _catalog_find(_catalog_source_fixture.items, "GitRepository", "shared-catalog")
    inference = _catalog_find(_catalog_workload_fixture.items, "Kustomization", "inference-sync")
    pools = _catalog_find(_catalog_pool_fixture.items, "Kustomization", "pool-sync")
    assert source.metadata.namespace == "thalamus-system"
    assert source.spec.url == "https://git.example.invalid/team/catalog.git"
    assert source.spec.ref == {name: "refs/heads/reviewed"}
    assert source.spec.secretRef.name == "flux-readonly"
    assert inference.spec.sourceRef == {kind: "GitRepository", name: source.metadata.name, namespace: source.metadata.namespace}
    assert pools.spec.sourceRef == inference.spec.sourceRef
    assert inference.spec.path == "rendered/inference" and pools.spec.path == "rendered/pools"
    assert inference.spec.dependsOn == [{name: pools.metadata.name, namespace: pools.metadata.namespace}]
    assert inference.metadata.namespace == "thalamus-inference"
    assert inference.spec.targetNamespace == "thalamus-inference"
    assert inference.spec.serviceAccountName == "catalog-reconciler"
    assert pools.metadata.namespace == "thalamus-system"
    assert pools.spec.serviceAccountName == "pool-reconciler"
    assert "targetNamespace" not in pools.spec
    assert inference.spec.prune and inference.spec.wait and pools.spec.prune and pools.spec.wait
    assert len([item for item in _catalog_workload_fixture.items + _catalog_pool_fixture.items if item.kind == "GitRepository"]) == 0
}

test_catalog_source_read_access_is_name_scoped = lambda {
    role = _catalog_find(_catalog_source_fixture.items, "Role", "catalog-source-observer")
    binding = _catalog_find(_catalog_source_fixture.items, "RoleBinding", "catalog-source-observer")
    assert role.metadata.namespace == "thalamus-system"
    assert role.rules == [{apiGroups: ["source.toolkit.fluxcd.io"], resources: ["gitrepositories"], resourceNames: ["shared-catalog"], verbs: ["get"]}]
    assert binding.roleRef.name == role.metadata.name
    assert binding.subjects == [
        {kind: "ServiceAccount", name: "thalamus-server", namespace: "thalamus-system"},
        {kind: "ServiceAccount", name: "thalamus-worker", namespace: "thalamus-system"},
    ]
}

test_catalog_and_pool_authority_are_separate = lambda {
    workloadSA = _catalog_find(_catalog_workload_fixture.items, "ServiceAccount", "catalog-reconciler")
    workloadRole = _catalog_find(_catalog_workload_fixture.items, "Role", "catalog-reconciler")
    poolSA = _catalog_find(_catalog_pool_fixture.items, "ServiceAccount", "pool-reconciler")
    poolRole = _catalog_find(_catalog_pool_fixture.items, "ClusterRole", "pool-reconciler")
    poolBinding = _catalog_find(_catalog_pool_fixture.items, "ClusterRoleBinding", "pool-reconciler")
    assert not workloadSA.automountServiceAccountToken and not poolSA.automountServiceAccountToken
    assert workloadSA.metadata.namespace == "thalamus-inference" and poolSA.metadata.namespace == "thalamus-system"
    assert all rule in workloadRole.rules {"rbac.authorization.k8s.io" not in rule.apiGroups and "secrets" not in rule.resources and "clusterqueues" not in rule.resources and "resourceflavors" not in rule.resources and "priorityclasses" not in rule.resources}
    assert poolRole.rules == [
        {apiGroups: ["kueue.x-k8s.io"], resources: ["resourceflavors", "clusterqueues"], verbs: ["get", "list", "watch", "create", "update", "patch", "delete"]},
        {apiGroups: ["scheduling.k8s.io"], resources: ["priorityclasses"], verbs: ["get", "list", "watch", "create", "update", "patch", "delete"]},
    ]
    assert poolBinding.roleRef.name == "pool-reconciler"
    assert poolBinding.subjects == [{kind: "ServiceAccount", name: "pool-reconciler", namespace: "thalamus-system"}]
}

test_catalog_source_supports_tags_and_optional_credentials = lambda {
    source = CatalogSource {namespace: "thalamus-system", url: "https://git.example.invalid/catalog.git", ref: "refs/tags/release-1"}
    repository = _catalog_find(source.items, "GitRepository", "catalog-source")
    inference = GitOps {namespace: "thalamus-inference", sourceNamespace: "thalamus-system"}
    reconciler = _catalog_find(inference.items, "Kustomization", "catalog-sync")
    assert repository.spec.ref.name == "refs/tags/release-1"
    assert "secretRef" not in repository.spec and "dependsOn" not in reconciler.spec
}

test_catalog_source_and_workload_reject_unsafe_inputs = lambda {
    invalidRef = runtime.catch(lambda { source = CatalogSource {namespace: "thalamus-system", url: "https://git.example.invalid/catalog.git", ref: "refs/heads/../bad"}.items })
    embeddedCredential = runtime.catch(lambda { source = CatalogSource {namespace: "thalamus-system", url: "https://user:password@git.example.invalid/catalog.git", ref: "refs/heads/main"}.items })
    tenantSource = runtime.catch(lambda { workload = GitOps {namespace: "thalamus-inference", sourceNamespace: "thalamus-inference"}.items })
    missingDependencyNamespace = runtime.catch(lambda { workload = GitOps {namespace: "thalamus-inference", sourceNamespace: "thalamus-system", poolDependencyName: "pool-sync"}.items })
    secretRule = runtime.catch(lambda { workload = GitOps {namespace: "thalamus-inference", sourceNamespace: "thalamus-system", extraRules: [{apiGroups: [""], resources: ["secrets"], verbs: ["get"]}]}.items })
    assert invalidRef != Undefined and embeddedCredential != Undefined and tenantSource != Undefined
    assert missingDependencyNamespace != Undefined and secretRule != Undefined
}
