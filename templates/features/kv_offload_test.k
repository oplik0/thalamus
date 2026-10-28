# Run from the repository root: kcl test ./templates/features
import json
import runtime

test_kv_offload_rejects_negative_size = lambda {
    assert "--kv-offloading-size" in offload_args(1)
    err = runtime.catch(lambda { args = offload_args(-1) })
    assert err != Undefined
}

test_kv_memory_reservation_and_boundaries = lambda {
    assert pod_memory_gi(0, 2, 24, 24, True) <= 24
    # --kv-offloading-size is one total pool for all TP ranks. The scheduler
    # must request and limit the total below, not multiply by tensor parallelism.
    assert pod_memory_gi(16, 2, 32, 64, False) == 64
    assert shm_gi(16, 2) == 34
    host_err = runtime.catch(lambda { size = pod_memory_gi(16, 2, 32, 63, False) })
    assert host_err != Undefined
    assert pod_memory_gi(0, 7, 8, 32, False) <= 32
    shm_err = runtime.catch(lambda { size = pod_memory_gi(0, 8, 8, 32, False) })
    assert shm_err != Undefined
    assert pod_memory_gi(1, 1, 16, 64, False) <= 64
    shared_err = runtime.catch(lambda { size = pod_memory_gi(1, 1, 16, 64, True) })
    assert shared_err != Undefined
    assert shm_gi(1, 1) > 0
    invalid_err = runtime.catch(lambda { size = shm_gi(1, 0) })
    assert invalid_err != Undefined
}

test_kv_scheduler_locality_configuration = lambda {
    disabled = config(False, 0, Undefined)
    disabled_types = [p.type for p in disabled.plugins]
    assert "load-aware-scorer" in disabled_types
    assert "prefix-cache-scorer" not in disabled_types
    assert "approx-prefix-cache-producer" not in disabled_types
    baseline = config(True, 0, Undefined)
    offloaded = config(True, 8, Undefined)
    baseline_producer = [p for p in baseline.plugins if p.type == "approx-prefix-cache-producer"][0]
    offloaded_producer = [p for p in offloaded.plugins if p.type == "approx-prefix-cache-producer"][0]
    assert offloaded_producer.parameters.lruCapacityPerServer > baseline_producer.parameters.lruCapacityPerServer
    assert not offloaded_producer.parameters.autoTune
    assert "prefix-cache-scorer" in [p.pluginRef for p in offloaded.schedulingProfiles[0].plugins]
    err = runtime.catch(lambda { cfg = config(False, 8, Undefined) })
    assert err != Undefined
}

test_kv_scheduler_composes_flow_control = lambda {
    classes = [TrafficClass {name: "interactive", priority: 10, enginePriority: -10, maxRequests: 100, maxBytes: "1Gi"}]
    flow = router_config(classes, "30s")
    cfg = config(True, 8, flow)
    types = [p.type for p in cfg.plugins]
    assert "prefix-cache-scorer" in types
    assert "approx-prefix-cache-producer" in types
    assert "round-robin-fairness-policy" in types
    assert "fcfs-ordering-policy" in types
    assert "utilization-detector" in types
    assert cfg.featureGates == ["flowControl"]
    assert cfg.requestHandler.propagatePriority
    assert cfg.flowControl.defaultRequestTTL == "30s"
    assert cfg.flowControl.priorityBands[0].priority == 10
    assert cfg.flowControl.defaultPriorityBand.fairnessPolicyRef == "round-robin-fairness-policy"
    assert "prefix-cache-scorer" in [p.pluginRef for p in cfg.schedulingProfiles[0].plugins]
    without_cache = config(False, 0, flow)
    assert "prefix-cache-scorer" not in [p.type for p in without_cache.plugins]
    assert without_cache.flowControl.priorityBands[0].maxRequests == "100"
}

test_kv_scheduler_ref_and_configmap = lambda {
    cfg = config(True, 8, Undefined)
    cms = resources("models", "alpha", cfg)
    sched = scheduler("models", "alpha")
    assert cms[0].kind == "ConfigMap"
    assert cms[0].metadata.namespace == "models"
    assert cms[0].metadata.name == sched.config.ref.name
    assert sched.config.ref.key == kv_config_key
    stored = json.decode(cms[0].data[kv_config_key])
    assert stored.apiVersion == "llm-d.ai/v1" and stored.kind == "EndpointPickerConfig"
    assert "prefix-cache-scorer" in [p.pluginRef for p in stored.schedulingProfiles[0].plugins]
    assert "approx-prefix-cache-producer" in [p.type for p in stored.plugins]
    main = sched.template.containers[0]
    assert main.name == "main"
    assert "alpha-inference-pool" in main.command
    assert "models" in main.command
    assert "--secure-serving=false" in main.command
    assert "--metrics-endpoint-auth=false" in main.command
    assert "--config-file" not in main.command and "--config-text" not in main.command
}

test_kv_scheduler_invalid_input = lambda {
    cfg = config(True, 0, Undefined)
    valid = scheduler("models", "alpha")
    assert valid.config.ref.name != ""
    assert len(resources("models", "alpha", cfg)) == 1
    identity_err = runtime.catch(lambda { s = scheduler("", "alpha") })
    invalid_config = {**cfg, apiVersion = "v1"}
    config_err = runtime.catch(lambda { cm = resources("models", "alpha", invalid_config) })
    size_err = runtime.catch(lambda { bad = config(True, -1, Undefined) })
    assert identity_err != Undefined and config_err != Undefined and size_err != Undefined
}
