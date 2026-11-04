# Render each global RLS domain once before the service routes that reference it:
#   kcl limits.k -D limit_file=catalog/limits/example-shared.yaml
# The worker and both renderers consume the same tracked budget-caps.json.
# This renders the RLS and config; Redis and its Secret are operator-managed.
# Verify network enforcement and loaded domain before exposing service routes.
import file
import json
import yaml
import templates.gateway as gateway

_limit_file = option("limit_file", required=True)
_budget_file = option("budget_file", default="catalog/budget-caps.json")
_workload_namespace = option("workload_namespace", default="thalamus-inference")
_profile: gateway.LimitProfile = yaml.decode(file.read(_limit_file))
_caps: gateway.BudgetCaps = json.decode(file.read(_budget_file))
assert _profile.namespace == _workload_namespace, "limit profile must use the worker workload namespace"
assert _profile.deployment != Undefined, "limit profile requires managed deployment settings"
_rate_limit = gateway.from_catalog(_profile, _caps)

apiVersion = "v1"
kind = "List"
items = gateway.limiter_resources(_profile, _rate_limit)
