# Per-deployment serving NetworkPolicies for the managed Gateway, model and
# endpoint picker. These must be applied by trusted Git/Flux after the platform
# namespace default-deny policies and admission are installed. A Kubernetes
# List is not an atomic security transition. Policies are additive: unrelated
# allowances, host-network traffic, node-local services, pod/namespace label
# spoofing, and a CNI that does not enforce policies are outside this renderer.
# Protect all configured selectors, CIDRs and catalog changes through reviewed
# administrator-controlled admission and Git projection.
#
# KServe v0.21.0's exact workload and SchedulerLabels, and scheduler ports:
# https://github.com/kserve/kserve/blob/d1482554fc4f66dd41aee70e01f5174e24f265bd/pkg/controller/v1alpha2/llmisvc/workload_single_node.go
# https://github.com/kserve/kserve/blob/d1482554fc4f66dd41aee70e01f5174e24f265bd/pkg/controller/v1alpha2/llmisvc/scheduler.go
# https://github.com/kserve/kserve/blob/d1482554fc4f66dd41aee70e01f5174e24f265bd/config/llmisvcconfig/config-llm-scheduler.yaml
#
# Prometheus and DNS can be in different namespaces. If isolated, Prometheus
# needs operator-managed egress to the model/EPP and DNS needs ingress from the
# serving Pods on TCP/UDP 53, plus any recursive egress it requires.
# The Gateway's platform policy must independently allow xDS, RLS and DNS. If a
# cluster uses node-local DNS, host-network peers, service DNAT or a different
# health-probe path, the operator must supply and verify the additional policies
# supported by that CNI. API CIDRs must cover the addresses actually evaluated by
# that CNI, including the API Service or post-DNAT endpoints as appropriate.
#
# NetworkPolicy is Pod-wide, not container- or URL-scoped: storage initializer
# egress is also available to its model container; Prometheus or EPP allowed to
# scrape port 8000 can reach the inference API on that port. All are trusted
# infrastructure peers. Public endpoints must remain accessible only through the
# authenticated Gateway, with TLS/network controls and protected credentials.

_network_model_labels = lambda d: any, isRaw: bool -> {str:str} {
    {"thalamus.ai/deployment": d.name} if isRaw else {
        "app.kubernetes.io/name": d.name
        "app.kubernetes.io/part-of": "llminferenceservice"
        "app.kubernetes.io/component": "llminferenceservice-workload"
        "kserve.io/component": "workload"
    }
}

_network_picker_labels = lambda d: any, isRaw: bool -> {str:str} {
    {"app.kubernetes.io/name": "${d.name}-epp"} if isRaw else {
        "app.kubernetes.io/name": d.name
        "app.kubernetes.io/part-of": "llminferenceservice"
        "app.kubernetes.io/component": "llminferenceservice-router-scheduler"
    }
}

_network_peer = lambda namespaceLabels: {str:str}, podLabels: {str:str} -> any {
    {namespaceSelector: {matchLabels: namespaceLabels}, podSelector: {matchLabels: podLabels}}
}

_network_dns_egress = lambda peer: any -> any {
    {
        to: [_network_peer(peer.namespaceSelector, peer.podSelector)]
        ports: [{protocol: "UDP", port: 53}, {protocol: "TCP", port: 53}]
    }
}

_network_destination_egress = lambda destinations: [NetworkDestination] -> [any] {
    [{to: [{ipBlock: {cidr: destination.cidr}}], ports: [{protocol: "TCP", port: destinationPort} for destinationPort in destination.ports]} for destination in destinations]
}

_network_deployment = lambda service: Service, d: Deployment, artifact: Artifact, resolvedPool: any -> [any] {
    isRaw = d.rolloutStrategy == "Recreate" or resolvedPool != Undefined
    modelLabels = _network_model_labels(d, isRaw)
    pickerLabels = _network_picker_labels(d, isRaw)
    gatewayLabels = service.network.gatewayPodSelector
    gatewayPeer = _network_peer({"kubernetes.io/metadata.name": service.namespace}, gatewayLabels)
    # The private Gateway is in this workload namespace and has a separately
    # reviewed Pod selector; no policy is written into another namespace.
    privatePeers = ([_network_peer({"kubernetes.io/metadata.name": service.namespace}, service.agentic.internalGatewayPodLabels)] if service.agentic != Undefined else [])
    prometheusPeer = _network_peer(service.network.prometheus.namespaceSelector, service.network.prometheus.podSelector)
    dnsEgress = _network_dns_egress(service.network.dns)
    # PVC and OCI modelcar/native volumes are mounted by Kubernetes, not
    # downloaded by the Pod. Every other nonpackaged URI is treated as remote.
    localStorage = artifact.uri.startswith("pvc://") or artifact.uri.startswith("oci://") or artifact.uri.startswith("oci+native://")
    remoteStorage = not d.packagedModel and not localStorage
    assert not remoteStorage or len(service.network.storageEgress) > 0, "remote model storage requires reviewed CIDR and port egress"
    storageRules = ([dnsEgress] + _network_destination_egress(service.network.storageEgress) if remoteStorage else [])
    [
        {
            apiVersion: "networking.k8s.io/v1"
            kind: "NetworkPolicy"
            metadata: {name: "serving-${d.name}-model", namespace: service.namespace}
            spec: {
                podSelector: {matchLabels: modelLabels}
                policyTypes: ["Ingress", "Egress"]
                ingress: [{from: [gatewayPeer, {podSelector: {matchLabels: pickerLabels}}, prometheusPeer] + privatePeers, ports: [{protocol: "TCP", port: 8000}]}]
                egress: storageRules
            }
        },
        {
            apiVersion: "networking.k8s.io/v1"
            kind: "NetworkPolicy"
            metadata: {name: "serving-${d.name}-picker", namespace: service.namespace}
            spec: {
                podSelector: {matchLabels: pickerLabels}
                policyTypes: ["Ingress", "Egress"]
                ingress: [
                    {from: [gatewayPeer] + privatePeers, ports: [{protocol: "TCP", port: 9002}]},
                    {from: [prometheusPeer], ports: [{protocol: "TCP", port: 9090}]},
                ]
                egress: [
                    {to: [{podSelector: {matchLabels: modelLabels}}], ports: [{protocol: "TCP", port: 8000}]},
                    dnsEgress,
                ] + _network_destination_egress(service.network.apiServer)
            }
        },
        {
            apiVersion: "networking.k8s.io/v1"
            kind: "NetworkPolicy"
            metadata: {name: "serving-${d.name}-gateway", namespace: service.namespace}
            spec: {
                podSelector: {matchLabels: gatewayLabels}
                policyTypes: ["Egress"]
                egress: [
                    {to: [{podSelector: {matchLabels: modelLabels}}], ports: [{protocol: "TCP", port: 8000}]},
                    {to: [{podSelector: {matchLabels: pickerLabels}}], ports: [{protocol: "TCP", port: 9002}]},
                ]
            }
        },
    ] + ([{
        apiVersion: "networking.k8s.io/v1"
        kind: "NetworkPolicy"
        metadata: {name: "serving-${d.name}-private", namespace: service.namespace}
        spec: {
            podSelector: {matchLabels: service.agentic.internalGatewayPodLabels}
            policyTypes: ["Egress"]
            egress: [
                {to: [{podSelector: {matchLabels: modelLabels}}], ports: [{protocol: "TCP", port: 8000}]},
                {to: [{podSelector: {matchLabels: pickerLabels}}], ports: [{protocol: "TCP", port: 9002}]},
            ]
        }
    }] if service.agentic != Undefined else [])
}

# service.network is required and validated by the Service schema. Rendering
# only emits workload-namespace objects, as required by catalog-sync's scoped
# ServiceAccount and Flux targetNamespace. Gateway is in the same namespace;
# Prometheus/DNS namespace policy ownership remains an operator prerequisite.
network_resources = lambda service: Service, artifacts: {str:Artifact}, resolvedPool: any -> [any] {
    assert all d in service.deployments {d.artifact in artifacts}, "unknown artifact reference in serving network policy"
    [policy for d in service.deployments for policy in _network_deployment(service, d, artifacts[d.artifact], resolvedPool)]
}
