# Disposable admission/Flux probe, not the complete application installer.
import templates.platform.rbac as rbac
import templates.platform.security as security
import templates.platform.gitops as gitops
import templates.platform.pool_gitops as pool_gitops

_system = "thalamus-system"
_workload = "thalamus-inference"
_redis = "docker.io/library/redis@sha256:236e397c1d5ab7a94adaf1a51eec3ca8333b05fafcd6d423c6c7cc5987e519a0"
_rbac = rbac.Rbac {
    systemNamespace = _system
    workloadNamespace = _workload
    projectionName = "thalamus-keys"
    leaseName = "thalamus-worker-lease"
}
_security = security.Security {
    systemNamespace = _system
    workloadNamespace = _workload
    gatewayNamespace = _workload
    gatewayPodLabels = {"gateway.networking.k8s.io/gateway-name": "inference-serving"}
    runnerImages = [_redis]
    runnerTemplates = [{image: _redis, command: ["redis-server", "--version"]}]
}
_source = gitops.CatalogSource {
    namespace = _system
    url = "https://github.com/oplik0/thalamus.git"
    ref = "refs/heads/qa/k3d-bcc5c4c2"
}
_catalog = gitops.GitOps {
    namespace = _workload
    sourceNamespace = _system
    poolDependencyName = "pool-sync"
    poolDependencyNamespace = _system
}
_pool = pool_gitops.PoolGitOps {systemNamespace = _system}
apiVersion = "v1"
kind = "List"
items = [
    {apiVersion: "v1", kind: "Namespace", metadata: {name: _system}},
    {apiVersion: "v1", kind: "Namespace", metadata: {name: _workload, labels: {"thalamus.dev/live-admission": "enforced"}}},
] + _rbac.items + _security.items + _source.items + _catalog.items + _pool.items
