# Reviewed composition of catalog intent. Git/Flux owns all emitted resources;
# the gateway and upstream llm-d endpoint picker remain on the inference path.
# A rendered manifest is not evidence that the cluster has applied it.
import json
import templates.features.kv_offload as kv
import templates.features.agentic_api as agentic_recipe
import templates.features.shared_pool as shared_pool_ops
import templates.gateway.policy as gateway_policy

manifest_name = lambda d: Deployment -> str {
    # ConfigMaps are immutable. Including the entire revision permits a later
    # in-place update without trying to modify an existing immutable object.
    # Split the digest into DNS labels rather than truncating the identity.
    "${d.name}-manifest.${d.revision[0:32]}.${d.revision[32:64]}"
}

backend_identity = lambda service: str, d: Deployment -> str {
    "${service}/${d.name}/${d.revision}"
}

model_manifest = lambda namespace: str, d: Deployment, artifact: Artifact -> any {
    {
        apiVersion: "v1"
        kind: "ConfigMap"
        metadata: {name: manifest_name(d), namespace: namespace}
        immutable: True
        data: {"manifest.json": json.encode({
            image: d.image
            modelPath: "/opt/thalamus/model"
            modelDigest: artifact.modelDigest
            tokenizerDigest: artifact.tokenizerDigest
        })}
    }
}

serving_annotations = lambda d: Deployment -> {str:str} {
    {
        "thalamus.ai/deployment": d.name
        "thalamus.ai/revision": d.revision
        "thalamus.ai/serving-kind": ("raw" if d.rolloutStrategy == "Recreate" else "kserve")
    } | ({"thalamus.ai/model-manifest": manifest_name(d)} if d.packagedModel else {})
}

kserve_workload = lambda service: Service, d: Deployment, artifact: Artifact, hardware: Hardware -> any {
    # An explicit pool spec replaces KServe's generated spec, so retain its
    # workload selector and picker binding while failing closed for flow control.
    scheduler = kv.scheduler(service.namespace, d.name) | ({
        pool: {
            spec: {
                selector: {
                    matchLabels: {
                        "app.kubernetes.io/name": d.name
                        "app.kubernetes.io/part-of": "llminferenceservice"
                        "kserve.io/component": "workload"
                    }
                }
                targetPorts: [{number: 8000}]
                endpointPickerRef: {
                    name: "${d.name}-epp-service"
                    kind: "Service"
                    port: {number: 9002}
                    failureMode: "FailClose"
                }
            }
        }
    } if service.trafficClasses != Undefined else {})
    template = pod_spec(service, d, hardware)
    {
        apiVersion: "serving.kserve.io/v1alpha2"
        kind: "LLMInferenceService"
        metadata: {name: d.name, namespace: service.namespace, annotations: serving_annotations(d)}
        spec: {
            model: {uri: artifact.uri, name: service.name}
            storageInitializer: {enabled: not d.packagedModel}
            replicas: d.replicas
            # KServe owns rolling Deployment updates. Strict Recreate is a raw
            # Flux-owned Deployment below, not an unsupported LLMISVC field.
            rolloutStrategy: {maxUnavailable: 0, maxSurge: 1}
            labels: {"llm-d.ai/engine-type": "vllm"}
            router: {scheduler: scheduler}
            template: template
        }
    }
}

backend = lambda service: Service, d: Deployment, pooled: bool -> any {
    {
        apiVersion: "agentgateway.dev/v1alpha1"
        kind: "AgentgatewayBackend"
        metadata: {
            name: "${d.name}-backend"
            namespace: service.namespace
            annotations: {
                "thalamus.ai/deployment": d.name
                "thalamus.ai/revision": d.revision
                "thalamus.ai/serving-kind": ("raw" if d.rolloutStrategy == "Recreate" or pooled else "kserve")
            }
        }
        spec: {
            ai: {provider: {custom: {
                backendRef: {group: "inference.networking.k8s.io", kind: "InferencePool", name: "${d.name}-inference-pool"}
                model: service.name
                formats: [{type: "Completions", path: "/v1/chat/completions"}] + ([{type: "Responses", path: "/v1/responses"}, {type: "Messages", path: "/v1/messages"}] if service.agentic != Undefined else [])
            }}}
            # This is a logical gateway/backend assertion, not a pod identity.
            policies: {
                transformation: {response: {set: [{name: "x-thalamus-backend", value: json.encode(backend_identity(service.name, d))}]}}
                if service.agentic != Undefined:
                    ai: {routes: {"/v1/chat/completions": "Completions", "/v1/responses": "Responses", "/v1/messages": "Messages"}}
            }
        }
    }
}

# Agentgateway's ext_proc timeout defaults to 10s. A queued request must be
# allowed to wait for the configured llm-d TTL plus a small dispatch margin.
picker_timeout_policy = lambda service: Service, d: Deployment, pooled: bool -> any {
    {
        apiVersion: "agentgateway.dev/v1alpha1"
        kind: "AgentgatewayPolicy"
        metadata: {name: "${d.name}-epp-timeout", namespace: service.namespace}
        spec: {
            targetRefs: [{group: "", kind: "Service", name: ("${d.name}-epp" if d.rolloutStrategy == "Recreate" or pooled else "${d.name}-epp-service")}]
            backend: {http: {requestTimeout: "${service.trafficClasses.ttl}5s"}}
        }
    }
}

route_policy = lambda svc: Service, routeName: str, candidateName: str, isBenchmark: bool, resolvedRate: gateway_policy.RateLimit -> any {
    gateway_policy.Policy {
        namespace: svc.namespace
        service: svc.name
        route: routeName
        keySelector: svc.gateway.keySelector
        version: svc.versions["agentgateway"]
        rateLimit: resolvedRate
        candidateDeployment: candidateName
        benchmark: isBenchmark
        trafficClasses: ([c.name for c in svc.trafficClasses.classes] if svc.trafficClasses != Undefined else [])
    }.item
}

public_route = lambda service: Service -> any {
    targets = [{group: "agentgateway.dev", kind: "AgentgatewayBackend", name: "${d.name}-backend", weight: d.weight} for d in service.deployments if d.role == "stable" and d.weight > 0]
    {
        apiVersion: "gateway.networking.k8s.io/v1"
        kind: "HTTPRoute"
        metadata: {name: service.name, namespace: service.namespace}
        spec: {
            parentRefs: [{group: "gateway.networking.k8s.io", kind: "Gateway", name: service.gateway.name, namespace: service.gateway.namespace}]
            hostnames: [service.gateway.hostname]
            rules: [{
                matches: [{method: "POST", path: {type: "Exact", value: "/v1/chat/completions"}}]
                backendRefs: targets
            }]
        }
    }
}

candidate_route = lambda service: Service, d: Deployment -> any {
    {
        apiVersion: "gateway.networking.k8s.io/v1"
        kind: "HTTPRoute"
        metadata: {name: "${d.name}-candidate", namespace: service.namespace}
        spec: {
            parentRefs: [{group: "gateway.networking.k8s.io", kind: "Gateway", name: service.gateway.name, namespace: service.gateway.namespace}]
            hostnames: [service.gateway.hostname]
            rules: [{
                matches: [{method: "POST", path: {type: "Exact", value: "/candidates/${d.name}/v1/chat/completions"}}]
                filters: [{type: "URLRewrite", urlRewrite: {path: {type: "ReplaceFullPath", replaceFullPath: "/v1/chat/completions"}}}]
                backendRefs: [{group: "agentgateway.dev", kind: "AgentgatewayBackend", name: "${d.name}-backend"}]
            }]
        }
    }
}

# The upstream inference-perf adapter uses the legacy text-completions API.
# Agentgateway's AI Completions format is chat-only. Every deployment has its
# own benchmark-only route to the actual InferencePool, bound to a run key and
# a request-count RLS policy. Legacy benchmark traffic has no token accounting.
benchmark_route = lambda service: Service, d: Deployment -> any {
    {
        apiVersion: "gateway.networking.k8s.io/v1"
        kind: "HTTPRoute"
        metadata: {name: "${d.name}-benchmark", namespace: service.namespace}
        spec: {
            parentRefs: [{group: "gateway.networking.k8s.io", kind: "Gateway", name: service.gateway.name, namespace: service.gateway.namespace}]
            hostnames: [service.gateway.hostname]
            rules: [{
                matches: [{method: "POST", path: {type: "Exact", value: "/candidates/${d.name}/v1/completions"}}]
                filters: [
                    {type: "URLRewrite", urlRewrite: {path: {type: "ReplaceFullPath", replaceFullPath: "/v1/completions"}}},
                    {type: "ResponseHeaderModifier", responseHeaderModifier: {set: [{name: "x-thalamus-backend", value: backend_identity(service.name, d)}]}},
                ]
                backendRefs: [{
                    group: "inference.networking.k8s.io"
                    kind: "InferencePool"
                    name: "${d.name}-inference-pool"
                    weight: 1
                }]
            }]
        }
    }
}

render_service = lambda service: Service, artifacts: {str:Artifact}, hardware: {str:Hardware}, resolvedPool: any, limitProfile: gateway_policy.LimitProfile, budgetCaps: gateway_policy.BudgetCaps -> any {
    assert all d in service.deployments {d.artifact in artifacts and d.hardware in hardware}, "unknown artifact or hardware reference"
    assert all d in service.deployments {d.tensorParallelSize <= hardware[d.hardware].gpuCount}, "tensor parallel size exceeds GPUs per node"
    assert all d in service.deployments {not d.packagedModel or artifacts[d.artifact].uri == "oci://${d.image}"}, "packaged model artifact URI must identify the exact serving image digest"
    assert service.versions["kserve"] == "v0.21.0" and service.versions["agentgateway"] == "v1.7.0" and service.versions["inferenceExtension"] == "v1.6.2", "renderer requires KServe v0.21.0, agentgateway v1.7.0 and inference extension v1.6.2"
    assert service.gateway.rateLimit == limitProfile.name and limitProfile.namespace == service.namespace, "service must resolve its shared limit profile in the worker workload namespace"
    assert limitProfile.deployment == Undefined or (limitProfile.deployment.gatewayName == service.gateway.name and limitProfile.deployment.gatewayNamespace == service.gateway.namespace and limitProfile.deployment.gatewayPodSelector == service.network.gatewayPodSelector), "service and limiter must select the same managed gateway"
    assert service.agentic == Undefined or (limitProfile.deployment != Undefined and limitProfile.deployment.agenticGatewayPodSelector != Undefined and limitProfile.deployment.agenticGatewayPodSelector == service.agentic.internalGatewayPodLabels), "agentic internal gateway must match the limiter ingress selector"
    rateLimit = gateway_policy.from_catalog(limitProfile, budgetCaps)
    assert (service.pool == Undefined and resolvedPool == Undefined) or (service.pool != Undefined and resolvedPool != Undefined and service.pool == resolvedPool.name), "service pool reference must resolve to the named catalog pool"
    assert resolvedPool == Undefined or (all d in service.deployments {hardware[d.hardware].nodeSelector == resolvedPool.nodeSelector and hardware[d.hardware].gpuCount == resolvedPool.gpuPerNode}), "pool flavor and hardware topology must match the serving hardware"
    assert resolvedPool == Undefined or (all d in service.deployments {d.packagedModel}), "shared-pool workloads require a packaged immutable model"
    assert resolvedPool == Undefined or (all d in service.deployments {d.role != "stable" or d.weight == 0 or d.rolloutStrategy != "Recreate" or shared_pool_ops.filler_replicas(resolvedPool, d) == 0}), "Recreate with shared-pool fillers requires a staged drain and observation"

    schedulers = [item for d in service.deployments for item in (kv.resources(service.namespace, d.name, scheduler_config(service, d)) if d.rolloutStrategy == "RollingUpdate" and resolvedPool == Undefined else [])]
    kserve = [kserve_workload(service, d, artifacts[d.artifact], hardware[d.hardware]) for d in service.deployments if d.rolloutStrategy == "RollingUpdate" and resolvedPool == Undefined]
    raw_workloads = [item for d in service.deployments for item in (render(service, d, artifacts[d.artifact], hardware[d.hardware], resolvedPool) if d.rolloutStrategy == "Recreate" or resolvedPool != Undefined else [])]
    objectives = (shared_pool_ops.objectives(service.namespace, ["${d.name}-inference-pool" for d in service.deployments], service.trafficClasses.classes, service.name) if service.trafficClasses != Undefined else [])
    local_queues = (shared_pool_ops.local_queues(service, resolvedPool) if resolvedPool != Undefined else [])
    agentic_resources = (agentic_recipe.agentic_resources(service.agentic, rateLimit, service.deployments) if service.agentic != Undefined else [])
    networkPolicies = network_resources(service, artifacts, resolvedPool)
    publicPolicies = [route_policy(service, service.name, "", False, rateLimit)]
    candidatePolicies = [route_policy(service, "${d.name}-candidate", d.name, False, rateLimit) for d in service.deployments if d.role == "candidate"]
    benchmarkPolicies = [route_policy(service, "${d.name}-benchmark", d.name, True, rateLimit) for d in service.deployments]
    pickerPolicies = ([picker_timeout_policy(service, d, resolvedPool != Undefined) for d in service.deployments] if service.trafficClasses != Undefined else [])
    modelManifests = [model_manifest(service.namespace, d, artifacts[d.artifact]) for d in service.deployments if d.packagedModel]
    backends = [backend(service, d, resolvedPool != Undefined) for d in service.deployments]
    publicRoutes = [public_route(service)]
    candidateRoutes = [candidate_route(service, d) for d in service.deployments if d.role == "candidate"]
    benchmarkRoutes = [benchmark_route(service, d) for d in service.deployments]
    objects = publicPolicies + candidatePolicies + benchmarkPolicies + pickerPolicies + modelManifests + schedulers + kserve + raw_workloads + backends + publicRoutes + candidateRoutes + benchmarkRoutes + local_queues + objectives + agentic_resources + networkPolicies
    resourceKeys = ["${r.apiVersion}|${r.kind}|${r.metadata.namespace}|${r.metadata.name}" for r in objects]
    assert len({key: True for key in resourceKeys}) == len(resourceKeys), "rendered resource names must not collide"
    {apiVersion: "v1", kind: "List", items: objects}
}
