# Run from the repository root: kcl test ./templates/features
import runtime

_test_pool = Pool {
    name: "gpu-pool"
    namespace: "models"
    hardwareProfile: "a100"
    nodeSelector: {accelerator: "a100"}
    gpuPerNode: 4
    totalGpu: 8
    prometheusAddress: "http://prometheus:9090"
    versions: {kueue: "v0.16.0", keda: "2.21.0", "llm-d-router": "v1.0.0"}
    quotaCheckStrategy: "IgnoreUndeclared"
    members: [
        {name: "alpha", image: "vllm:v1", model: "alpha-model", gpuPerReplica: 2, cpu: "4", memory: "16Gi", prometheusQuery: "alpha_queue", threshold: "4"},
        {name: "beta", image: "vllm:v1", model: "beta-model", gpuPerReplica: 2, cpu: "4", memory: "16Gi", prometheusQuery: "beta_queue", threshold: "4"},
    ]
}

test_queues_and_workloads = lambda {
    items = render_pool(_test_pool)
    queues = [x for x in items if x.kind == "ClusterQueue"]
    filler = [x for x in queues if x.metadata.name == "gpu-pool-filler"][0]
    normal = [x for x in queues if x.metadata.name == "gpu-pool-alpha"][0]
    assert filler.spec.resourceGroups[0].flavors[0].resources[0].nominalQuota == "0"
    assert normal.spec.resourceGroups[0].flavors[0].resources[0].nominalQuota == "4"
    assert normal.spec.cohort == filler.spec.cohort
    assert normal.spec.preemption.reclaimWithinCohort == "Any"
    assert normal.spec.preemption.borrowWithinCohort.policy == "LowerPriority"
    deployments = [x for x in items if x.kind == "Deployment"]
    a = [x for x in deployments if x.metadata.name == "gpu-pool-alpha-normal"][0]
    f = [x for x in deployments if x.metadata.name == "gpu-pool-alpha-filler"][0]
    assert a.spec.replicas == 1 and f.spec.replicas == 1
    assert (a.spec.replicas + f.spec.replicas) * 2 == 4
    assert a.spec.template.metadata.labels["thalamus.ai/pool-member"] == f.spec.template.metadata.labels["thalamus.ai/pool-member"]
    assert f.spec.template.metadata.labels["kueue.x-k8s.io/queue-name"] == "gpu-pool-filler"
    assert "priority" in a.spec.template.spec.containers[0].args
    scaled = [x for x in items if x.kind == "ScaledObject"][0]
    assert scaled.spec.minReplicaCount == 1
    assert scaled.spec.triggers[0].metadata.ignoreNullValues == "false"
    assert scaled.spec.advanced.horizontalPodAutoscalerConfig.behavior.scaleDown.stabilizationWindowSeconds == 600
}

test_reject_partial_serving_unit = lambda {
    err = runtime.catch(lambda {
        p = Pool {
            name: "p", namespace: "n", hardwareProfile: "h", nodeSelector: {gpu: "h"}
            gpuPerNode: 4, totalGpu: 8, prometheusAddress: "http://prometheus"
            versions: {kueue: "v", keda: "v", "llm-d-router": "v"}
            quotaCheckStrategy: "IgnoreUndeclared"
            members: [{name: "m", image: "img", model: "model", gpuPerReplica: 3, cpu: "1", memory: "1Gi", prometheusQuery: "q", threshold: "1"}]
        }
    })
    assert err == "replicas must divide nodes and nominal shares"
}

test_reject_missing_quota_strategy = lambda {
    err = runtime.catch(lambda {
        p = Pool {
            name: "p", namespace: "n", hardwareProfile: "h", nodeSelector: {gpu: "h"}
            gpuPerNode: 4, totalGpu: 4, prometheusAddress: "http://prometheus"
            versions: {kueue: "v", keda: "v", "llm-d-router": "v"}
            members: [{name: "m", image: "img", model: "model", gpuPerReplica: 1, cpu: "1", memory: "1Gi", prometheusQuery: "q", threshold: "1"}]
        }
    })
    assert err == "GPU-only queues require Kueue quotaCheckStrategy IgnoreUndeclared"
}

test_reject_reserved_filler_member = lambda {
    err = runtime.catch(lambda {
        p = Pool {
            **_test_pool
            members = [{name: "filler", image: "img", model: "model", gpuPerReplica: 2, cpu: "1", memory: "1Gi", prometheusQuery: "q", threshold: "1"}]
        }
    })
    assert err == "member name filler is reserved for the shared filler queue"
}

test_traffic_priority_and_router = lambda {
    classes = [TrafficClass {name: "interactive", priority: 10, enginePriority: -10, maxRequests: 100, maxBytes: "1Gi"}, TrafficClass {name: "batch", priority: -5, enginePriority: 5, maxRequests: 50, maxBytes: "1Gi"}]
    config = router_config(classes, "30s")
    assert "flowControl" in config.featureGates
    assert config.requestHandler.propagatePriority
    assert config.flowControl.defaultPriorityBand.fairnessPolicyRef == "round-robin-fairness-policy"
    obj = objectives("models", ["alpha-pool"], classes)
    assert obj[0].spec.priority == 10 and obj[1].spec.priority == -5
    assert obj[0].spec.poolRefs[0].name == "alpha-pool"
    err = runtime.catch(lambda { c = TrafficClass {name: "wrong", priority: 10, enginePriority: 10, maxRequests: 10, maxBytes: "1Gi"} })
    assert err == "vLLM priority must invert router priority"
}
