# Thalamus

Proposed vocabulary for the internal model-access platform, accompanying the September 2026 architecture review.

## Language

**Model offering**:
A stable, user-visible model identity with a declared capability and access contract. It continues to exist while its serving deployments are temporarily unavailable.
_Avoid_: Backend, endpoint

**Model deployment**:
A particular served model revision and runtime configuration with its own availability and capacity. Several compatible deployments may serve one model offering.

**Serving pool**:
A set of compatible model-serving replicas managed as a unit for request placement and capacity protection.

**Team**:
A group of people that owns access policies and a share of inference resources.

**Project**:
A team's unit of workload ownership, access, and usage attribution.

**Workload class**:
The service expectations attached to a workload, such as interactive, deferrable agent, or batch. Class membership does not imply permission to interrupt or replay work.
_Avoid_: Priority header

**Service policy**:
The authorized latency targets, resource limits, and sharing rules that apply to a workload.

**Admission**:
The decision to release a request into a serving pool, wait within a bounded deadline, or reject it because of policy or available capacity.

**Placement**:
The selection of a compatible serving replica for an admitted request.

**Model availability**:
The current ability of an offering to serve requests, distinct from whether the offering is published or a caller may use it.

**Gateway-run tool**:
A tool executed on the caller's behalf as part of an inference request, with its results returned to the model before the request completes.

**Reference spend**:
A valuation of recorded token usage against an explicitly named, dated comparison price. It is neither the infrastructure's actual cost nor proof that an equivalent external service would produce the same results.
_Avoid_: Actual spend, realized savings
