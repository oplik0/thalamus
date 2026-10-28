# Testing Thalamus v2

Run the commands below from the repository root. These are instructions, not a record of checks having passed. The v2 workspace is the collection of crates in `Cargo.toml`; the old v1 backend, development services and authentication flow are not the v2 test setup.

## Rust workspace

Install the tool versions declared in `mise.toml`, then run the workspace checks:

```sh
mise install
cargo fmt --all -- --check
cargo build --workspace --all-targets
cargo test --workspace
```

Equivalently, `mise run fmt:check`, `mise run build` and `mise run test` use those commands. Database-backed SQLx tests require `DATABASE_URL` for a disposable PostgreSQL instance whose user can create test databases. The default test run excludes Rust tests marked `#[ignore]`.

## PostgreSQL integration tests

Use a separately provisioned, disposable PostgreSQL database, never a production database. CI uses PostgreSQL 16. The `thalamus-changes` store tests are ignored by default and require `TEST_DATABASE_URL`; their database user must be able to create and drop schemas. They also use Git and `cat`. For example, after provisioning the database and supplying its connection string:

```sh
export TEST_DATABASE_URL='postgresql://USER:PASSWORD@HOST:5432/DISPOSABLE_DATABASE'
cargo test -p thalamus-changes --test store -- --ignored
```

These tests create isolated schemas and normally drop them, but an interrupted or failed run can leave schemas or temporary files behind. The server and worker instead use `DATABASE_URL`; server startup applies its bundled migrations. See the server and worker configuration in [README.md](README.md) before attempting an integrated deployment. The runtime compose file is not a ready-made isolated test database setup.

## UI

The v2 UI is in `ui/`. Install its locked dependencies, then check, test and build it:

```sh
mise run ui:install
mise run ui:check
mise run ui:test
mise run ui:build
```

`mise run ui:dev` starts Vite separately; it does not proxy API or authentication requests. For integrated browser testing, build the UI and run the server behind a same-origin HTTPS proxy as described in [README.md](README.md). UI checks alone do not demonstrate an integrated server, OIDC provider, or Kubernetes deployment.

For API bearer authentication, configure the optional `THALAMUS_API_INTROSPECTION_ENDPOINT`, `THALAMUS_API_CLIENT_ID`, `THALAMUS_API_CLIENT_SECRET`, `THALAMUS_API_AUDIENCE`, and optionally `THALAMUS_API_REQUIRED_SCOPES` as documented in the README. A real integration exercise needs a trusted introspection service, appropriately issued tokens and a locally provisioned user; unit tests or UI checks cannot substitute for those dependencies.

## KCL example

Render the illustrative catalog service from the root so KCL resolves file paths in the intended sandbox:

```sh
mise run kcl:render
# Equivalent:
kcl main.k -D service_file=catalog/services/example-service.yaml
kcl test ./templates/serving
kcl test ./templates/features
```

This is a render of example inputs, not evidence of a real deployment or live Kubernetes behavior. `mise run ci` combines Rust formatting, build and tests with this render and the UI check, build and test; install UI dependencies first.

For an opt-in standalone gateway security smoke, run `python3 scripts/gateway-policy-smoke.py`. It requires KCL, Python 3, Docker and Linux host networking. It renders the example policy and exercises its authorization and attribution expressions against agentgateway v1.7.0, including forged headers and malformed credentials. It does not test Kubernetes policy conversion, ConfigMap selection or a deployment.

No real cluster, production GitOps repository, or deployment history is supplied here. Cluster behavior and runtime proof require actual infrastructure and independently collected evidence; neither example rendering nor simulated results establish them. The server binary also starts without a capability-evidence provider, so evidence-dependent client configuration is unavailable in that setup.

## Bounded mutation testing

Install [cargo-mutants](https://mutants.rs/) separately (`cargo install --locked cargo-mutants`). To limit the scope, worker count and elapsed time, for example:

```sh
timeout 20m cargo mutants -p thalamus-core -f crates/thalamus-core/src/lib.rs -j 1 --timeout 60
```

The outer `timeout` bounds the whole run; `--timeout 60` limits each mutant test. An interrupted run is incomplete, not a passing mutation result. Review the generated `mutants.out` results, including missed and timed-out mutants; mutation testing does not establish production correctness.

## Creusot

[Creusot](https://guide.creusot.rs/) is an optional deductive verification tool, not part of the declared mise toolchain or the normal workspace checks. Follow its installation guide for its required Rust toolchain, Why3 and provers; `cargo creusot --help` describes the installed command. The current workspace has no Creusot proof configuration or annotated proof targets. Add and review suitable specifications and verifier support before attempting a proof; ordinary Rust tests and a successful build are not formal verification.
