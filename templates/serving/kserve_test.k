# Run from the repository root: kcl test ./templates/serving
import runtime

test_default_deployment = lambda {
    d = Deployment {name: "d", artifact: "a", hardware: "h", image: "image@sha256:123"}
    assert d.replicas == 1
    assert d.features.prefixCaching
    assert d.features.kvOffloadGi == 0
    assert d.weight == 1
}
test_packaged_model_rejects_mutable_tag = lambda {
    err = runtime.catch(lambda {
        d = Deployment {name: "d", artifact: "a", hardware: "h", image: "example:latest", packagedModel: True}
    })
    assert err == "packaged model requires an immutable sha256 image digest"
}

test_packaged_model_accepts_digest = lambda {
    d = Deployment {name: "d", artifact: "a", hardware: "h", image: "example@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef", packagedModel: True}
    assert d.packagedModel
}

test_offload_requires_prefix_cache = lambda {
    err = runtime.catch(lambda {
        f = Features {kvOffloadGi: 4, prefixCaching: False}
    })
    assert err == "KV offload requires prefix caching"
}

test_hardware_memory_bound = lambda {
    err = runtime.catch(lambda {
        h = Hardware {nodeSelector: {accelerator: "gpu"}, gpuCount: 1, cpu: "4", memoryGi: 32, hostMemoryGi: 16}
    })
    assert err == "invalid pod/host memory"
}

test_reject_untrusted_agentic_mode = lambda {
    err = runtime.catch(lambda {
        s = Service {
            name: "svc"
            namespace: "ns"
            gateway: {name: "gw", namespace: "ns", hostname: "example.invalid", keySelector: {service: "svc"}}
            versions: {kserve: "v", agentgateway: "v", inferenceExtension: "v", vllm: "v"}
            deployments: [{name: "d", artifact: "a", hardware: "h", image: "image"}]
            agenticMode: "shared"
        }
    })
    assert err == "agentic-api requires verified tenant isolation and trusted attribution; unsupported"
}

test_reject_mismatched_contract = lambda {
    err = runtime.catch(lambda {
        s = Service {
            name: "svc"
            namespace: "ns"
            gateway: {name: "gw", namespace: "ns", hostname: "example.invalid", keySelector: {service: "svc"}}
            versions: {kserve: "v", agentgateway: "v", inferenceExtension: "v", vllm: "v"}
            deployments: [
                {name: "d1", artifact: "a", hardware: "h", image: "image", contextLength: 4096},
                {name: "d2", artifact: "a", hardware: "h", image: "image", contextLength: 8192},
            ]
        }
    })
    assert err == "weighted deployments must expose the same declared contract"
}
