# Render from the repository root:
#   kcl main.k -D service_file=catalog/services/example-service.yaml
# Optional -D artifacts_file=..., hardware_file=..., pool_file=...,
# limit_file=..., budget_file=..., and workload_namespace=... override defaults.
# Git/Flux owns these resources. KCL renders desired state; it does not install
# controllers or prove that a Gateway, rate limiter, model, or serving pod is
# actually ready. Mutable credentials are projected separately by the worker.
import file
import json
import yaml
import templates.serving as serving
import templates.features.shared_pool as pool
import templates.gateway.policy as gateway

_service_file = option("service_file", required=True)
_artifacts_file = option("artifacts_file", default="catalog/artifacts.yaml")
_hardware_file = option("hardware_file", default="catalog/hardware.yaml")
_workload_namespace = option("workload_namespace", default="thalamus-inference")
_service: serving.Service = yaml.decode(file.read(_service_file))
_artifacts: {str:serving.Artifact} = yaml.decode(file.read(_artifacts_file))
_hardware: {str:serving.Hardware} = yaml.decode(file.read(_hardware_file))
assert _service.namespace == _workload_namespace, "service must use the worker workload namespace"
_pool_file = option("pool_file", default=("catalog/pools/${_service.pool}.yaml" if _service.pool != Undefined else ""))
_resolved_pool: pool.Pool = (yaml.decode(file.read(_pool_file)) if _service.pool != Undefined else Undefined)
_limit_file = option("limit_file", default="catalog/limits/${_service.gateway.rateLimit}.yaml")
_budget_file = option("budget_file", default="catalog/budget-caps.json")
_limit: gateway.LimitProfile = yaml.decode(file.read(_limit_file))
_budget: gateway.BudgetCaps = json.decode(file.read(_budget_file))
_result = serving.render_service(_service, _artifacts, _hardware, _resolved_pool, _limit, _budget)

apiVersion = _result.apiVersion
kind = _result.kind
items = _result.items
