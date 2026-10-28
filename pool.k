# Render each shared GPU pool once, before dependent service manifests:
#   kcl pool.k -D pool_file=catalog/pools/example-pool.yaml
# Kueue owns admission/preemption, KEDA owns scaling, and Git/Flux owns these
# reviewed pool-wide resources. Per-service LocalQueues are rendered by main.k.
import file
import yaml
import templates.features.shared_pool as pool

_pool_file = option("pool_file", required=True)
_workload_namespace = option("workload_namespace", default="thalamus-inference")
_pool: pool.Pool = yaml.decode(file.read(_pool_file))
assert all member in _pool.members {member.namespace == _workload_namespace}, "pool members must use the worker workload namespace"

apiVersion = "v1"
kind = "List"
items = pool.render_pool(_pool)
