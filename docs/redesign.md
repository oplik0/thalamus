# Thalamus v2 — inference operations control plane

Oct 26, 2026 · @opliko

## Summary

Thalamus v2 is a control plane for self-hosted inference on heterogeneous Kubernetes GPUs. It sits beside vLLM, llm-d, KServe, agentgateway and Flux, never in the request path. It turns "serve this model on this hardware with these features, for these teams" into reviewed Git changes, live self-service objects, and evidence that the result works.

v1 was a data-plane router: OpenAI/Anthropic proxying, routing strategies, a priority queue and Wasm plugins. The needs moved to more GPUs of more kinds, and agentgateway, llm-d's endpoint picker and agentic-api now cover the request path. What is missing is the layer that ties them together; today each component is configured separately through KCL and Flux, with manual edits.

**Goals**

- Turn the common deployment changes (new model, image bump, context or batching change, enabling a feature) into a form or an agent proposal that ends in a reviewed PR.
- Show one view per model service: what Git says, what Flux applied, what runs, whether it passes functional checks, how it performs live (latency, throughput, queue depth, KV cache use) and in benchmarks.
- Give users self-service: API keys, budgets, usage by project, ready-made client configuration for coding agents.
- Give agents (pi, opencode, Claude Code, kagent) the same context and the same constrained write paths as the UI, through MCP.
- Make advanced features (agentic-api, tiered KV cache, prefill/decode split) deployable as tested recipes.

## Non-goals

Thalamus wraps upstream components; it never reimplements them. If a feature needs Thalamus to do one of the following, the design is wrong:

- **Proxy inference traffic** or parse token streams. agentgateway, the llm-d endpoint picker and agentic-api own the request path.
- **Schedule or place pods.** Kubernetes, Kueue and KServe own placement and admission; KEDA owns replica counts.
- **Run an agent loop.** pi, opencode, Claude Code or kagent run agents; Thalamus supplies tools and context.
- **Store metrics or logs.** Prometheus and the existing log stack keep them; Thalamus queries and correlates.
- **Generate load.** inference-perf, GuideLLM or llm-d-benchmark generate it; Thalamus schedules runs and keeps results.

Thalamus must also stay optional. If it is down, inference and existing keys keep working, and the GitOps repo stays editable by hand. Only key issuance, revocation and Thalamus-driven changes pause.

## Architecture

Thalamus talks to five things: the Kubernetes API, a Git forge, Prometheus, its own Postgres and the OIDC provider. Clients never reach a model through it.

&#91;embedded content: architecture · request path and control path\]

In text:

- **Request path** (Thalamus is never in it): clients → agentgateway (keys and limits) → agentic-api (optional) → llm-d pool (endpoint picker) → vLLM, one variant per GPU class.
- **Thalamus** (API and web UI, MCP server and CLI, worker for projections and runs, Flux and rollout follower):
  - opens PRs on the GitOps repo (any forge); Flux applies the resulting manifests to the request path;
  - makes live writes of key and project data to agentgateway;
  - reads status from the request path;
  - queries Prometheus (metrics, usage), stores runs, keys and the audit log in Postgres, and logs users in through the OIDC provider.
- **Users and agents** reach Thalamus through the web UI, MCP and the CLI.

Flux applies everything reviewed in Git. Thalamus writes only the key and project `ConfigMap`s it owns, and otherwise reads. agentic-api is in the path only for stateful Responses and Messages calls.

| Concern | Owned by | Thalamus's role |
| --- | --- | --- |
| Running the model | vLLM first; SGLang or other engines later through engine templates | Pick and validate the serving configuration |
| Serving lifecycle | KServe `LLMInferenceService`; raw llm-d charts as an escape hatch | Render intent from templates, explain status |
| Endpoint selection, flow control | llm-d endpoint picker, `InferencePool` | Configure through templates, surface its behavior |
| Autoscaling | KEDA, from llm-d-autoscaling's blueprints; Kueue for shared pools | Template shared GPU pools and autoscaler settings, show scaling decisions |
| Auth enforcement, rate limits, MCP federation | agentgateway | Issue keys and metadata, write the objects it reads, read usage |
| Stateful Responses API | agentic-api | Deploy it as a recipe, verify it |
| Desired infrastructure state | Git + KCL + Flux | Edit data files, open PRs, follow reconciliation |
| Metrics and logs | Prometheus, existing log stack | Query and tie them to a service and revision |
| Agent execution | pi, opencode, Claude Code, kagent | Provide MCP tools, launch debug sessions |
| Load generation | inference-perf, GuideLLM, llm-d-benchmark | Schedule Jobs, keep provenance and results |
| Quota counters | agentgateway's global rate-limit service and its shared store | Set policy and bounded values, show usage and enforcement state |

## Domain model

The UI and API are organized around the **published service**: the stable name users call. Everything else exists to explain how a published service is served and whether it works.

| Concept | What it is | Lives in | Upstream object |
| --- | --- | --- | --- |
| Published service | Stable name (`coding-main`), API surface, capabilities, access policy; points at one or more deployments | Git catalog | `HTTPRoute` + `InferencePool` (+ agentic-api route) |
| Deployment | One recipe applied to one artifact on one hardware profile, with overrides, in a cluster and namespace | Git catalog | `LLMInferenceService` or an llm-d variant |
| Recipe | Engine (vLLM first) and default image, parallelism, context and batching, chat template, tool and reasoning parsers, optional feature components, validation suite, known limits | Git (KCL template + defaults) | A template family |
| Hardware profile | GPU product, memory, GPUs per node, interconnect, CPU architecture, host memory and whether it is shared with GPU memory (as on DGX Spark), driver stack | Git catalog, checked against node labels | Node pool + labels |
| Model artifact | Exact weights, tokenizer, revision, format and quantization | Git catalog | Hugging Face repo at a revision, OCI image or PVC |
| Run | A validation, benchmark or diagnostic against one deployment revision | Postgres; raw results in object storage | Kubernetes `Job` |

**Changes** are durable records like runs: actor, base commit, structured patch, render identity, branch, PR, state and errors. Changes and runs carry idempotency keys, so a retry after a crash finds the branch or Job it already created.

**Mixed hardware.** One published service can have several deployments on different hardware profiles, as variants in one `InferencePool` or as weighted routes across pools. The same mechanism handles blue/green and engine upgrades: add a deployment, validate it, shift weight, remove the old one.

**Engines and images.** A recipe names its engine. vLLM is the only one at first, but engine-specific parts (arguments, metric names, parser options, health checks) live in a per-engine template, so SGLang can be added without changing the model. Each recipe declares the upstream protocols it needs, such as OpenAI chat for the gateway and checks, or Anthropic Messages for agentic-api's Messages route. NVIDIA's [flare-local-inference](https://github.com/NVIDIA/flare-local-inference) is a research prototype for SM120/SM121 GPUs whose README names no API yet.

A deployment may override the recipe's image with any registry, tag or digest, including bleeding-edge and custom builds. Results attach to the image digest, so a custom image becomes Validated only after it passes.

**Status is five separate facts**, shown as columns, never one green dot:

1. **Accepted:** the change is on the default branch.
2. **Applied:** the chain from catalog commit to serving resource is complete for that revision.
3. **Running:** the `LLMInferenceService` is ready with the rendered image digests.
4. **Functional:** the latest validation on this revision passed.
5. **Measured:** a benchmark exists for this revision, or for an identical rendered manifest.

Each status cell records when it was observed and why, and can read Unknown or Stale, so a lost watch never leaves a green cell standing.

**Live performance** sits beside the status columns, from Prometheus: TTFT and inter-token latency percentiles, throughput, queue depth, KV cache use and GPU utilisation per deployment.

**Validation tiers** apply to each tested combination of recipe, model artifact and hardware profile:

- **Validated:** passed the suite at a pinned set of component versions.
- **Expected:** declared requirements match; not tested here.
- **Incompatible:** a known blocker, with the reason recorded.

**Capabilities** come in three kinds: declared (what the recipe or artifact claims), validated (what a test showed for one deployment revision) and published (what callers may rely on). A published service offers only the contract common to every deployment that may serve it: the lowest validated context limit and the features all of them pass. At first only compatible deployments may join a service. The published contract feeds the client configuration generator. A context-limit probe shows the limit is accepted, not that long-context quality holds under load.

**Evidence identity.** Identical manifests do not prove an identical runtime: a tag can move, a PVC's contents can change, and a referenced ConfigMap or the gateway can differ. Each run records four identities: desired configuration (canonical rendered resources and resolved dependencies), observed runtime (image digests, model and tokenizer contents, effective configuration, component versions), test definition (suite, runner and client versions, workload, seed) and run conditions (hardware, replicas, cache state, competing traffic, time). A prior result counts as evidence only when the relevant identities match, and it is shown as a prior run with its date. PVC-backed models need a content manifest or an immutable snapshot.

## Write paths and state ownership

Thalamus has three write paths, and each object has exactly one. The rule of thumb: **logic goes through Git, data that users create on their own goes live.** For example, the `AgentgatewayPolicy` with its CEL rules is reviewed in Git, while the key entries those rules read are written live.

| Change | Path | Why |
| --- | --- | --- |
| New model, deployment, recipe or override | Git PR | Reviewed and reproducible |
| Gateway routes, `AgentgatewayPolicy`, CEL rules, rate-limit descriptors | Git PR | Access logic is reviewed |
| Upper limits on budgets per team | Git PR | Admin decision; CEL enforces the cap |
| Create, expire or revoke an API key | Live: key `ConfigMap` | Self-service must take seconds, not a review |
| Create a project, map keys and projects to teams for usage | Live: copied into each key's metadata | Same |
| Budget values within the cap | Live: key or project metadata | Same |
| Benchmark, validation and debug runs | Live: `Job` with a TTL | Ephemeral |
| Runs, results, audit log, users, teams, projects | Postgres | Neither intent nor cluster state |

**Rules for live objects**

- Postgres is the source of truth for keys and projects. The `ConfigMap`s are projections that a reconcile loop rebuilds, so a deleted or edited `ConfigMap` heals itself.
- Requested state and effective projection are tracked apart: each projection records its generation and errors, and the UI shows "revocation pending" until a probe with the revoked key is rejected.
- Live objects carry `app.kubernetes.io/managed-by: thalamus`, a reserved name prefix, and server-side apply with field manager `thalamus`. Git never defines an object with the same name. Flux prunes only objects in its own inventory, so it leaves these alone.
- Key entries hold the SHA-256 hash and metadata (user, team, project, cost centre, traffic class, allowed models, budget, expiry), never the key itself. The key is shown once at creation.
- A `ConfigMap` holds at most 1 MiB. Shard by team behind a label selector if the key count ever approaches that.
- Every write, Git or live, goes to an audit table with the actor: a user, or an agent acting for a user.

**Read-only state.** What is running, what Flux applied and what the gateway counted is always read from the cluster and Prometheus. Thalamus may cache it but never stores it as truth.

## GitOps workflow

Hand-written KCL modules hold the "how"; Thalamus edits only plain data files those modules import. It renders by calling the same `kcl` CLI and container image that CI uses, never by embedding kcl-lib.

A proposed repo layout:

```
catalog/                  # machine-edited data (YAML)
  artifacts.yaml
  hardware.yaml
  services/coding-main.yaml   # one file per published service + its deployments
templates/                # hand-written KCL, reviewed like code
  serving/kserve.k
  serving/llmd.k
  features/kv_offload.k
  features/agentic_api.k
  gateway/routes.k
  gateway/policy.k
```

One file per published service keeps PRs small and conflicts rare.

Today CI renders the KCL and most of the result deploys as Helm releases. So the rendered diff covers both the release values and the `helm template` output, and "applied" means the `HelmRelease` is ready at that revision. UI forms come from a JSON Schema generated from the KCL schemas, so there is one schema to maintain.

**Change flow**

1. **Propose.** A UI form or an agent produces a structured patch to a data file, never free text edits.
2. **Validate.** Render with the new data; KCL schema checks fail fast with the KCL error.
3. **Diff.** Show the data diff and the rendered-manifest diff against the base commit's render.
4. **Open a PR** on branch `thalamus/<change-id>` through the forge library. The body carries the rendered diff summary, the validation tier of the target recipe and hardware, and who asked.
5. **Merge** according to the change's rollout mode: normal review for gated and rolling changes, approval plus a scheduled window for in-place ones (see Rollouts and promotion).
6. **Follow** the chain from catalog commit to CI output, Flux source, Helm release and serving resource, then run the recipe's checks and attach results to the revision. A later commit can supersede one that Flux never applied on its own.
7. **Roll back** by opening a revert PR from the service page.

**Rules**

- Thalamus never edits hand-written `.k` files. Template changes are ordinary PRs, reviewed like code.
- The repo works without Thalamus: editing a data file by hand has the same effect.
- Upstream churn should land in templates, within a supported composition of pinned versions. Thalamus reads upstream objects as dynamic objects plus a few hand-written fields, so a renamed API group means an adapter change, not regenerated types. Example: within four months llm-d moved the WVA CRD to a new API group (0.8), switched its autoscaling guides to KEDA (0.9), renamed the autoscaler project and deprecated the WVA guides (0.10), then shipped 1.0.
- Existing deployments are imported, not recreated: the first milestone turns the edits repeated across the last \~20 GitOps commits into catalog fields.
- Every render or PR runs in its own worktree from a bare clone, pinned to its base commit; when the base moves, the diff is regenerated.
- The authoritative render check runs in CI, so a Thalamus outage never blocks human PRs. Thalamus calls the same implementation for previews.
- Forms come from the KCL-generated JSON Schema plus a small layer of UI metadata (units, grouping). The schema carries structure only, so KCL checks still run server-side.

## Rollouts and promotion

Deploying a change and exposing it to users are separate steps. Each change gets a rollout mode from the capacity it needs: gated when a second copy fits, in-place when it does not. In-place changes on a single-instance model mean downtime, so they need an owner's approval and a scheduled window.

| Mode | Used when | How it runs | What users see |
| --- | --- | --- | --- |
| Gated | GPUs for a second copy are free, or held by preemptible fillers | Candidate deployment outside the service's routes; validation on an authenticated candidate route; promotion PR moves routing; checks on the public route; removal PR for the old deployment | No downtime |
| Rolling | One extra pod fits and the change needs no gate, such as a probe tweak | Kubernetes rolling update with one surge pod; checks after rollout; revert PR on failure | Mixed versions for a few minutes |
| In-place | No room for a second copy | Owner approval, a scheduled window, update with `Recreate`, checks, revert offered | Downtime for one model load |

Thalamus picks the mode from the hardware profile and the GPUs a new pod could actually get on one node, counting GPUs held by fillers as available. The PR states the mode and why. The recipe sets the workload's update strategy to match, so an in-place update never waits Pending for GPUs that do not exist.

**In-place changes**

- **Approval:** the service's owners approve the PR and pick a window. Merging is the deployment, so the PR stays unmerged until then.
- **Windows** live in the catalog: one-off times, or a recurring slot per service such as nightly at 02:00. At the window Thalamus re-renders against the current base, confirms approvals and CI still hold, and merges; Flux applies it.
- **Guard without Thalamus:** the CI render check flags a PR that changes an in-place service outside its window, so a manual merge at the wrong time is a deliberate override. If Thalamus is down at the window, nothing happens and a person can merge by hand.
- **Expected downtime** comes from evidence: the measured time from pod start to Ready in this deployment's earlier rollouts. The PR, the service page and the user portal show it with the window.
- **During the window** the service has no ready endpoint, so requests fail fast. Optionally a service names a fallback service, such as a smaller model on other hardware, that takes its traffic meanwhile; the portal marks the reduced capabilities.
- **Rollback** costs another model load, so a revert PR reuses the window unless an admin runs it at once.

**Gated changes**

- The candidate gets its own serving resource and pool, reachable only through an authenticated candidate route with validation keys. KServe's own rollout weights are the alternative; one of the two owns the route fields, never both.
- Validation records which backend answered, so a test cannot pass by reaching the old deployment.
- Test the moment a backend goes from no traffic to live with agentgateway before relying on it.
- In shared GPU pools, fillers are rollout headroom: a candidate at normal priority preempts fillers, so a model with one serving copy can still roll out gated.

## Forge library

Thalamus talks to GitHub, GitLab and Forgejo through one Rust library: a unified data model with the existing per-forge crates as adapters, modelled on [forges](https://github.com/danielroe/forges). It ships as its own crate in the workspace and can be published separately once it proves out.

**First adapter: GitHub Enterprise**, which hosts the GitOps repo today; octocrab takes a custom base URL for it. GitLab and Forgejo follow. Gitea probably works through forgejo-api but is not a priority.

**Commits go through Git, not the forge API.** Thalamus already needs a checkout to render KCL, so it commits there and pushes with a token or deploy key. That keeps multi-file commits identical on every forge. The forge API handles only review objects: change requests, statuses, comments and webhooks.

| Adapter crate | Version | Style | Notes |
| --- | --- | --- | --- |
| [octocrab](https://github.com/XAMPPRocky/octocrab) | 0.54.5 | Typed handlers plus raw `_get`/`_post` | GitHub App auth; raw requests cover gaps |
| [gitlab](https://gitlab.kitware.com/utils/rust-gitlab) | 0.1905.0 | Endpoint builders run through `Query` / `AsyncQuery`; caller-defined response types | Tracks GitLab 19.5 API v4; version number follows GitLab |
| [forgejo-api](https://codeberg.org/Cyborus/forgejo-api) | 0.12.0 | Generated from Forgejo's OpenAPI document; async and blocking | Covers PRs, merges, commit statuses, hooks |

The gitlab crate lets callers define their own response structs, so its adapter can deserialize straight into the unified model.

**Unified model (first cut)**

- `RepoRef`: forge kind, host, owner or namespace path, name, parsed from a clone or web URL.
- `ChangeRequest`: number, title, body, source and target branch, head SHA, state (open, merged, closed), draft, mergeability (including unknown), base, head and merge commits, source repository, author, labels, URL.
- `Check`: name, status (queued, in progress, completed) and a separate conclusion (success, failure, neutral, cancelled, skipped, timed out, action required), description, URL. Commit statuses on all three forges and GitHub check runs map to it, plus a combined state per SHA.
- `Comment` on a change request, and `Review` state where supported.
- `WebhookEvent`: change request opened, updated, merged or closed; push; check updated. Signature checks are per forge: GitHub HMAC-SHA256, GitLab's shared token, Forgejo's HMAC.

**Design decisions**

- One cargo feature per adapter (`github`, `gitlab`, `forgejo`), so a deployment pulls in only what it uses.
- Per-resource traits (`ChangeRequests`, `Checks`, `Comments`, `Webhooks`) implemented by each adapter, plus a `Forge` enum for dispatch.
- A capability matrix declared as data per adapter, as forges does, so callers check support instead of matching on errors. Support can also depend on forge version, credentials and repository settings.
- Pagination returns a `Stream` of results.
- Each adapter exposes its underlying client, so forge-specific needs never widen the model.
- Tests: recorded fixtures per forge, plus integration tests against a Forgejo container in CI.

**What Thalamus uses it for.** Opening and updating its own PRs, following their state, merging scheduled in-place changes, and adding an advisory rendered-diff comment to every PR on the GitOps repo, including hand-written ones. The authoritative render check runs in CI. Webhooks trigger reconciliation, and periodic reads recover missed events.

## Identity, keys and usage

Users log in with OIDC. Thalamus issues API keys, and agentgateway enforces them from a `ConfigMap` of hashed keys with metadata, using CEL rules kept in Git.

**Login.** OIDC only, against the existing identity provider. A groups claim maps to Thalamus roles and, optionally, teams. Platform admins come from a configured group. The UI uses a session cookie backed by a Postgres session table and a signing key from a Kubernetes Secret, so redeploys and extra replicas never log anyone out; v1's PASETO, OPAQUE and HTTP-signature code does not return.

**Keys**

- Format: `thl_` plus 32 random bytes. Postgres stores the SHA-256 hash, which is projected into the key `ConfigMap`. The key is shown once.
- Metadata per key: user, team, project, allowed services, budget, expiry, creator. CEL rules in the `AgentgatewayPolicy` read it, for example to reject a model outside the key's allowed services.
- Expiry is enforced twice: CEL checks it per request, and Thalamus removes expired entries on a timer.
- Revocation removes the entry. It takes effect when agentgateway picks up the `ConfigMap` change; that is fast enough in practice.
- Three kinds: personal keys; project keys for CI and long-running agents; short-lived keys Thalamus mints for its own debug and benchmark Jobs.
- Offboarding: disabling a user in the identity provider revokes their personal keys at the next sync; their project keys move to another project member or are revoked.

**Budgets and rate limits.** agentgateway's token-based rate limits use descriptors built from key metadata (user, project, team); limits come from that metadata, clamped by caps kept in Git. Counters live in agentgateway's global rate-limit service and its shared store, not in Thalamus, so a budget holds across gateway replicas. The policy states its unit and reset window, which levels apply together, and what happens when the counter service is down. Tokens are counted after a response, so a request admitted under budget can finish over it: budgets are allowances, not hard spending ceilings.

**Usage**

- The source is gateway telemetry: tokens per request with key metadata, in Prometheus and the logs. Prometheus labels stay bounded to service, project, team and traffic class, never key IDs. Thalamus rolls usage up daily in Postgres for history and quota display; these summaries are for display, not exact accounting.
- Projects stay canonical in Postgres; project, team and cost centre are copied into each key's metadata, so the gateway, telemetry and Thalamus agree. Usage keeps the attribution it had at request time, so moving a project to another team does not rewrite history.
- agentic-api turns one public request into several inference rounds. Usage is counted at those internal model calls, tagged with the original project and team; outer requests are a separate metric, so nothing is counted twice (see Feature recipes).

**Authorization inside Thalamus.** Three roles to start: platform admin, team admin, member, scoped by team. A plain role table is enough at first; Casbin returns only if policies outgrow it.

**Agents act as a user.** An agent calling the MCP server authenticates as the user it works for, gets that user's permissions, and the audit log records both.

**Client configuration.** For each published service and client (opencode, pi, Claude Code, Codex), the portal generates a ready config: base URL, API flavour, a key placeholder or a newly issued key (stored keys are hashes and cannot be shown again), validated context limit, tool and reasoning flags, MCP endpoints. Generators are versioned per client major version. A "check my setup" button runs one tool-call round trip with a key the user pastes in.

## Shared GPU pools and traffic classes

Two controls decide who gets GPUs. Shared pools split a fixed set of GPUs across models by load; traffic classes decide which requests run first and on which hardware. v1's priority queue idea survives as configuration of upstream queues.

### Shared GPU pools

N GPUs serve K models. With no load, each model gets an equal share; as load shifts, shares follow it.

Upstream autoscaling alone does not do this. [llm-d-autoscaling](https://github.com/llm-d/llm-d-autoscaling) now holds KEDA blueprints that scale each model on its own signals (queue depth, KV cache use); its WVA controller is deprecated on `main`. [Kueue](https://github.com/kubernetes-sigs/kueue) supplies the missing half, so Thalamus renders configuration instead of running its own allocator:

1. A pool in the catalog becomes a Kueue cohort on one `ResourceFlavor`, the hardware profile.
2. Each member model gets a ClusterQueue whose nominal quota is its share of the GPUs, with borrowing allowed.
3. Each model runs two replica sets in the same `InferencePool`: load-driven replicas scaled by KEDA, and low-priority filler replicas sized to its even share.
4. Fillers go to a separate ClusterQueue with zero nominal quota. Kueue preempts only workloads that use more than their queue's nominal quota, so fillers must always be borrowing.
5. With no load, fillers hold every GPU in an even split. Under load, a model's KEDA replicas reclaim its share from fillers (`reclaimWithinCohort: Any`) and borrow beyond it (`borrowWithinCohort: LowerPriority`).
6. When a second model gets busy, it reclaims its share from the first model's borrowed replicas. When load falls, KEDA scales down and the preempted fillers are admitted again.

- **Known costs:** Kueue admits a Deployment's pods one by one (plain Pod integration) and supports LeaderWorkerSet for multi-node replicas. Preemption may pick a model's own filler first, costing an extra model load; Kueue's alpha `PreemptionConfig` selectors may avoid that.
- **Slow on purpose:** a large model takes minutes to load, so KEDA scale-down uses long stabilization windows.
- **Optional like the rest:** Thalamus only renders queues, priorities and KEDA objects through Git. If it stops, nothing changes at runtime.
- **Fallback:** if preemption churn costs too much, a small allocator in the worker that writes replica counts directly is plan B.
- **One scaling mode per deployment:** fixed, keda or shared-pool. Templates make sure no other HPA or controller acts on the same workload.
- **Whole serving units:** quota counts GPUs, but a TP=4 replica needs four free GPUs on one node. Pool members use replica sizes that divide the nodes evenly, or GPUs strand.
- **Missing metrics mean unknown, not zero demand.** The first experiment stays narrow: one hardware profile, single-node replicas, no scale to zero.

### Traffic classes

Classes such as `interactive`, `standard` and `batch` live in the catalog. Each has a priority, a queue timeout, allowed hardware profiles in order of preference, and rate limits.

- **Assignment:** key or project metadata sets the class. agentgateway strips client-sent `x-llm-d-inference-*` headers and sets them with CEL from `apiKey` metadata: `x-llm-d-inference-objective` from the class, `x-llm-d-inference-fairness-id` from the project. A request may lower its class, never raise it.
- **Inside the engine:** recipes turn on vLLM's priority scheduling (`--scheduling-policy priority`). The llm-d router writes the resolved priority into each request's `priority` field when `requestHandler.propagatePriority` is on (off by default), replacing anything the client sent. A nonzero priority fails on a server without priority scheduling, so Thalamus renders both from one class definition. A test checks the sign conversion: llm-d treats higher as more urgent, vLLM runs lower first.
- **Before the engine:** the llm-d router's flow control queues requests in priority bands, one `InferenceObjective` (`llm-d.ai/v1`) per class, round-robin across fairness IDs within a band. Here higher means more important, and negative priorities are sheddable, with per-band queue limits, a TTL and optional eviction of in-flight sheddable requests. Each class therefore stores both numbers. Flow control sits behind the `flowControl` feature gate, off by default, so M8 turns it on and load-tests it.
- **Hardware preference, deferred:** weighted routes split traffic; they do not send interactive requests to fast hardware until it saturates. Fastest-first spillover needs a named endpoint-picker scoring setup or agentgateway failover, chosen and tested in M8. Until then each class maps to explicit pools.
- **Deep queues:** batch and background jobs go to [llm-d-async](https://github.com/llm-d/llm-d-async). It pulls from a queue and dispatches only when its gates see spare capacity, for example from Prometheus saturation signals.
- **llm-d-async details:** it supports Redis sorted sets (recommended), GCP Pub/Sub and an experimental Postgres transport. Each request needs a deadline, and queues carry `interactive`, `async` or `batch` tiers.
- **Batch API:** [llm-d-batch-gateway](https://github.com/llm-d/llm-d-batch-gateway) (1.0.0 in llm-d 1.0) is an OpenAI-compatible `/v1/batches` and `/v1/files` service. It keeps metadata and its priority queue in Postgres and files in S3, dispatches to the llm-d router, and can use llm-d-async as an optional dispatcher. It scopes data to the authenticated tenant but expects the gateway to say who that is; its docs use Kuadrant/Authorino, so the agentgateway mapping needs a test. It stays out of scope until batch demand is real, since accepting jobs brings cancellation, idempotency, results and retention with it.
- **Validation:** a mixed-class load test passes when interactive latency stays within its SLO while the batch backlog grows. It also covers cancelling queued requests, a failed metrics scrape, an endpoint restart, timeouts that agree between gateway, picker and engine, and batch deadlines.

## Validation and benchmarking

A deployment revision counts as working only after it passes functional checks. Benchmarks are tied to the exact rendered configuration, so results stay comparable across upgrades.

| Level | Question it answers | Path | When | Gates the deployment |
| --- | --- | --- | --- | --- |
| Functional | Do clients work? Streaming, a tool-call loop with the configured parser, reasoning output, Responses continuation, a context-limit probe | Candidate route before promotion, public route after | Before promotion, after every rollout, nightly | Yes: gates promotion |
| Client smoke test | Does each supported client work end to end? `opencode run` and `pi -p` with a fixed prompt | Public route | After every rollout | Sets client capabilities |
| Engine performance | What can this hardware and configuration do? | An engine endpoint or internal Service selecting one revision | On demand, on recipe change | No |
| End-to-end performance | What do users get through gateway, picker and agentic-api? | Public route | On demand | No |
| Quality | Did a quantization or upgrade break the task? A small fixed coding and tool-use suite | Public route | On recipe change | Optional |

An `InferencePool` is a routing object, not an HTTP endpoint, so paths are explicit: engine results come from one revision's endpoint, serving-pool results from an internal route through the endpoint picker, and user-visible results from the public route with its policies and agentic-api.

**Runners.** Start with [inference-perf](https://github.com/kubernetes-sigs/inference-perf), which has shared-prefix and multi-turn workloads, load stages, trace replay and SLO goodput. GuideLLM and llm-d-benchmark can follow through the same Job contract: runner image, workload spec, a short-lived key, CPU and time limits, results written to object storage plus a summary JSON that Thalamus ingests. Each run pins its measurement semantics: runner image, tokenizer, seed, load model (fixed rate or fixed concurrency), warmup and token counting. For reasoning models, time to first token and time to first answer token are reported separately.

**Provenance per run:** service, deployment, commit, rendered-manifest hash, image digests, model revision, hardware profile and node names, component versions (vLLM, llm-d, KServe, agentgateway), workload spec, cache warmth (cold or warm prefix cache), and production traffic on the same pool during the run. The cluster is shared, so runs during heavy traffic are flagged.

**Workloads shaped like coding agents.** Coding agents reuse long prompt prefixes, so random-prompt benchmarks undersell prefix caching and KV offload. Start with curated workloads resembling that traffic, built with inference-perf's shared-prefix and multi-turn generators and tuned by the length distributions gateway telemetry does show. Aggregate telemetry cannot show which requests shared a prefix or a session, so richer shapes need extra instrumentation and the owners' permission.

**Comparison.** The UI shows curves, not single scores: TTFT and inter-token latency at p50 and p95 against request rate, and goodput at the SLO set for each published service. It compares only runs with the same workload spec and flags differing conditions.

**Promotion.** A run's page has "promote this configuration", which opens a PR pinning the tested image digests and artifacts rather than tags that may have moved. The PR follows the service's rollout mode.

## Agent integration

Agents get a Thalamus MCP server whose tools connect Git, the cluster, metrics and runs. They change things only through the same PR path and live writes the UI uses, with the permissions of the user they act for.

| Tool | What it does | Kind |
| --- | --- | --- |
| `get_service_context` | Joins a published service to its deployments, Git source, rendered resources, status columns and recent runs | Read |
| `compare_revisions` | Diffs two revisions: data, rendered manifests, validation and benchmark results | Read |
| `query_metrics` | Preset Prometheus queries for one service; free-form PromQL only for admins | Read |
| `get_logs` | Recent logs from deployments the user may see; other teams' prompts stay hidden | Read |
| `check_compatibility` | Recipes and their validation tier on each hardware profile | Read |
| `run_validation`, `run_benchmark` | Start a run from a named runner and template with bounded parameters, never a raw Job spec | Bounded execution |
| `get_run` | Progress and results of a run | Read |
| `propose_change` | Structured patch to a catalog file, rendered diff, then a PR | Write through Git |

- **Transport:** streamable HTTP; pi rejects the legacy SSE transport. Thalamus acts as an MCP resource server, with the token audience and discovery metadata the MCP authorization spec requires.
- **CLI too:** a `thalamus` CLI with JSON output wraps the same API, for agents and people who prefer shell tools.
- **No generic Kubernetes tools.** Pair Thalamus with Flux's MCP server in read-only mode for raw inspection.
- **Benchmarks cost GPU time.** Runs against production pools need an admin role or come out of a team's run budget.

**Debug sessions** (the Debug button on a service or failed run)

1. Thalamus creates a Job running `opencode serve` with a checkout of the GitOps repo, and proxies its HTTP API to the UI. pi's stdio RPC mode can follow later; one transport first.
2. The Job gets the Thalamus and Flux MCP servers, a read-only ServiceAccount for the service's namespace, and a short-lived key for one of your own models.
3. It gets **no forge credentials**: changes go through `propose_change`, so Thalamus does all Git work and attribution.
4. Thalamus seeds the session with the service context and the failing evidence.
5. The UI streams the session's events and lets the user reply.
6. A deadline and a model and tool budget bound the running session. The transcript and outcome go to Postgres, and the Job's TTL cleans up afterwards.

Running debug agents on your own models doubles as continuous tool-calling validation. kagent stays optional, for long-lived hosted agents such as one that watches alerts.

## Feature recipes

Each advanced feature is a recipe: a KCL template with defaults, its resource requirements, a validation suite, and the hardware profiles where it is validated.

**Template families.** KServe's `LLMInferenceService` is the default, since it creates the `InferencePool`, endpoint picker and routes. Raw llm-d charts are the escape hatch for features KServe has not caught up on. Each recipe pins a tested composition of vLLM image, llm-d, KServe, Gateway API Inference Extension and agentgateway versions. Even the agentgateway and KServe guide is tested against one specific set, including a KServe release candidate.

**Baseline route.** Every recipe routes through an `AgentgatewayBackend` wrapping the `InferencePool` and overrides the KServe route to use it, because token-based rate limits and LLM telemetry need agentgateway to parse requests. The first acceptance test sends a real request through the public route and checks authentication, authorization, attribution and token limiting together.

### agentic-api

[agentic-api](https://github.com/vllm-project/agentic-api) is a Rust/Axum layer that adds stored state, tool execution and multi-turn orchestration for the Responses and Messages APIs. It forwards stateless calls to an upstream set by `--llm-api-base`, uses Postgres for shared sessions, and has an `agentic-llm-d` crate for llm-d.

- **Topology:** public `/v1/responses` and `/v1/messages` on agentgateway go to agentic-api, which calls the `InferencePool` through a separate internal route so requests cannot loop.
- **Isolation:** stored state is not isolated per user today. In [issue #107](https://github.com/vllm-project/agentic-api/issues/107), still open, a report on 0.9.0 found responses read by ID with no owner check. `store:false` does not help on its own: a stored `previous_response_id` or `conversation` still supplies earlier history. So the recipe enforces one of two modes rather than leaving it to clients. **Shared stateless** rejects state references and disables the stored-state routes. **Team-shared state** runs one instance and database per team, and the team is the sharing boundary. Per-user state waits for owner checks upstream.
- **Identity chain:** authentication and attribution are separate. agentgateway verifies the user's Thalamus key, strips client-sent attribution headers, sets trusted ones (user, project, team, class), and calls agentic-api with a shared service key from a Secret. agentic-api preserves end-to-end headers on each inference round. The internal route accepts attribution headers only alongside agentic-api's service key. Each supported operation gets its own test, and unsupported ones stay off. Longer term, contribute a trusted-gateway owner header to issue #107, whose open thread asks exactly that.
- **Usage:** counted at the internal model calls, tagged with the original project and team. Outer requests are a separate metric, so nothing is counted twice.
- **Validation:** each supported operation separately (continuation, streaming, the server-side tool loop, compaction), attribution on every internal round, and for team-shared state a check that another team's key cannot read a stored response.

### Tiered KV cache

- llm-d documents native vLLM offloading, LMCache and Mooncake. Start with native offloading.
- vLLM's native CPU tier lives in `/dev/shm`, which counts toward pod memory. The recipe changes the offload size, the pod memory limit, the shm size and the endpoint picker's prefix-cache settings together.
- The hardware profile's host memory per node decides whether the recipe is eligible.
- Validation runs the same multi-turn, shared-prefix workload with and without offload and compares TTFT at fixed load and memory use.
- The UI says what it does: keeps reusable prefix blocks outside GPU memory. It does not make longer contexts fit.

### Later

Prefill/decode split and speculative decoding, once benchmarks on your own traffic show a gain. Prefill/decode split also needs an interconnect field on the hardware profile.

## Tech stack and what carries over

Thalamus v2 is Rust in a new workspace. From v1, only the name and a few domain types carry over, copied in as reference: about 3.3k of v1's 24k lines of Rust.

```
crates/
  thalamus-core     # domain types: service, deployment, recipe, profile, run
  thalamus-server   # Axum API, MCP server, serves the UI
  thalamus-worker   # loops: key and project projection, Flux follower, run ingestion
  thalamus-gitops   # checkout, kcl rendering, data-file patches, diffs
  thalamus-k8s      # kube-rs client, dynamic objects
  forge             # forge library (working name), one feature per adapter
  thalamus-cli      # CLI over the API
ui/                 # web app
```

Server and worker can ship as one binary with two roles; the worker loops take a Kubernetes `Lease` so only one replica runs them. A Lease is not fencing, so every external action carries the idempotency key of its change or run.

**Main dependencies**

- axum, tokio, sqlx with Postgres, as in v1.
- kube and kube-runtime for watches. Upstream objects are read as `DynamicObject`s plus small hand-written structs for the fields Thalamus uses (metadata, replicas, conditions), so an upstream schema change does not mean regenerated types.
- rmcp for the MCP server, reusing what v1's MCP gateway work taught.
- openidconnect for login.
- The `git` and `kcl` CLIs, shelled out, so Thalamus behaves exactly like CI and people do.
- object\_store for benchmark artifacts on S3-compatible storage.
- A Svelte 5 web UI: SvelteKit in SPA mode served by Axum, built from m3-svelte (Material 3) components and theme tokens rather than custom CSS, with a TypeScript client generated from the API's OpenAPI document. m3-svelte has no data table, so tables are plain HTML styled with its tokens. Flows from the existing custom frontend are ported onto this stack.

**v1 modules**

| v1 code | Fate | Why |
| --- | --- | --- |
| `teams/` (teams, projects, memberships, hierarchy) | Copy as reference | Still the organisation model |
| `auth/domain/api_key.rs`, `key_storage.rs` | Copy as reference | Key record shape; hash changes from Argon2 to SHA-256, which agentgateway compares; fine for 256-bit random keys |
| `authorization/`, `casbin_model.conf` | Defer | Plain roles first |
| `users/` | Rewrite | Users now come from OIDC |
| Rest of `auth/`: OPAQUE, PASETO, signing keys, rotation, HTTP signatures, revocation | Drop | OIDC plus session cookie; v1's own retrospective calls the security work overbuilt |
| `llm_proxy/`, `routing/`, `backends/` | Drop | The data plane is upstream |
| `plugin/`, Extism, `crates/thalamus-plugin`, `examples/plugins/` | Drop | No request path to extend |
| `batch/` | Drop | Built on `ProxyService` |
| `mcp/`, including the rmcp gateway | Drop | agentgateway federates MCP |
| KCL loader and watcher, forked kcl crates | Drop | Shell out to the `kcl` CLI |
| `ui/` (Expo, React Native for Web) | Drop | v1's retrospective records the cost |
| mise tasks, CI, `#[sqlx::test]` patterns | Keep | Still fit |

Tag the current default branch `v1-final` before replacing the tree, so v1 stays reachable and the repo keeps its name, issues and history.

## Milestones

Nine milestones, each useful on its own; M2 is where hand edits to KCL stop. No dates are set yet.

&#91;embedded content: milestones · dependencies and exit criteria\]

| Milestone | Depends on | Scope | Exit criterion |
| --- | --- | --- | --- |
| M0 · Catalog from repeated edits | — | Mine the last \~20 GitOps commits; catalog data files plus KCL templates, no code | A repeated edit is a one-file data change |
| M1 · Read-only service view | M0 | Catalog joined with Flux, serving and pool status; five status columns, vLLM metrics | One real service shows every column right |
| M2 · Changes through PRs | M1 | Forge library, first adapter; one change type; render check in CI, advisory diff on every PR | An image bump goes from form to running |
| M3 · Validation and benchmarks | M2 | Functional suite and inference-perf Jobs; results keyed to runtime and test identity | An upgrade shows before and after curves |
| M4 · Rollouts: promotion and scheduling | M3 | Candidate routes, then a promotion PR; in-place mode with approval and a scheduled window | A one-instance model updates on schedule |
| M5 · Self-service | M1 | OIDC, keys via ConfigMap and CEL, projects; usage rollups, client config generator | A user gets a key and working config alone |
| M6 · Agents | M3 | MCP server and CLI, then Debug sessions; changes only through propose\_change | An agent turns a failed rollout into a PR |
| M7 · Feature recipes | M3 | Tiered KV cache first, then agentic-api; each with its own validation suite | Both validated on one hardware profile |
| M8 · Shared pools and traffic classes | M2, M5 | Kueue pools, flow control, priority propagation; batch gateway only once batch demand is real | Batch waits, interactive keeps its SLO |

M5 needs only M1's server, so self-service can move ahead of M2 if key handling in the current frontend hurts more than deployment edits. M8 needs M2's templates and M5's key metadata. Porting key self-service and client setup first, next to the M1 service view, replaces the current frontend soonest.

## Risks and open questions

The biggest risk is scope: written out, this is a whole platform for one maintainer. Every milestone below is useful on its own, and the non-goals are the tripwires.

| Risk | Mitigation |
| --- | --- |
| Scope grows past one maintainer | Non-goals as tripwires; each milestone useful alone; Thalamus stays optional |
| Upstream churn in llm-d, the Inference Extension, KServe, agentgateway, agentic-api | Logic in templates; pinned compositions; CRD types generated per composition |
| Recipe × hardware matrix outgrows validation capacity | Small supported matrix; the Expected tier says plainly what is untested |
| Benchmarks compete with production for GPUs | Run budgets per team; runs during heavy traffic are flagged |
| Someone edits a live `ConfigMap` by hand | Reconcile loop rebuilds it from Postgres; audit log shows the diff |
| In-place updates take a single-instance model offline | Owner approval, scheduled windows, a measured downtime estimate, an optional fallback service |
| Old results reused for a runtime that changed | Evidence counts only when runtime and test identities match; prior runs show their date |

**Open questions**

- [x] Which key metadata can agentgateway's CEL rules and rate-limit descriptors read on the deployed version? Answer: all metadata set on the key entry.
- [x] How long does a key revocation take to reach agentgateway after the `ConfigMap` changes? Answer: fast enough in practice.
- [x] How is KCL rendered for Flux today: CI to a rendered branch, an OCI artifact, or a plugin? Thalamus must call the same thing. Answer: CI renders it; most of it deploys as Helm releases.
- [x] How should identity pass from agentgateway to agentic-api? Answer: agentgateway verifies the user's key, sets trusted attribution headers and calls agentic-api with a shared service key; push a trusted-gateway owner header upstream.
- [x] Which flows from the existing custom frontend move over first? Answer: API keys and client setup, next to the M1 service view.
- [x] Which forge hosts the GitOps repo, and so which adapter comes first? Answer: GitHub Enterprise.
- [x] Does forgejo-api work against Gitea, or does Gitea need its own adapter? Answer: probably works; not a priority.
- [x] Should UI forms use a JSON Schema generated from the KCL schemas, or one kept beside them? Answer: generated from KCL.
- [x] Is S3-compatible object storage available for benchmark artifacts? Answer: yes.
- [x] Who sets the latency targets (TTFT, inter-token latency) for each published service? Answer: defaults per traffic class in Git, overridable per published service; owners propose, a platform admin checks feasibility.
- [x] How does a request's traffic class reach vLLM's per-request `priority`: set by agentgateway, the endpoint picker or the client? Answer: agentgateway sets llm-d's objective and fairness headers from key metadata, and the llm-d router writes the priority into vLLM's field.
- [x] Is the endpoint picker's flow control mature enough on the deployed version? Answer: it ships in llm-d 1.0 behind the flowControl feature gate, off by default; M8 enables and load-tests it.
- [x] Who accepts batch jobs for llm-d-async: an upstream producer, or a small Thalamus service outside the control-plane binary? Answer: llm-d-batch-gateway, an OpenAI-compatible upstream service.
- [x] Could Kueue's fair sharing replace the shared-pool allocator? Answer: yes, with fillers in a zero-quota queue; see Shared GPU pools.
- [ ] Does agentic-api preserve trusted attribution headers on every inference round, for each supported operation? Test in M7.
- [ ] Which header does llm-d-batch-gateway read for the tenant, and can agentgateway set it from key metadata?
- [ ] Does Kueue preempt a model's own fillers before other models' fillers, and can `PreemptionConfig` prevent it?
- [ ] Which services are in-place only today, and which maintenance windows suit their users?
- [ ] Can agentgateway fail a service over to a named fallback service while it has no ready endpoint?
- [ ] How does a backend behave when it goes from no traffic to live behind agentgateway?
- [ ] Does the pinned llm-d router convert priority signs correctly for vLLM?

## Sources

- [vllm-project/agentic-api](https://github.com/vllm-project/agentic-api): architecture, storage, auth, and tenant-scoped state listed as future work.
- [llm-d releases](https://github.com/llm-d/llm-d/releases): WVA CRD moved to the `llm-d.ai` API group (0.8.0, 24 Jun), autoscaling guides moved to KEDA (0.9.0, 17 Aug), autoscaler renamed to llm-d-autoscaling and WVA guides deprecated (0.10.0, 29 Sep), 1.0.0 (20 Oct).
- [pi MCP docs](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/mcp.md): stdio and streamable HTTP; SSE rejected.
- [danielroe/forges](https://github.com/danielroe/forges): the model for the forge library.
- [octocrab](https://crates.io/crates/octocrab), [gitlab](https://crates.io/crates/gitlab) and [forgejo-api](https://crates.io/crates/forgejo-api) on crates.io: versions as of this doc's date; the gitlab and forgejo-api descriptions come from their published sources.
- [Thalamus v1 write-up](https://github.com/oplik0/thalamus/blob/ba88cc4f08fc0a74618cf84ce95d644c8420eed7/docs/documentation.typ): over-built security, React Native for Web cost, kcl-lib pain.
- agentgateway docs, read from the source repo [agentgateway/website](https://github.com/agentgateway/website): virtual keys (`ConfigMap` of SHA-256 hashes, metadata, `configMapSelector`), LLM request transformations, and the CEL context reference (`apiKey` metadata flattened into CEL).
- [llm-d-async](https://github.com/llm-d/llm-d-async): queue transports, deadlines, dispatch gates, SLA tiers; no user-facing API.
- [llm-d-autoscaling](https://github.com/llm-d/llm-d-autoscaling): KEDA blueprints and a test bed; the WVA controller is deprecated on `main`.
- [NVIDIA/flare-local-inference](https://github.com/NVIDIA/flare-local-inference): research prototype for SM120/SM121 GPUs.
- [m3-svelte on npm](https://www.npmjs.com/package/m3-svelte): version 7.2.1, peer dependency Svelte 5.40 or later; component list read from the published package.
- [agentic-api API reference](https://github.com/vllm-project/agentic-api/blob/main/docs/api/index.md) and [issue #107](https://github.com/vllm-project/agentic-api/issues/107): OIDC or shared-key auth, token not forwarded, open owner-scoping work.
- [llm-d flow control](https://github.com/llm-d/llm-d/blob/main/docs/architecture/router/flow-control.md): priority bands, fairness-ID and objective headers, feature gate.
- [llm-d-router](https://github.com/llm-d/llm-d-router): `InferenceObjective` moved here from the Inference Extension.
- [Gateway API Inference Extension releases](https://github.com/kubernetes-sigs/gateway-api-inference-extension/releases): v1.6.0 moved the full EPP to llm-d.
- [llm-d-batch-gateway](https://github.com/llm-d/llm-d-batch-gateway): OpenAI-compatible batch API.
- [vLLM scheduler config](https://github.com/vllm-project/vllm/blob/main/vllm/config/scheduler.py) and [chat protocol](https://github.com/vllm-project/vllm/blob/main/vllm/entrypoints/openai/chat_completion/protocol.py): `fcfs` or `priority` policy, per-request `priority` field.
- Kueue docs: [Deployments](https://github.com/kubernetes-sigs/kueue/blob/main/site/content/en/docs/tasks/run/deployment.md), [ClusterQueue preemption](https://github.com/kubernetes-sigs/kueue/blob/main/site/content/en/docs/concepts/cluster_queue.md), [preemption](https://github.com/kubernetes-sigs/kueue/blob/main/site/content/en/docs/concepts/preemption/_index.md).
- [llm-d-router openai-parser](https://github.com/llm-d/llm-d-router/blob/main/pkg/epp/framework/plugins/requesthandling/parsers/openai/README.md): `requestHandler.propagatePriority`, off by default.
- [agentgateway KServe guide source](https://github.com/agentgateway/website/blob/main/assets/agw-docs/pages/integrations/kserve.md): `AgentgatewayBackend` wrapping the `InferencePool` for token limits and LLM telemetry.
