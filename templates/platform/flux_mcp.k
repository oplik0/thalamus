# Optional Flux MCP deployment. Install the pinned Flux source-controller and
# helm-controller and their OCIRepository/HelmRelease CRDs separately. All
# configured namespaces must already exist. Apply RBAC and NetworkPolicies
# before enabling the HelmRelease; a Kubernetes List is not an atomic rollout.
# The release waits for the OCIRepository artifact. The upstream chart contract:
# https://github.com/controlplaneio-fluxcd/charts/tree/v0.17.1/charts/flux-operator-mcp
# Chart 0.62.1 labels its pods app.kubernetes.io/name=flux-operator-mcp and
# exposes http://flux-operator-mcp.<namespace>.svc:9090/mcp. Its image is
# <imageRepository>:v0.62.1@sha256:<imageDigest>; the administrator must verify
# that the supplied digest is the desired upstream image. The chart tag is not
# a cryptographic pin of the OCI chart artifact.
#
# HTTPRoute get/list is restricted to the workload namespace. When that
# namespace is listed in observationNamespaces, Flux object get permissions
# also apply there; an empty list omits Flux object access. No access is granted
# to other namespaces. Trace requires Flux ownership labels on the HTTPRoute
# and can fail closed for cross-namespace reconcilers or sources. Namespaced
# roles cannot trace arbitrary pod/controller owners, and no Secret or write
# access is granted. Flux object specs may themselves contain sensitive values;
# protect this Role and service-account binding.
#
# NetworkPolicies are additive, so other policies can widen either MCP ingress
# or egress (including apiEgress). Administrators must review the supplied native
# API egress rules and all other policies, protect the trusted namespace/pod
# selectors, and use a CNI that enforces NetworkPolicy. Host-network and node
# traffic have CNI-dependent limitations. Debug Jobs are never directly allowed.
import regex

_dns_label = r"^[a-z0-9]([-a-z0-9]*[a-z0-9])?$"
_image_repository = r"^[a-z0-9]([-a-z0-9]*[a-z0-9])?(\.[a-z0-9]([-a-z0-9]*[a-z0-9])?)*(:[0-9]+)?/[a-z0-9]+(([._]|__|-+)[a-z0-9]+)*(/[a-z0-9]+(([._]|__|-+)[a-z0-9]+)*)*$"

_flux_rules = [
    {apiGroups: ["kustomize.toolkit.fluxcd.io"], resources: ["kustomizations"], verbs: ["get"]},
    {apiGroups: ["helm.toolkit.fluxcd.io"], resources: ["helmreleases"], verbs: ["get"]},
    {apiGroups: ["source.toolkit.fluxcd.io"], resources: ["gitrepositories", "ocirepositories", "buckets", "helmrepositories", "helmcharts", "externalartifacts"], verbs: ["get"]},
    {apiGroups: ["fluxcd.controlplane.io"], resources: ["resourcesets", "fluxinstances"], verbs: ["get"]},
    {apiGroups: ["source.extensions.fluxcd.io"], resources: ["artifactgenerators"], verbs: ["get"]},
]

schema FluxMcp:
    namespace: str
    systemNamespace: str
    workloadNamespace: str
    # Empty, or exactly the workload namespace; never a cross-namespace grant.
    observationNamespaces: [str]
    # Native NetworkPolicy egress rules to the actual Kubernetes API endpoints.
    apiEgress: [any]
    # Registry/repository without an embedded image tag or digest.
    imageRepository: str
    imageDigest: str

    items: [any] = [{
        apiVersion: "rbac.authorization.k8s.io/v1"
        kind: "Role"
        metadata: {name: "flux-operator-mcp", namespace: workloadNamespace}
        rules: [
            {apiGroups: ["gateway.networking.k8s.io"], resources: ["httproutes"], verbs: ["get", "list"]},
        ] + (_flux_rules if workloadNamespace in observationNamespaces else [])
    }, {
        apiVersion: "rbac.authorization.k8s.io/v1"
        kind: "RoleBinding"
        metadata: {name: "flux-operator-mcp", namespace: workloadNamespace}
        roleRef: {apiGroup: "rbac.authorization.k8s.io", kind: "Role", name: "flux-operator-mcp"}
        subjects: [{kind: "ServiceAccount", name: "flux-operator-mcp", namespace: namespace}]
    }, {
        apiVersion: "networking.k8s.io/v1"
        kind: "NetworkPolicy"
        metadata: {name: "flux-operator-mcp", namespace: namespace}
        spec: {
            podSelector: {matchLabels: {"app.kubernetes.io/name": "flux-operator-mcp"}}
            policyTypes: ["Ingress", "Egress"]
            ingress: [{
                from: [{
                    namespaceSelector: {matchLabels: {"kubernetes.io/metadata.name": systemNamespace}}
                    podSelector: {matchLabels: {"app.kubernetes.io/name": "thalamus-server"}}
                }]
                ports: [{protocol: "TCP", port: 9090}]
            }]
            egress: [{
                to: [{
                    namespaceSelector: {matchLabels: {"kubernetes.io/metadata.name": "kube-system"}}
                    podSelector: {matchLabels: {"k8s-app": "kube-dns"}}
                }]
                ports: [{protocol: "UDP", port: 53}, {protocol: "TCP", port: 53}]
            }] + apiEgress
        }
    }, {
        # The system namespace may otherwise deny the server's outbound traffic.
        apiVersion: "networking.k8s.io/v1"
        kind: "NetworkPolicy"
        metadata: {name: "thalamus-server-flux-mcp", namespace: systemNamespace}
        spec: {
            podSelector: {matchLabels: {"app.kubernetes.io/name": "thalamus-server"}}
            policyTypes: ["Egress"]
            egress: [{
                to: [{
                    namespaceSelector: {matchLabels: {"kubernetes.io/metadata.name": namespace}}
                    podSelector: {matchLabels: {"app.kubernetes.io/name": "flux-operator-mcp"}}
                }]
                ports: [{protocol: "TCP", port: 9090}]
            }]
        }
    }, {
        apiVersion: "source.toolkit.fluxcd.io/v1"
        kind: "OCIRepository"
        metadata: {name: "flux-operator-mcp", namespace: namespace}
        spec: {
            interval: "60m"
            url: "oci://ghcr.io/controlplaneio-fluxcd/charts/flux-operator-mcp"
            ref: {tag: "0.62.1"}
            layerSelector: {mediaType: "application/vnd.cncf.helm.chart.content.v1.tar+gzip", operation: "copy"}
        }
    }, {
        apiVersion: "helm.toolkit.fluxcd.io/v2"
        kind: "HelmRelease"
        metadata: {name: "flux-operator-mcp", namespace: namespace}
        spec: {
            interval: "30m"
            chartRef: {kind: "OCIRepository", name: "flux-operator-mcp"}
            values: {
                transport: "http"
                readonly: True
                rbac: {create: False}
                serviceAccount: {create: True, name: "flux-operator-mcp"}
                networkPolicy: {create: False}
                image: {repository: imageRepository, tag: "v0.62.1@sha256:${imageDigest}"}
            }
        }
    }]

    check:
        len(namespace) <= 63 and regex.match(namespace, _dns_label), "MCP namespace must be a Kubernetes DNS label"
        len(systemNamespace) <= 63 and regex.match(systemNamespace, _dns_label), "system namespace must be a Kubernetes DNS label"
        len(workloadNamespace) <= 63 and regex.match(workloadNamespace, _dns_label), "workload namespace must be a Kubernetes DNS label"
        namespace != systemNamespace and namespace != workloadNamespace and systemNamespace != workloadNamespace, "MCP, system and workload namespaces must be distinct"
        len(observationNamespaces) <= 1 and (all n in observationNamespaces {n == workloadNamespace}), "MCP observations must be limited to the workload namespace"
        len(apiEgress) > 0, "at least one Kubernetes API egress rule is required"
        regex.match(imageRepository, _image_repository), "image repository must be a registry/repository without tag or digest"
        regex.match(imageDigest, r"^[0-9a-f]{64}$"), "image digest must be 64 lowercase sha256 hex digits"
