# Run from the repository root: kcl test ./templates/features
import runtime

_test_pool = Pool {
    name: "gpu-pool"
    hardwareProfile: "a100"
    nodeSelector: {accelerator: "a100"}
    gpuPerNode: 4
    totalGpu: 8
    prometheusAddress: "http://prometheus:9090"
    versions: {kueue: "v0.18.0", keda: "2.20.2", "llm-d-router": "v1.0.0"}
    members: [
        {name: "alpha", service: "example", namespace: "models", gpuPerReplica: 2, prometheusQuery: "alpha_queue", threshold: "4"},
        {name: "beta", service: "example", namespace: "models", gpuPerReplica: 2, prometheusQuery: "beta_queue", threshold: "4"},
    ]
}
_test_service = {
    name: "example"
    namespace: "models"
    deployments: [
        {name: "alpha", hardware: "a100", tensorParallelSize: 2, role: "stable", weight: 1},
        {name: "beta", hardware: "a100", tensorParallelSize: 2, role: "stable", weight: 1},
    ]
}
_classes = [
    TrafficClass {name: "interactive", priority: 10, enginePriority: -10, maxRequests: 100, maxBytes: "1Gi"},
    TrafficClass {name: "batch", priority: -5, enginePriority: 5, maxRequests: 50, maxBytes: "500Mi"},
]

test_queue_quotas_and_preemption = lambda {
    items = render_pool(_test_pool)
    flavors = [x for x in items if x.kind == "ResourceFlavor"]
    queues = [x for x in items if x.kind == "ClusterQueue"]
    filler = [x for x in queues if x.metadata.name == "gpu-pool-filler"][0]
    alpha = [x for x in queues if x.metadata.name == "gpu-pool-alpha"][0]
    beta = [x for x in queues if x.metadata.name == "gpu-pool-beta"][0]
    assert len(flavors) == 1 and flavors[0].spec.nodeLabels == {accelerator: "a100"}
    assert alpha.apiVersion == "kueue.x-k8s.io/v1beta2"
    assert alpha.spec.cohortName == filler.spec.cohortName and beta.spec.cohortName == "gpu-pool"
    assert filler.spec.resourceGroups[0].flavors[0].resources[0].nominalQuota == "0"
    assert alpha.spec.resourceGroups[0].flavors[0].resources[0].nominalQuota == "4"
    assert beta.spec.resourceGroups[0].flavors[0].resources[0].nominalQuota == "4"
    assert alpha.spec.preemption.reclaimWithinCohort == "Any"
    assert alpha.spec.preemption.borrowWithinCohort.policy == "LowerPriority"
    assert alpha.spec.namespaceSelector.matchLabels["kubernetes.io/metadata.name"] == "models"
    assert len([x for x in items if x.kind == "LocalQueue"]) == 0
    assert len([x for x in items if x.kind == "Deployment" or x.kind == "ScaledObject"]) == 0
}

test_local_queues_belong_to_member_service = lambda {
    queues = local_queues(_test_service, _test_pool)
    assert len(queues) == 4
    assert [x for x in queues if x.metadata.name == "gpu-pool-alpha"][0].spec.clusterQueue == "gpu-pool-alpha"
    assert [x for x in queues if x.metadata.name == "gpu-pool-alpha-filler"][0].spec.clusterQueue == "gpu-pool-filler"
    assert all q in queues {q.metadata.namespace == "models"}
}

test_cross_service_pool_resources_are_rendered_once = lambda {
    shared = Pool {
        **_test_pool
        members = [
            {name: "alpha", service: "one", namespace: "models", gpuPerReplica: 2, prometheusQuery: "q1", threshold: "1"},
            {name: "beta", service: "two", namespace: "other-models", gpuPerReplica: 2, prometheusQuery: "q2", threshold: "1"},
        ]
    }
    first = {name: "one", namespace: "models", deployments: [{name: "alpha", hardware: "a100", tensorParallelSize: 2, role: "stable", weight: 1}]}
    second = {name: "two", namespace: "other-models", deployments: [{name: "beta", hardware: "a100", tensorParallelSize: 2, role: "stable", weight: 1}]}
    cluster_items = render_pool(shared)
    a = local_queues(first, shared)
    b = local_queues(second, shared)
    assert len([x for x in cluster_items if x.kind == "ClusterQueue"]) == 3
    assert len([x for x in cluster_items if x.kind == "LocalQueue"]) == 0
    assert len(a) == 2 and len(b) == 2
    assert a[1].metadata.name == "gpu-pool-alpha-filler"
    assert b[1].metadata.name == "gpu-pool-beta-filler"
    assert a[1].spec.clusterQueue == "gpu-pool-filler" and b[1].spec.clusterQueue == "gpu-pool-filler"
    assert [x for x in cluster_items if x.kind == "ClusterQueue" and x.metadata.name == "gpu-pool-beta"][0].spec.namespaceSelector.matchLabels["kubernetes.io/metadata.name"] == "other-models"
}

test_filler_and_load_replicas_share_pool = lambda {
    d = _test_service.deployments[0]
    normal = pool_labels(_test_pool, d.name, d.name, "normal")
    filler = pool_labels(_test_pool, d.name, d.name, "filler")
    assert normal["thalamus.ai/deployment"] == filler["thalamus.ai/deployment"]
    assert normal["thalamus.ai/pool-role"] == "normal" and filler["thalamus.ai/pool-role"] == "filler"
    assert normal["kueue.x-k8s.io/queue-name"] == "gpu-pool-alpha"
    assert filler["kueue.x-k8s.io/queue-name"] == "gpu-pool-alpha-filler"
    assert normal["llm-d.ai/engine-type"] == "vllm" and filler["llm-d.ai/engine-type"] == "vllm"
    assert priority_class(_test_pool, "normal") == "gpu-pool-normal"
    assert priority_class(_test_pool, "filler") == "gpu-pool-filler"
    assert (1 + filler_replicas(_test_pool, d)) * d.tensorParallelSize == 4
}

test_candidate_and_promoted_revision_share_logical_member = lambda {
    templated = Pool {
        **_test_pool
        members = [
            {name: "alpha", service: "example", namespace: "models", gpuPerReplica: 2, prometheusQuery: 'sum(queue{pool="{{deployment}}-inference-pool"}) or max(up{service="{{deployment}}-epp"})', threshold: "4"},
            {name: "beta", service: "example", namespace: "models", gpuPerReplica: 2, prometheusQuery: "beta_queue", threshold: "4"},
        ]
    }
    candidate = {name: "alpha-candidate", poolMember: "alpha", hardware: "a100", tensorParallelSize: 2, role: "candidate", weight: 1}
    before = {name: "example", namespace: "models", deployments: _test_service.deployments + [candidate]}
    queues = local_queues(before, _test_pool)
    labels = pool_labels(_test_pool, candidate.name, member_name(candidate), "normal")
    assert len(queues) == 4
    assert labels["thalamus.ai/deployment"] == "alpha-candidate"
    assert labels["kueue.x-k8s.io/queue-name"] == "gpu-pool-alpha"
    promoted = {name: "alpha-candidate", poolMember: "alpha", hardware: "a100", tensorParallelSize: 2, role: "stable", weight: 1}
    retained = {name: "alpha", hardware: "a100", tensorParallelSize: 2, role: "stable", weight: 0}
    after = {name: "example", namespace: "models", deployments: [retained, promoted, _test_service.deployments[1]]}
    assert len(local_queues(after, _test_pool)) == 4
    oldScaler = scaled_object(before, templated, _test_service.deployments[0])
    scaled = scaled_object(after, templated, promoted)
    assert oldScaler.metadata.name == "gpu-pool-alpha" and scaled.metadata.name == oldScaler.metadata.name
    assert oldScaler.spec.scaleTargetRef.name == "alpha" and scaled.spec.scaleTargetRef.name == "alpha-candidate"
    assert oldScaler.spec.triggers[0].metadata.query == 'sum(queue{pool="alpha-inference-pool"}) or max(up{service="alpha-epp"})'
    assert scaled.spec.triggers[0].metadata.query == 'sum(queue{pool="alpha-candidate-inference-pool"}) or max(up{service="alpha-candidate-epp"})'
}

test_reject_two_active_revisions_for_one_member = lambda {
    candidate = {name: "alpha-new", poolMember: "alpha", hardware: "a100", tensorParallelSize: 2, role: "candidate", weight: 1}
    valid = {name: "example", namespace: "models", deployments: _test_service.deployments + [candidate]}
    second = {name: "alpha-new", poolMember: "alpha", hardware: "a100", tensorParallelSize: 2, role: "stable", weight: 1}
    service = {name: "example", namespace: "models", deployments: _test_service.deployments + [second]}
    assert len(local_queues(valid, _test_pool)) == 4
    err = runtime.catch(lambda { q = local_queues(service, _test_pool) })
    assert err != Undefined
}

test_reject_foreign_member_reference = lambda {
    foreign = Pool {
        **_test_pool
        members = [
            {name: "alpha", service: "example", namespace: "models", gpuPerReplica: 2, prometheusQuery: "q", threshold: "1"},
            {name: "beta", service: "other", namespace: "models", gpuPerReplica: 2, prometheusQuery: "q", threshold: "1"},
        ]
    }
    candidate = {name: "candidate", poolMember: "alpha", hardware: "a100", tensorParallelSize: 2, role: "candidate", weight: 1}
    valid = {name: "example", namespace: "models", deployments: [_test_service.deployments[0], candidate]}
    badCandidate = {name: "candidate", poolMember: "beta", hardware: "a100", tensorParallelSize: 2, role: "candidate", weight: 1}
    service = {name: "example", namespace: "models", deployments: [_test_service.deployments[0], badCandidate]}
    assert len(local_queues(valid, foreign)) == 2
    err = runtime.catch(lambda { q = local_queues(service, foreign) })
    assert err != Undefined
}

test_keda_targets_the_routed_normal_workload = lambda {
    scaled = scaled_object(_test_service, _test_pool, _test_service.deployments[0])
    assert scaled.apiVersion == "keda.sh/v1alpha1"
    assert scaled.metadata.namespace == "models"
    assert scaled.spec.scaleTargetRef.kind == "Deployment"
    assert scaled.spec.scaleTargetRef.name == "alpha"
    assert scaled.spec.minReplicaCount == 1 and scaled.spec.maxReplicaCount == 4
    assert scaled.spec.advanced.horizontalPodAutoscalerConfig.behavior.scaleDown.stabilizationWindowSeconds == 600
    assert scaled.spec.triggers[0].metadata.ignoreNullValues == "false"
}

test_flow_control_and_scoped_objectives = lambda {
    cfg = router_config(_classes, "30s")
    assert cfg.apiVersion == "llm-d.ai/v1" and cfg.kind == "EndpointPickerConfig"
    assert "flowControl" in cfg.featureGates and cfg.requestHandler.propagatePriority
    assert cfg.flowControl.defaultRequestTTL == "30s"
    assert cfg.flowControl.priorityBands[0].priority == 10
    assert cfg.flowControl.priorityBands[0].maxRequests == "100"
    assert cfg.flowControl.priorityBands[1].maxBytes == "500Mi"
    assert all band in cfg.flowControl.priorityBands {band.fairnessPolicyRef == "round-robin-fairness-policy" and band.orderingPolicyRef == "fcfs-ordering-policy"}
    objs = objectives("models", ["alpha-inference-pool", "beta-inference-pool"], _classes, "example")
    assert objs[0].apiVersion == "llm-d.ai/v1" and objs[0].metadata.name == "example-interactive"
    assert objs[0].spec.priority == 10 and objs[1].spec.priority == -5
    assert objs[0].spec.poolRefs == [{name: "alpha-inference-pool"}, {name: "beta-inference-pool"}]
    other = objectives("models", ["other-inference-pool"], _classes, "other")
    assert other[0].metadata.name != objs[0].metadata.name
}

test_reject_partial_serving_unit = lambda {
    assert len(render_pool(_test_pool)) > 0
    err = runtime.catch(lambda {
        p = Pool {**_test_pool, members = [Member {**_test_pool.members[0], gpuPerReplica = 3}, _test_pool.members[1]]}
    })
    assert err != Undefined
}

test_reject_unsupported_kueue_version = lambda {
    assert len(render_pool(_test_pool)) > 0
    err = runtime.catch(lambda {
        p = Pool {**_test_pool, versions = {kueue: "v0.16.0", keda: "2.20.2", "llm-d-router": "v1.0.0"}}
    })
    assert err != Undefined
}

test_reject_unconfigured_quota_strategy = lambda {
    assert len(render_pool(_test_pool)) > 0
    err = runtime.catch(lambda { p = Pool {**_test_pool, quotaCheckStrategy = "BlockUndeclared"} })
    assert err != Undefined
}

test_reject_reserved_member = lambda {
    assert len(render_pool(_test_pool)) > 0
    assert runtime.catch(lambda {
        p = Pool {**_test_pool, members = [Member {**_test_pool.members[0], name = "filler"}, _test_pool.members[1]]}
    }) != Undefined
}

test_reject_duplicate_members = lambda {
    assert len(render_pool(_test_pool)) > 0
    assert runtime.catch(lambda {
        p = Pool {**_test_pool, members = [_test_pool.members[0], Member {**_test_pool.members[1], name = "alpha"}]}
    }) != Undefined
}

test_reject_deployment_not_in_pool_or_mismatched_gpu = lambda {
    assert scaled_object(_test_service, _test_pool, _test_service.deployments[0]).spec.scaleTargetRef.name == "alpha"
    missing = runtime.catch(lambda { x = scaled_object(_test_service, _test_pool, {name: "other", hardware: "a100", tensorParallelSize: 2, role: "stable", weight: 1}) })
    mismatch = runtime.catch(lambda { x = filler_replicas(_test_pool, {name: "alpha", hardware: "a100", tensorParallelSize: 1, role: "stable", weight: 1}) })
    assert missing != Undefined and mismatch != Undefined
}

test_reject_wrong_service_membership = lambda {
    assert len(local_queues(_test_service, _test_pool)) == 4
    wrong = {name: "intruder", namespace: "models", deployments: _test_service.deployments}
    err = runtime.catch(lambda { q = local_queues(wrong, _test_pool) })
    scaler = runtime.catch(lambda { s = scaled_object(wrong, _test_pool, wrong.deployments[0]) })
    assert err != Undefined and scaler != Undefined
}

test_reject_colliding_filler_queue_name = lambda {
    assert len(render_pool(_test_pool)) > 0
    err = runtime.catch(lambda {
        p = Pool {**_test_pool, members = [_test_pool.members[0], Member {**_test_pool.members[1], name = "alpha-filler"}]}
    })
    assert err != Undefined
}

test_reject_invalid_priority_and_bands = lambda {
    assert len(router_config(_classes, "30s").flowControl.priorityBands) == 2
    wrongSign = runtime.catch(lambda { c = TrafficClass {**_classes[0], enginePriority = 10} })
    # The engine's inverse follows the changed priority, so the only invalid
    # dimension is the duplicate router priority.
    duplicatePriority = runtime.catch(lambda {
        c = router_config([_classes[0], TrafficClass {**_classes[1], priority = 10, enginePriority = -10}], "30s")
    })
    unbounded = runtime.catch(lambda { c = router_config(_classes, "0s") })
    zeroMillis = runtime.catch(lambda { c = router_config(_classes, "0ms") })
    badDuration = runtime.catch(lambda { c = router_config(_classes, "not-a-duration") })
    assert wrongSign != Undefined and duplicatePriority != Undefined
    assert unbounded != Undefined and zeroMillis != Undefined and badDuration != Undefined
}

test_reject_invalid_objective_references = lambda {
    assert len(objectives("models", ["p", "q"], _classes, "svc")) == 2
    dup = runtime.catch(lambda { o = objectives("models", ["p", "p"], _classes, "svc") })
    invalidName = runtime.catch(lambda { o = objectives("models", ["p", "q"], _classes, "INVALID") })
    assert dup != Undefined and invalidName != Undefined
}
