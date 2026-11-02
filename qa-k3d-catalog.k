# Non-model revision probes only: no GPU quota, model or serving evidence.
import file
import yaml

schema Probe:
    generation: int
    check:
        generation > 0

_probe = Probe {**yaml.decode(file.read(".qa/k3d/catalog.yaml"))}
_scope = option("scope", default="inference")
assert _scope in ["inference", "pools"]
apiVersion = "v1" if _scope == "inference" else "scheduling.k8s.io/v1"
kind = "ConfigMap" if _scope == "inference" else "PriorityClass"
metadata = {name: "qa-flux-revision", namespace: "thalamus-inference"} if _scope == "inference" else {name: "qa-flux-priority"}
if _scope == "inference":
    data = {generation: str(_probe.generation), expectedPriority: str(10000 + _probe.generation)}
else:
    value = 10000 + _probe.generation
    globalDefault = False
    preemptionPolicy = "Never"
