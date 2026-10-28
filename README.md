# Thalamus v2

Thalamus v2 is an inference operations **control plane** for self-hosted Kubernetes deployments. It is not a router or inference proxy: the inference request path remains with components such as agentgateway, llm-d and vLLM. The design, ownership boundaries and milestone plan are in [docs/redesign.md](docs/redesign.md).

The v2 source tree contains the control-plane libraries, `thalamus-server`, `thalamus-worker` and Svelte UI in `ui/`. The `v1-final` tag preserves the committed v1 tree. See the code and configuration below for the current integration boundaries.

## GitOps inputs

No actual GitOps repository, deployment catalog or deployment history is included or available here. Examples and test fixtures are illustrative only; they are not imported deployments. To use GitOps functionality against real infrastructure, supply your own actual repository and catalog as inputs to the relevant integration. Do not use example data as production configuration. Changes to deployment logic are intended to go through reviewed Git changes; Thalamus is not in the inference request path.

To render the illustrative service example, install KCL and run from the repository root:

```sh
kcl main.k -D service_file=catalog/services/example-service.yaml
```

Use the root `main.k` entrypoint so KCL resolves its file-reading sandbox from the repository root. The render checks the example configuration; it does not import a deployment or establish production readiness.

The capacity-selection logic is not a Kubernetes scheduler simulation: it depends on the caller supplying eligible nodes and cannot establish live Kubernetes scheduling, gateway behavior, performance, or deployment correctness. Illustrative examples and simulated results are not evidence of a real deployment or validation.

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

Client configuration also depends on a trusted capability-evidence provider; setting the gateway URL alone does not establish a supported client/model contract.

API bearer authentication is optional and independent of MCP. Set `THALAMUS_API_INTROSPECTION_ENDPOINT` to a trusted HTTPS introspection URL, together with nonempty `THALAMUS_API_CLIENT_ID`, `THALAMUS_API_CLIENT_SECRET`, and `THALAMUS_API_AUDIENCE` (an HTTPS resource URL for this API, distinct from MCP). Optionally set `THALAMUS_API_REQUIRED_SCOPES` to a whitespace-separated list. If the endpoint is absent, none of the other API introspection variables may be set and bearer authentication is disabled.

For each bearer request to `/api/v1`, the server introspects the token and requires it to be active, unexpired, issued by `THALAMUS_OIDC_ISSUER`, and valid for the exact API audience and every configured scope. The issuer/subject must already have been provisioned through browser OIDC login; current local roles still determine authorization. Send either `Authorization: Bearer <token>` or the browser session cookie, not both. Keep client secrets and tokens private.

The server executable currently starts without a capability-evidence provider, so client configuration that requires observed capability evidence is unavailable even if a gateway URL is set. Configuring a cluster, GitOps inputs, or a runner does not itself provide real-cluster or runtime proof: independent observations and run evidence must actually exist, and illustrative fixtures cannot establish deployment, scheduling, gateway behavior, performance, or production readiness.

For MCP, set `THALAMUS_MCP_INTROSPECTION_ENDPOINT` to the trusted HTTPS token introspection endpoint together with `THALAMUS_MCP_CLIENT_ID` and `THALAMUS_MCP_CLIENT_SECRET`. `THALAMUS_MCP_REQUIRED_SCOPES` is an optional whitespace-separated list. Without the endpoint MCP is not configured; the other MCP variables cannot be set alone. The expected token audience is `<THALAMUS_PUBLIC_ORIGIN>/mcp`; authenticated external subjects still require local authorization.

`THALAMUS_WORKFLOWS_CONFIG` is a JSON object with at least one of `gitops` and `runs`. The `gitops` object contains absolute `repository_dir` and `worktree_root` paths, `render_executable`, `render_args` (exactly one occurrence of `{service_file}` across the arguments), and a nonempty `policies` object keyed by service name. Each policy has `target_branch`, `validation_tier`, and a `rollout` object with `mode` of `gated`, `rolling`, or `in_place` (the latter also needs `window_start` and `window_end` timestamps). Optional `github` publication configuration contains `repository`, `api_base`, `token`, `actor`, `commit_author_name`, and `commit_author_email`. Keep credentials private. The `runs` object contains an absolute `profiles_file` and a positive `max_evidence_age_seconds`; profiles are operator/collector-owned, bind runs to a team, and require independently observed evidence. These settings do not supply real infrastructure or evidence by themselves.

## Worker and result storage

The worker needs the same `DATABASE_URL` and a real Git checkout in `THALAMUS_CATALOG_DIR`; it reads tracked `budget-caps.json` from the checkout's HEAD. Configure `THALAMUS_K8S_NAMESPACE`, `THALAMUS_GATEWAY_CONFIGMAP`, nonempty JSON object `THALAMUS_GATEWAY_SELECTOR_LABELS`, `THALAMUS_WORKER_ID`, `THALAMUS_LEASE_NAME`, and `THALAMUS_WORKER_HEALTH_ADDR`. Set `THALAMUS_FLUX_VERSION`, `THALAMUS_HELM_VERSION`, `THALAMUS_SERVING_VERSION`, and `THALAMUS_INFERENCE_POOL_VERSION` to the API versions used by the cluster.

`THALAMUS_RUNNERS_JSON` is an operator-controlled JSON object keyed by runner name. Each entry provides `kind`, digest-pinned `image`, `command` array, allowed `workloads` array, `service_account`, and optional `key_secret` prefix. `THALAMUS_VALIDATION_CONFIGS_JSON` maps service names to deployment names to validation configuration objects; see `crates/thalamus-runs/src/jobs.rs` for supported fields. Use appropriately scoped Kubernetes service accounts and permissions.

Configure S3-compatible result storage with `THALAMUS_S3_ENDPOINT` (HTTPS origin), `THALAMUS_S3_BUCKET`, `THALAMUS_S3_REGION`, `THALAMUS_S3_ACCESS_KEY_ID`, `THALAMUS_S3_SECRET_ACCESS_KEY`, and `THALAMUS_S3_RUN_SECRET_PREFIX`. Provision the bucket and credentials separately and enforce create-only conditional PUTs through bucket policy; do not rely on the runner to preserve immutability. The worker creates a short-lived presigned PUT for `runs/<run UUID>/result/output.json` in a per-run Kubernetes Secret; runner pods receive the URL rather than the S3 credentials. Protect both the credentials and these Secrets.

The server applies its bundled identity, run and change migrations at startup under a database advisory lock, recording checksums in `thalamus_schema_migrations`; changed applied migrations fail startup. Provision a database user with the necessary migration permissions and start the server before the worker. The libraries also expose their bundled SQL migrations for deployments that manage schema separately.

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

Rust 1.94.1, Node.js 24, pnpm 10 and KCL 0.13.2 are declared in `mise.toml`. The UI install uses the committed pnpm lockfile. For a UI development server, run `mise run ui:dev`. Formatting can be applied with `mise run fmt` or checked without changes with `mise run fmt:check`. `mise run kcl:render` renders the illustrative KCL example, and `mise run ci` runs that render alongside the Rust format, build and test checks and the UI check, build and test checks; install UI dependencies first.

Some integration tests may require PostgreSQL. Provide an isolated test database through `DATABASE_URL` and `TEST_DATABASE_URL`; CI uses PostgreSQL 16 and a dedicated `thalamus_test` database. Never point tests at a production database. Kubernetes integration requires explicitly configured credentials and a suitable cluster; normal local checks do not require accessing a cluster.

See the individual crates for their interfaces and required inputs. The architecture document describes intended later capabilities as well as the current direction; it is not a claim that all milestones have been implemented.
