# Flux-owned raw llm-d serving for native Kubernetes Deployment rollout
# strategies. Agentgateway still calls the upstream llm-d endpoint picker; this
# module does not introduce an inference proxy or scheduler.
#
# https://github.com/llm-d/llm-d-router/blob/v1.0.0/config/charts/routerlib/templates/_deployment.yaml
# https://github.com/llm-d/llm-d-router/blob/v1.0.0/config/charts/routerlib/templates/_inferencepool.yaml
# https://github.com/llm-d/llm-d-router/blob/v1.0.0/pkg/epp/server/options.go
# Images, CRDs, network isolation, model content, and controllers remain trusted
# operator prerequisites. Flux owns these resources, not the application worker.
import json
import regex
import templates.features.kv_offload as kv
import templates.features.shared_pool as shared_pool

# A single engine specification is used by both KServe and raw workloads.
# Non-packaged models depend on KServe's storage initializer and are never
# accepted by raw.render. The image contains the engine and model/tokenizer
# under /opt/thalamus/model in packaged mode; this is intent, not attestation.
pod_spec = lambda service: any, d: any, hardware: any -> any {
    memory = kv.pod_memory_gi(d.features.kvOffloadGi, d.features.shmGi, hardware.memoryGi, hardware.hostMemoryGi, hardware.sharedGpuMemory)
    shm = kv.shm_gi(d.features.kvOffloadGi, d.features.shmGi)
    resources = {"nvidia.com/gpu": "${d.tensorParallelSize}", cpu: hardware.cpu, memory: "${memory}Gi"}
    {
        nodeSelector: hardware.nodeSelector
        volumes: [{name: "shm", emptyDir: {medium: "Memory", sizeLimit: "${shm}Gi"}}] + ([{name: "tmp", emptyDir: {}}] if d.packagedModel else [])
        containers: [{
            name: "main"
            image: d.image
            command: (["vllm", "serve", "/opt/thalamus/model"] if d.packagedModel else Undefined)
            env: ([{name: "HOME", value: "/tmp"}, {name: "HF_HOME", value: "/tmp/huggingface"}, {name: "HF_HUB_OFFLINE", value: "1"}, {name: "TRANSFORMERS_OFFLINE", value: "1"}] if d.packagedModel else Undefined)
            securityContext: ({readOnlyRootFilesystem: True, allowPrivilegeEscalation: False, capabilities: {drop: ["ALL"]}} if d.packagedModel else Undefined)
            args: [
                "--served-model-name", service.name,
                "--max-model-len", "${d.contextLength}",
                "--max-num-seqs", "${d.maxNumSeqs}",
                "--max-num-batched-tokens", "${d.maxNumBatchedTokens}",
                "--tensor-parallel-size", "${d.tensorParallelSize}",
            ] + (["--tokenizer", "/opt/thalamus/model"] if d.packagedModel else []) + (["--enable-prefix-caching"] if d.features.prefixCaching else ["--no-enable-prefix-caching"]) + kv.offload_args(d.features.kvOffloadGi) + (["--enable-auto-tool-choice", "--tool-call-parser", d.features.toolParser] if d.features.toolParser != "" else []) + (["--reasoning-parser", d.features.reasoningParser] if d.features.reasoningParser != "" else []) + (["--scheduling-policy", "priority"] if service.trafficClasses != Undefined else [])
            ports: [{name: "http", containerPort: 8000, protocol: "TCP"}]
            readinessProbe: {httpGet: {path: "/health", port: 8000}, periodSeconds: 10, timeoutSeconds: 5}
            volumeMounts: [{name: "shm", mountPath: "/dev/shm"}] + ([{name: "tmp", mountPath: "/tmp"}] if d.packagedModel else [])
            resources: {requests: resources, limits: resources}
        }]
    }
}

# Both KServe and the raw EPP use precisely the same supported v1 config.
# --config-text embeds the configuration in the raw EPP pod template so a
# config change restarts it with the intended content atomically. A mutable
# ConfigMap mount could be updated after the restarted EPP read its old file.
scheduler_config = lambda service: any, d: any -> any {
    flow = (shared_pool.router_config(service.trafficClasses.classes, service.trafficClasses.ttl) if service.trafficClasses != Undefined else Undefined)
    kv.config(d.features.prefixCaching, d.features.kvOffloadGi, flow)
}

# Deployment selectors are stable regardless of Kueue. The pool selector is
# intentionally common to normal and filler while each Deployment owns only
# its own role. The role is not an authentication or ownership boundary.
workload = lambda service: any, d: any, hardware: any, role: str, pool: any -> any {
    assert role in ["normal", "filler"], "invalid raw deployment role"
    assert role == "normal" or pool != Undefined, "filler requires a shared pool"
    memberName = shared_pool.member_name(d)
    active = d.role == "stable" and d.weight > 0
    assert role != "filler" or active, "only an active stable deployment can have fillers"
    roleLabels = (shared_pool.pool_labels(pool, d.name, memberName, role) if pool != Undefined else {"thalamus.ai/deployment": d.name, "thalamus.ai/pool-role": role, "llm-d.ai/engine-type": "vllm"})
    selector = {"thalamus.ai/deployment": d.name, "thalamus.ai/pool-role": role}
    name = (d.name if role == "normal" else "${d.name}-filler")
    engineSpec = pod_spec(service, d, hardware)
    annotations = {
        "thalamus.ai/serving-kind": "raw"
        "thalamus.ai/deployment": d.name
        "thalamus.ai/revision": d.revision
        "thalamus.ai/model-manifest": "${d.name}-manifest.${d.revision[0:32]}.${d.revision[32:64]}"
    }
    {
        apiVersion: "apps/v1"
        kind: "Deployment"
        metadata: {name: name, namespace: service.namespace, annotations: annotations}
        spec: {
            # KEDA owns replicas only for the active stable workload. A pooled
            # candidate or retained revision runs one replica without changing
            # the catalog's desired promoted configuration.
            replicas: (shared_pool.filler_replicas(pool, d) if role == "filler" else (Undefined if pool != Undefined and active else (1 if pool != Undefined else d.replicas)))
            strategy: {type: d.rolloutStrategy}
            selector: {matchLabels: selector}
            template: {
                metadata: {labels: roleLabels, annotations: annotations}
                spec: {
                    **engineSpec
                    automountServiceAccountToken: False
                    priorityClassName: (shared_pool.priority_class(pool, role) if pool != Undefined else Undefined)
                }
            }
        }
    }
}

render = lambda service: any, d: any, artifact: any, hardware: any, pool: any -> [any] {
    assert service.name != "" and service.namespace != "", "raw serving requires a service and namespace"
    assert d.rolloutStrategy in ["Recreate", "RollingUpdate"], "invalid raw rollout strategy"
    assert d.rolloutStrategy == "Recreate" or pool != Undefined, "raw RollingUpdate requires a shared pool"
    assert d.packagedModel, "raw serving requires a packaged immutable model"
    assert len(d.image.split("@sha256:")) == 2 and regex.match(d.image, "^.+@sha256:[0-9a-f]{64}$"), "raw serving requires an immutable sha256 image digest"
    assert regex.match(d.revision, "^[0-9a-f]{64}$"), "raw serving requires a lowercase sha256 deployment revision"
    assert artifact.uri == "oci://${d.image}", "packaged model artifact URI must identify the exact serving image digest"
    assert d.tensorParallelSize <= hardware.gpuCount, "tensor parallel size exceeds GPUs per node"
    active = d.role == "stable" and d.weight > 0
    member = (shared_pool.member_for(pool, d) if pool != Undefined else Undefined)
    assert pool == Undefined or (member.service == service.name and member.namespace == service.namespace), "deployment must belong to this pool member service and namespace"
    # Distinct normal and filler Deployments are independent controllers;
    # Recreate cannot prevent them from overlapping revisions. The caller must
    # coordinate a drain before changing a previously nonzero filler count.
    assert pool == Undefined or not active or d.rolloutStrategy != "Recreate" or shared_pool.filler_replicas(pool, d) == 0, "Recreate cannot guarantee no overlap with shared-pool fillers"
    poolName = "${d.name}-inference-pool"
    eppName = "${d.name}-epp"
    eppLabels = {"app.kubernetes.io/name": eppName}
    eppConfig = json.encode(scheduler_config(service, d))
    [
        workload(service, d, hardware, "normal", pool),
        {
            apiVersion: "inference.networking.k8s.io/v1"
            kind: "InferencePool"
            metadata: {name: poolName, namespace: service.namespace}
            spec: {
                selector: {matchLabels: {"thalamus.ai/deployment": d.name}}
                targetPorts: [{number: 8000}]
                appProtocol: "http"
                endpointPickerRef: {name: eppName, kind: "Service", port: {number: 9002}, failureMode: "FailClose"}
            }
        },
        {
            apiVersion: "v1"
            kind: "Service"
            metadata: {name: eppName, namespace: service.namespace}
            # Operators must configure Prometheus scraping and protect this
            # unauthenticated metrics endpoint as part of the cluster boundary.
            spec: {selector: eppLabels, ports: [
                {name: "grpc-ext-proc", protocol: "TCP", port: 9002, targetPort: 9002, appProtocol: "kubernetes.io/h2c"},
                {name: "http-metrics", protocol: "TCP", port: 9090, targetPort: 9090},
            ]}
        },
        {
            apiVersion: "apps/v1"
            kind: "Deployment"
            metadata: {name: eppName, namespace: service.namespace}
            spec: {
                replicas: 1
                strategy: {type: "Recreate"}
                selector: {matchLabels: eppLabels}
                template: {
                    metadata: {labels: eppLabels}
                    spec: {
                        serviceAccountName: "thalamus-epp"
                        terminationGracePeriodSeconds: 130
                        containers: [{
                            name: "epp"
                            image: kv.kv_router_image
                            command: ["/app/epp"]
                            args: ["--pool-name", poolName, "--pool-namespace", service.namespace, "--pool-group", "inference.networking.k8s.io", "--grpc-port=9002", "--grpc-health-port", "9003", "--secure-serving=false", "--metrics-endpoint-auth=false", "--tracing=false", "--config-text", eppConfig]
                            ports: [{name: "grpc", containerPort: 9002}, {name: "grpc-health", containerPort: 9003}, {name: "metrics", containerPort: 9090}]
                            livenessProbe: {grpc: {port: 9003, service: "liveness"}, initialDelaySeconds: 5, periodSeconds: 10}
                            readinessProbe: {grpc: {port: 9003, service: "readiness"}, periodSeconds: 2}
                            resources: {requests: {cpu: "8", memory: "8Gi"}, limits: {memory: "16Gi"}}
                        }]
                    }
                }
            }
        },
    ] + ([workload(service, d, hardware, "filler", pool), shared_pool.scaled_object(service, pool, d)] if pool != Undefined and active else [])
}
