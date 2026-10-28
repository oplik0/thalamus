# Thalamus v2

Thalamus v2 is an inference operations **control plane** for self-hosted Kubernetes deployments. It is not a router or inference proxy: the inference request path remains with components such as agentgateway, llm-d and vLLM. The design, ownership boundaries and milestone plan are in [docs/redesign.md](docs/redesign.md).

The v2 workspace contains the control-plane libraries, `thalamus-server`, `thalamus-worker` and Svelte UI in `ui/`. Fixtures are not production configuration. See [TESTING.md](TESTING.md) for reproducible local checks and their limits.

## Greenfield GitOps inputs and installation

The repository supplies an initial catalog, templates and installer; no prior cluster, private repository or deployment history is required. Operators must supply real endpoints, pinned images, credentials, permissions and infrastructure for an installation. The committed examples are offline fixtures, not imported deployments.

```sh
kcl main.k -D service_file=catalog/services/example-service.yaml
kcl limits.k -D limit_file=catalog/limits/example-shared.yaml
kcl pool.k -D pool_file=catalog/pools/example-pool.yaml
kcl deploy/install.k -D config_file=install.fixture.yaml
```

Service, pool and limits entrypoints run from the repository root. Installer `config_file` paths resolve relative to `deploy/`, so the last command uses `deploy/install.fixture.yaml`; an absolute operator-owned path also works. Replace fixture values with reviewed real inputs before deployment. `limits.k` and `pool.k` render their shared resources once, independently of per-service renders; budget caps are tracked in `catalog/budget-caps.json`. Regenerate the service form schema with `mise run kcl:schema`; CI checks `catalog/service.schema.json` against its KCL source. Rendering or schema validation does not establish runtime admission, scheduling or production readiness.

The installer fixture's Thalamus, runner and debug image digests and example model digests are synthetic; its hosts, CIDRs and Secret names are examples, not provisioned resources. Replace them with operator-verified inputs. The Redis default pins the official `redis:7.4.3` OCI index (`docker.io/library/redis@sha256:236e397c1d5ab7a94adaf1a51eec3ca8333b05fafcd6d423c6c7cc5987e519a0`).

The installer manages a distinct data-plane `servingGateway` in the workload namespace and the shared namespaced raw EPP `thalamus-epp` service account; the control-plane Gateway is separate. Each service requires `network.gatewayPodSelector`, `dns` and `prometheus` namespace/pod selectors, and `apiServer` CIDRs and ports. `storageEgress` is optional for packaged or local models but required for nonpackaged remote storage, including its download, CDN and authentication destinations. Administrators must review catalog network changes, protected labels, the enforcing CNI, pre/post-DNAT API destinations, DNS ingress, node-local and host-network exceptions, and peer policies including Prometheus egress. Policies are additive and Pod-wide, not URL- or container-scoped: download egress also reaches the model container, and trusted Prometheus/EPP scrapers with access to port 8000 can reach the inference API.

The fixture names the managed public serving Gateway `inference-serving` (not the admission-reserved `thalamus-` prefix); the optional private `agentic-internal` Gateway requires a ClusterIP HTTPS listener, operator-provisioned `agentic-internal-tls` certificate with the Service DNS SAN and a chain trusted by the immutable `thalamus-agentic-ca` ConfigMap's `ca.crt`. Fixture CA data is deliberately invalid. Public Gateway-to-agentic application traffic also requires independently verified encrypted cluster transport; the private listener does not establish it.

The installer renders a single-primary TLS/auth Redis StatefulSet and PVC, not an HA deployment. Provision its workload auth Secret (`url`, `auth`), Redis server TLS Secret (`tls.crt`, `tls.key`) and separate RLS CA Secret (`ca.crt`); the URL host must be the Redis Service DNS name and match the certificate SAN, and the password must be at least 32 base64url/hex characters. Supply durable RWO storage accessible to UID/GID 10001 and back up the PVC: losing it resets budget counters. Reconcile the shared `limits.k` RLS in `rendered/inference`, wait for Redis and RLS readiness and verify the loaded domain before traffic.

## Catalog publication and Flux

The site must provide a Git repository, reachable ref and credentials for the installer `catalog` checkout and for Flux, as well as a protected branch and required CI. The installer consumes these inputs; it does not provision the repository, credentials, branch protection or CI in a separate site repository. The site repository must contain the catalog, KCL entrypoints (`main.k`, `limits.k`, `pool.k`), their templates and module files, and `scripts/render-catalog.py` for manual and CI use. Thalamus and site CI need Python 3 and KCL 0.13.2. The supplied runtime image includes Python, pinned KCL and a separately installed, trusted copy of the renderer; keep that copy and the reviewed site repository script at the same version.

For every manual catalog PR, regenerate from the repository root:

```sh
python3 scripts/render-catalog.py --write
```

This reads `catalog/services`, `catalog/limits` and `catalog/pools` and produces the renderer-owned `rendered/inference` and `rendered/pools` trees, each with a `kustomization.yaml`. It prunes obsolete generated files and prints a deterministic combined Kubernetes `List` to stdout. Review and commit the changed source and all changes, including deletions, in both rendered trees together in the same proposal commit and PR. Do not hand-edit or put unrelated files in those generated trees. The default workload namespace is `thalamus-inference`; for another namespace, pass `--workload-namespace <namespace>` consistently to manual rendering and CI and set the same `gitops.workload_namespace` in Thalamus. The catalog service and limit namespaces and pool member namespaces must already match; rendering does not rewrite the source.

Thalamus makes structured changes to source catalog data and runs its trusted installed renderer with the proposal worktree as the working directory; it does not execute the renderer script from the PR checkout. It stages the renderer-owned output with the source in the same PR commit, rather than publishing from a separate branch. For both human and Thalamus PRs, protected CI must run the following read-only check on the merge candidate, using the site's matching namespace:

```sh
python3 scripts/render-catalog.py --check
```

`--check` fails on missing, modified or extra generated files without changing the checkout. This repository's GitHub CI supplies the check for this repository; a separate site repository must install or copy an equivalent check and require it in branch protection, including merge-queue candidates if used. Protect the check and renderer implementation from unreviewed changes too. `templates/ci/generate.py` supplies a rollout-event verification workflow, not catalog publication CI.

For branch-based publication, set installer `catalog.url` and `catalog.ref` to the site repository and protected merge branch (for example, `refs/heads/main`); `catalog.commit` must be reachable from that ref. Enable `gitops` for the fixed `rendered/inference` path and, for shared pools, `poolGitOps` for the fixed `rendered/pools` path. Either reconciler can be enabled separately. The installer emits one shared `GitRepository` in `thalamus-system` from that URL and ref, so source and generated resources come from the same merged commit rather than an independently generated branch. If authentication is needed, provision the read-only HTTPS Flux Secret named by `gitops.secretName` in `thalamus-system`, with `username` and `password` keys; checkout and Git push credentials are separate.

The inference Kustomization uses the ordinary namespace-scoped `catalog-reconciler` service account and references the shared source across namespaces. The separately administrator-controlled pool Kustomization uses `pool-reconciler` for privileged pool resources. When both are enabled, inference depends on the pool Kustomization being ready and having applied the shared source artifact's revision; the Flux controller must allow cross-namespace source and dependency references. Reconciliation is not atomic. Protect pool inputs, the pool output and any generator or template capable of changing them with administrator review; do not grant ordinary catalog writers control over the privileged pool reconciliation or its permissions. The inference path must exclude cluster-scoped resources and installer-owned Redis Service, StatefulSet, NetworkPolicy, Secrets and PVC; catalog Flux owns the separate RLS objects. The `admin-redis-ownership.thalamus.dev` policy reserves the configured Redis name against non-admin writers in enforced workload namespaces. Provide narrowly scoped reconciler privileges and protected read-only source credentials.

Flux follows the selected ref and its reconcilers can lag or skip an intermediate commit. Inspect their observed source and applied revisions; sharing a source alone does not prove that either path applied a particular commit. Existing exact-revision, readiness and authorization checks still apply.

**Catalog publication is explicitly pinned.** The installer `catalog.commit` pins the application checkout at startup, while Flux follows the configured ref independently. A merge or Flux apply does not advance API authorization, catalog/manual-deployment allowlists, or worker budget caps. After reviewing the new catalog commit, the operator must publish the updated `catalog.commit` and redeploy both server and worker. Durable rollout stages can authorize dynamically generated candidates without a startup candidate inventory; that does not automatically publish ordinary catalog ACL changes.

Capacity selection uses supplied eligible-node observations, not a Kubernetes scheduler simulation.

## Running the server and UI

Build the static UI and start the v2 server from the repository root, after setting the required server configuration below and provisioning PostgreSQL and an OIDC provider:

```sh
mise install
mise run ui:install
mise run ui:build
cargo run -p thalamus-server
```

The server applies its migrations, discovers the OIDC provider, and serves the UI from `THALAMUS_UI_DIR` along with `/auth`, `/api/v1`, `/health`, and `/openapi.json`. Access it through the configured HTTPS public origin and trusted proxy; register `<origin>/auth/callback` with the OIDC provider. Browser users authenticate through OIDC and use the server's session cookie. `cargo run -p thalamus-server -- --openapi` prints the API document without starting the server.

`mise run ui:dev` runs Vite separately for UI development; its configuration does not proxy API or login requests to the server. For integrated browser usage, serve the built UI from the server or configure an appropriate same-origin HTTPS proxy.

## Server configuration

The server reads the following environment variables at startup:

| Variable | Purpose |
| --- | --- |
| `DATABASE_URL` | PostgreSQL connection string shared by the server and worker. |
| `THALAMUS_PUBLIC_ORIGIN` | Canonical HTTPS browser origin, without a path or trailing slash. The OIDC callback is `<origin>/auth/callback`. |
| `THALAMUS_OIDC_ISSUER`, `THALAMUS_OIDC_CLIENT_ID`, `THALAMUS_OIDC_CLIENT_SECRET` | OIDC issuer and browser-login client credentials. |
| `THALAMUS_SESSION_SIGNING_KEY` | Session signing secret of at least 32 bytes. Keep it private and stable across replicas. |
| `THALAMUS_CATALOG_DIR` | Path to the catalog root containing `services/`, not the `services` directory itself. Supply a real catalog; the examples here are illustrative. |
| `THALAMUS_PROMETHEUS_URL` | Prometheus endpoint used for catalog metrics. |
| `THALAMUS_GROUP_MAPPING` | Optional JSON object with `platform_admin_group` (string), `team_admin_groups` and `member_groups` (objects mapping exact OIDC group names to existing team UUIDs). Omitted mapping defaults to empty. |
| `THALAMUS_UI_DIR` | Built UI directory; defaults to `ui/build`. |
| `THALAMUS_BIND` | HTTP listen address; defaults to `127.0.0.1:8080`. Terminate HTTPS at a trusted proxy. |
| `THALAMUS_GATEWAY_URL` | Optional trusted HTTPS inference API base ending in `/v1`, used by client configuration and gateway probes. It is not the public control-plane origin. |
| `THALAMUS_K8S_NAMESPACE`, `THALAMUS_GATEWAY_CONFIGMAP` | Optional namespace and projection name used to observe gateway key revocation. Configure consistently with the worker. |
| `THALAMUS_WORKFLOWS_CONFIG` | Optional operator-owned JSON configuration for GitOps and/or runs, described below. |

Client configuration requires trusted observed capability evidence, not just the gateway URL. Configure the runtime observer and obtain fresh evidence before expecting a supported client/model contract.

API bearer authentication is optional and independent of MCP. Set `THALAMUS_API_INTROSPECTION_ENDPOINT` to a trusted HTTPS introspection URL, together with nonempty `THALAMUS_API_CLIENT_ID`, `THALAMUS_API_CLIENT_SECRET`, and `THALAMUS_API_AUDIENCE` (an HTTPS resource URL for this API, distinct from MCP). Optionally set `THALAMUS_API_REQUIRED_SCOPES` to a whitespace-separated list. If the endpoint is absent, none of the other API introspection variables may be set and bearer authentication is disabled.

For each bearer request to `/api/v1`, the server introspects the token and requires it to be active, unexpired, issued by `THALAMUS_OIDC_ISSUER`, and valid for the exact API audience and every configured scope. The issuer/subject must already have been provisioned through browser OIDC login; current local roles still determine authorization. Send either `Authorization: Bearer <token>` or the browser session cookie, not both. Keep client secrets and tokens private.

`THALAMUS_RUNTIME_CONFIG` configures the shared runtime observer for server and worker. Its `routes` map binds a published service to `public`, `team_id`, `project_id` and `service`; see `deploy/install.fixture.yaml` for the version and namespace fields. It does not contain a static candidate inventory. Candidate and benchmark routes are derived from the authorized deployment, and route/backend identity, ready pinned workload, policies and Flux revision are independently observed. Catalog authorization or a durable rollout stage authorizes the deployment; drift between admission, Job preparation and ingestion rejects evidence. Public-route validation uses the public topology, not an arbitrary candidate topology.

The operator-controlled `runtimeConfig.trusted_agentic_images` array names exact digest-pinned images authorized to receive a forwarded bearer; its empty default disables agentic observation. Configure it consistently for server and worker, never from catalog-writable manifests. The fixture references an official amd64 nightly agentic image as an example, not as operator approval or runtime evidence. Public agentic validation is required before making public capability claims.

For agentic dependency observation, the installer `agenticSecretNames` is an administrator-owned list of exact workload DB/search Secret names, never catalog-derived. Names must be unique DNS subdomains, outside the reserved `thalamus-` prefix, and must not name the configured Redis auth/TLS or Gateway TLS Secrets. The observer receives resource-name-scoped `get` only, not list or watch; it has no access to key projection or TLS Secrets and retains metadata and immutability rather than Secret contents. Dependencies must be immutable; observation binds their UID and resource version and requires ready Pods created strictly after their Secrets.

For MCP, set `THALAMUS_MCP_INTROSPECTION_ENDPOINT` to the trusted HTTPS token introspection endpoint together with `THALAMUS_MCP_CLIENT_ID` and `THALAMUS_MCP_CLIENT_SECRET`. `THALAMUS_MCP_REQUIRED_SCOPES` is an optional whitespace-separated list. Without the endpoint MCP is not configured; the other MCP variables cannot be set alone. The expected token audience is `<THALAMUS_PUBLIC_ORIGIN>/mcp`; authenticated external subjects still require local authorization.

`THALAMUS_WORKFLOWS_CONFIG` is operator-owned JSON shared by server and worker, with at least one of `gitops` and `runs`. Before starting either role with GitOps enabled, provision an existing bare clone of the site repository at the absolute `repository_dir` and an existing `worktree_root` directory writable by the roles; both must be accessible to them. The bare clone is separate from the application's pinned catalog working checkout. GitOps also requires service-keyed `policies` specifying `target_branch`, `validation_tier` and rollout. Its optional `workload_namespace` defaults to `thalamus-inference` and must match the installer and site CI.

In the selected catalog worktree, GitOps invokes `python3 -I /usr/local/lib/thalamus/render-catalog.py --write --workload-namespace <configured namespace>` by default. An optional `gitops.render_script_path` selects another operator-managed, trusted renderer. The selected path must exist as an absolute, canonical regular file outside both the bare repository and the worktree root. Protect it and its parent directories from catalog writers, and keep it version-matched with the checked-in script used by manual rendering and CI. The runtime needs Python 3 and the actual pinned KCL 0.13.2 executable on `PATH`, and the worktree needs the catalog and KCL inputs described above. The server and worker invoke the renderer with a sanitized environment and isolated `HOME`; a mise shim that depends on the caller's `HOME` is not sufficient.

`service_schema_path`, when set, is a repository-relative path to the generated schema. Gated and in-place policies require matching complete controller policy and trusted run profiles. The optional `github` block uses `repository`, `api_base`, `app_id`, `installation_id`, `private_key_file`, `ci_bot_user_id`, `actor`, `commit_author_name`, `commit_author_email`, and optionally `webhook_secret`: it is GitHub App authentication, not a static API token. Git push is a separate transport and needs independently configured Git credentials/deploy key and host trust accessible to the process. Protect secrets.

The GitHub App needs repository Metadata read, Administration read (branch protection and rulesets), Contents read/write (REST merge), Pull requests read/write, Checks read, and Commit statuses read/write. Subscribe the signed webhook to `pull_request` and `merge_group`. REST/GraphQL App tokens renew automatically; the Git CLI instead needs separately configured persistent `gitAuth` askpass/PAT or another transport credential, not a static expiring App token.

For in-place automation, configure a signed webhook secret of at least 32 bytes, pin the App ID and status creator bot ID for `thalamus/rollout-window`, and pin all required CI App IDs in the controller policy. Require strict up-to-date checks, reviews, enforce-admin and no bypass, with explicit trusted admins. CI, approvals, base and head are rechecked as a snapshot at merge; direct merge is guarded by expected head. Scheduled in-place merges reject mandatory merge queues, although non-windowed queue flows are supported. GitHub success statuses do not expire, and a window is not a promise that deployment completes by its end. Singleton in-place raw serving uses `Recreate`.

`merge_requested_at` records local intent; `merge_observed_at` records the local response or follow observation; `upstream_merged_at` is the forge PR's actual merged timestamp when supplied. Flux readiness is an observation, not an exact rollout completion time.

The `runs` object supplies an absolute `profiles_file` and positive `max_evidence_age_seconds`. Profiles authorize service/team/project, runner and workload rather than enumerate future deployment revisions; worker `THALAMUS_VALIDATION_CONFIGS_JSON` is likewise keyed by service and runner, with `kind`, `route_target` and approved `options` (see the installer fixture). Endpoint and backend identity come from fresh observation, never fixed profile revision or caller URL.

Gated promotion requires candidate and public validation using the same agentic mode and relevant test options. A validation of the underlying model alone, or of a different profile, cannot promote another agentic contract.

## Worker and result storage

The worker needs the same `DATABASE_URL`, `THALAMUS_RUNTIME_CONFIG`, and a Git checkout in `THALAMUS_CATALOG_DIR`; it reads tracked `budget-caps.json` from the checkout's HEAD. Configure `THALAMUS_K8S_NAMESPACE`, `THALAMUS_GATEWAY_CONFIGMAP`, nonempty JSON object `THALAMUS_GATEWAY_SELECTOR_LABELS`, `THALAMUS_WORKER_ID`, `THALAMUS_LEASE_NAME`, and `THALAMUS_WORKER_HEALTH_ADDR`. API versions are specified in the runtime config. Set `THALAMUS_USAGE_PROMETHEUS_URL`, `THALAMUS_USAGE_INPUT_COUNTER`, `THALAMUS_USAGE_OUTPUT_COUNTER`, `THALAMUS_USAGE_STEP_SECONDS`, `THALAMUS_USAGE_START_DATE`, and `THALAMUS_USAGE_RETENTION_DAYS` for usage collection.

`THALAMUS_RUNNERS_JSON` is operator-controlled JSON keyed by runner name. Each entry provides `kind`, digest-pinned `image`, `command`, allowed `workloads`, `service_account`, `route_target`, and optional `key_secret` prefix. `THALAMUS_VALIDATION_CONFIGS_JSON` holds service-level runner profiles, not deployment-specific backend assertions. Configure the same runtime binding and workflow profiles on both roles; see `deploy/install.fixture.yaml` and `crates/thalamus-runtime/src/admission.rs` for the contract. Use scoped Kubernetes service accounts and permissions.

Configure S3-compatible result storage with `THALAMUS_S3_ENDPOINT` (HTTPS origin), `THALAMUS_S3_BUCKET`, `THALAMUS_S3_REGION`, `THALAMUS_S3_ACCESS_KEY_ID`, `THALAMUS_S3_SECRET_ACCESS_KEY`, and `THALAMUS_S3_RUN_SECRET_PREFIX`. Provision the bucket and credentials separately and enforce create-only conditional PUTs through bucket policy; do not rely on the runner to preserve immutability. The worker creates a short-lived presigned PUT for `runs/<run UUID>/result/output.json` in a per-run Kubernetes Secret; runner pods receive the URL rather than the S3 credentials. Protect both the credentials and these Secrets.

The server applies its bundled identity, run and change migrations at startup under a database advisory lock, recording checksums in `thalamus_schema_migrations`; changed applied migrations fail startup. Provision a database user with the necessary migration permissions and start the server before the worker. The libraries also expose their bundled SQL migrations for deployments that manage schema separately.

Identity synchronization is optional but needed for managed offboarding: configure either `THALAMUS_IDP_KEYCLOAK_CLIENT_ID` and `THALAMUS_IDP_KEYCLOAK_CLIENT_SECRET` for the Keycloak read-only collector, or `THALAMUS_IDP_SNAPSHOT_FILE` for trusted complete snapshots, never both. Set `THALAMUS_OIDC_ISSUER` and the same explicit `THALAMUS_GROUP_MAPPING` on server and worker (`{}` is valid). `THALAMUS_OIDC_CA_FILE` supplies private CA trust for the IdP; malformed trust must fail closed. Managed key projections expire no later than ten minutes after the snapshot, so stale or incomplete identity data cannot indefinitely authorize users.

Personal and project keys both require exact-team catalog service allowlists; project keys additionally bind to their project. Traffic classes and cost centres are platform-reviewed, not caller-selected privilege escalation. Gateway token budgets are post-response soft allowances; benchmark request budgets are separate. See [docs/redesign.md](docs/redesign.md) for the enforcement boundaries.

Debug sessions run a bounded real opencode lifecycle with a read-only Flux Operator MCP server, deadline and model/tool budgets. Provision the digest-pinned images, trusted endpoints and policy hashes in `debugConfig`/`debugServerConfig`, and verify the CNI/network and gateway/rate-limit trust assumptions in the fixture. The managed workload and session Secret remain in the same namespace; Secret RBAC is restricted to the exact session, not a broad multi-namespace key projection. Debug has no forge credentials.

## Developer setup

Install [mise](https://mise.jdx.dev/) and run from the repository root:

```sh
mise install
mise run ui:install
mise run build
mise run test
mise run ui:check
mise run ui:test
mise run ui:build
```

Rust 1.94.1, Node.js 24, pnpm 10, KCL 0.13.2 and Go 1.27.3 are declared in `mise.toml`. `mise run test` resolves the pinned KCL executable and Python's `sys.executable` in the repository, then prepends both executable directories to `PATH`. Integrated publication tests use isolated worktrees and `HOME`, so shims that depend on the caller's checkout or `HOME` are insufficient. The real Git publication test requires KCL rather than silently passing without it; see [TESTING.md](TESTING.md) for a direct invocation. `mise run ci` includes Rust, UI, generated schema, the read-only catalog output check, catalog publication unit suite, catalog/installer renders and serving, feature, gateway and platform KCL suites. GitHub CI additionally prepares and runs the pinned offline Kubernetes/CRD schema validator and the standalone gateway smoke; see [TESTING.md](TESTING.md) for exact commands and limitations.

Some integration tests may require PostgreSQL. Provide an isolated test database through `DATABASE_URL` and `TEST_DATABASE_URL`; CI uses PostgreSQL 16 and a dedicated `thalamus_test` database. Never point tests at a production database. Kubernetes integration requires explicitly configured credentials and a suitable cluster; normal local checks do not require accessing a cluster.

The architecture document retains live milestone acceptance criteria. Implemented code and offline checks are not evidence that those live criteria have passed.
