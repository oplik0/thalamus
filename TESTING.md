# Testing Thalamus v2

Run the commands below from the repository root. These are instructions, not a record of checks having passed. The v2 workspace is the collection of crates in `Cargo.toml`; the old v1 backend, development services and authentication flow are not the v2 test setup.

## Rust workspace

Install the tool versions declared in `mise.toml`, then run the workspace checks:

```sh
mise install
mise run fmt:check
mise run build
mise run test
```

Database-backed SQLx tests require `DATABASE_URL` for a disposable PostgreSQL instance whose user can create test databases. The default test run excludes Rust tests marked `#[ignore]`.

On memory-constrained nodes, run the integrated checks with serial compilation and without debug symbols:

```sh
CARGO_BUILD_JOBS=1 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 mise run ci
```

These settings do not disable tests, Cargo features or debug assertions. The integrated task also checks the UI, catalog publication and KCL recipes.

The real Git catalog publication regression in `thalamus-gitops` requires Python 3 and the actual pinned KCL 0.13.2 executable; it must not silently pass without KCL. Integrated Rust publication tests and the server/worker renderer run with a sanitized environment and isolated `HOME`, so executable shims that depend on the caller's `HOME` or checkout are insufficient. `mise run test` and `mise run ci` resolve the actual executables in the repository, place only `kcl` and `python3` symlinks in a private temporary directory at the front of `PATH`, and remove that directory on exit. This preserves the ordering of other mise-selected tools. `mise run test` includes the real publication regression.

GitHub CI installs Python and pinned KCL and places a symlink to the actual KCL binary in a runner temporary directory on `PATH` before running Rust tests.

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

## KCL and offline deployment schemas

Render from the repository root with Python 3 and KCL 0.13.2. The installer resolves `config_file` relative to `deploy/`; an absolute operator file is also accepted.

```sh
python3 scripts/generate-service-schema.py --check
python3 scripts/render-catalog.py --check
python3 -B -m unittest discover -s scripts -p 'test_render_catalog.py'
kcl main.k -D service_file=catalog/services/example-service.yaml -o /tmp/thalamus-service.yaml
kcl main.k -D service_file=catalog/services/example-pool-service.yaml -o /tmp/thalamus-pool-service.yaml
kcl deploy/install.k -D config_file=install.fixture.yaml -o /tmp/thalamus-install.yaml
kcl limits.k -D limit_file=catalog/limits/example-shared.yaml -o /tmp/thalamus-limits.yaml
kcl pool.k -D pool_file=catalog/pools/example-pool.yaml -o /tmp/thalamus-pool.yaml
kcl test ./templates/serving
kcl test ./templates/features
kcl test ./templates/gateway
kcl test ./templates/platform
python3 deploy/prepare-schema-bundle.py /tmp/thalamus-schemas
/tmp/thalamus-schemas/validate --bundle /tmp/thalamus-schemas/schemas.json /tmp/thalamus-service.yaml /tmp/thalamus-pool-service.yaml /tmp/thalamus-install.yaml /tmp/thalamus-limits.yaml /tmp/thalamus-pool.yaml
```

`render-catalog.py --check` is read-only: it compares both complete generated trees, including their `kustomization.yaml` files, and fails on missing, modified or extra files. For a manual PR, use `python3 scripts/render-catalog.py --write` instead, then review and commit the catalog source and all changes in `rendered/inference` and `rendered/pools` together in the same proposal commit; it removes stale generated files and prints a deterministic combined Kubernetes `List` to stdout. For a non-default workload namespace, pass the same `--workload-namespace <namespace>` to rendering and checking, and configure Thalamus to use it too. Source service and limit namespaces and pool member namespaces must already match. See [README.md](README.md#catalog-publication-and-flux) for the site repository, credentials and branch-protection requirements.

These commands use the checked-in renderer. Thalamus server and worker instead invoke an operator-trusted installed copy with `python3 -I` from the catalog worktree, not the PR's Python script. For an integrated workflow, pre-create the bare repository and writable worktree root and provide a trusted regular renderer outside both. Keep the runtime copy version-matched to the repository and CI and protect its location from catalog writers; see [README.md](README.md#server-configuration). A local `--check` does not establish the installed runtime script's identity or permissions.

This repository's GitHub CI runs the read-only catalog check; operators must install and require an equivalent check in a separate site catalog repository, including on merge-queue candidates if used. It is a consistency check, not a publisher that commits files or proves deployment.

The catalog publication unit suite uses temporary worktrees; the real-catalog `--check` is a separate check. The pinned validator uses Go 1.27.3 and Kubernetes 1.34.2 and CRD schemas. Preparing the bundle may fetch pinned upstream inputs; validation itself uses the prepared offline bundle. Schema validation is not live admission, reconciliation, policy conversion or Kubernetes execution. `mise run ci` covers the read-only catalog check, publication unit suite, renders and KCL suites; GitHub CI also runs the offline validator and standalone gateway smoke.

`python3 scripts/gateway-policy-smoke.py` requires KCL, Python 3, Docker and Linux host networking. It exercises real standalone agentgateway, Redis and Envoy RLS auth, scope, headers, request-budget exhaustion and fail-closed behavior. It does not establish Kubernetes conversion, AI token accounting, metrics or EPP behavior.

## OIDC and offboarding smoke

```sh
bash scripts/oidc-smoke/run.sh
# Optional interactive fixture, retained until explicit cleanup:
bash scripts/oidc-smoke/run.sh --browser
bash scripts/oidc-smoke/run.sh --cleanup /path/printed/by/browser/run
```

Requires Docker Compose, Git, OpenSSL, Python 3 and `timeout`. The default creates an isolated network with no published ports and cleans up automatically. It uses a disposable Keycloak, PostgreSQL and server, a private CA and real login and collector/offboarding flow; it checks that missing or malformed CA trust fails. `--browser` exposes HTTPS on loopback and retains its private fixture directory and services; use its printed instructions and clean it up when finished. This is not a Kubernetes identity or gateway acceptance test.

The fixture builds a debug server with debug symbols disabled. It exercises functional integration, not a release container build or release performance.

## Real simulator benchmark

```sh
docker build -t thalamus-inference-perf -f scripts/inference-perf/Dockerfile .
python3 scripts/inference-perf/simulator.py --adapter-image thalamus-inference-perf --scenario multi-turn --load concurrent --output /tmp/thalamus-benchmark.json
python3 -m unittest discover -s scripts/inference-perf -p 'test_*.py'
```

The output path must not already exist. This runs the real inference-perf adapter and llm-d-inference-sim Docker image; a local adapter image ID is identified as local, not as a pushed registry digest. Measurements describe the simulator, not GPU performance, a Kubernetes Job, gateway, live provenance or production readiness.

No real cluster or previous deployment history is needed for these local checks. Live acceptance still requires explicitly approved infrastructure and independently collected evidence; fixtures are not evidence of deployed behavior.

## Bounded mutation testing

Install [cargo-mutants](https://mutants.rs/) separately (`cargo install --locked cargo-mutants`). The selected domain and authorization scope below requires `DATABASE_URL` pointing to disposable PostgreSQL with test-database creation permission. It exercises per-node capacity arithmetic and the production policy decisions for team, deployment and operation access:

```sh
CARGO_BUILD_JOBS=1 RUST_TEST_THREADS=2 TMPDIR=/var/tmp \
  cargo mutants --no-config \
  --package thalamus-core --package thalamus-access \
  --file crates/thalamus-core/src/capacity.rs \
  --file crates/thalamus-access/src/lib.rs \
  --re 'capacity.rs|Policy::(team_allowed|includes_deployment|permissions|authorize|authorized_teams)' \
  --test-workspace=false --timeout 60 --build-timeout 600 \
  --jobs 1 --jobserver-tasks 2 --output /var/tmp/thalamus-mutants
```

`--timeout 60` bounds each mutant test and `--build-timeout 600` bounds its build; these are not a whole-run deadline. An interrupted run is incomplete, not a passing mutation result. Inspect the output directory for missed, timed-out and unviable mutants rather than treating the command's invocation as proof. This scope does not establish correctness of other policy branches or production infrastructure.

The core property tests exercise production domain functions, including per-node capacity and published-capability intersection: `cargo test -p thalamus-core --test domain`. They explore generated cases, not all executions or live scheduling.

## Creusot

[Creusot](https://guide.creusot.rs/) is optional and not part of the normal workspace checks. The isolated `proofs/capacity` package compiles the same per-node arithmetic module used by `thalamus-core`. Its contracts cover rejecting a zero unit size or headroom above physical capacity (including overflow), and require the returned whole-unit count to fit without excess GPUs and to be maximal. They do not prove rollout-mode selection, aggregation across nodes, or the correctness of the input observations.

From the repository root, run the pinned Creusot 0.14.0 package with Nix. The overrides use the release's pinned nixpkgs input for Z3, CVC4 and CVC5 instead of rebuilding those solvers from source; Creusot, Why3, Why3find and free Alt-Ergo come from the release:

```sh
nix --extra-experimental-features 'nix-command flakes' \
  --option cores 2 --option max-jobs 2 shell --impure \
  --expr 'let
    f = builtins.getFlake "github:creusot-rs/creusot/v0.14.0";
    overrides = final: prev: {
      creusot = prev.creusot // {
        z3 = prev.z3; cvc4 = prev.cvc4; cvc5 = prev.cvc5;
      };
    };
    pkgs = import f.inputs.nixpkgs {
      system = builtins.currentSystem;
      overlays = [ f.overlays.default overrides ];
    };
  in pkgs.creusot.mkCreusotWrapped { isFree = true; }' \
  --command sh -c 'cd proofs/capacity && cargo creusot -j 1 --no-cache'
```

Alternatively install [Creusot 0.14.0](https://github.com/creusot-rs/creusot/tree/v0.14.0) locally with its `./INSTALL` script; that route requires rustup, opam and curl and installs the matching `nightly-2026-08-03` compiler and the Why3/Why3find and prover dependencies. Then run `(cd proofs/capacity && cargo creusot -j 1 --no-cache)` from the repository root. The proof package pins `creusot-std` to 0.14.0. A successful proof prints `Proved` for `node_serving_units.coma`; a build, translation alone, or an interrupted or incomplete prover run does not establish the contracts. The standalone proof crate can warn that the helper is unused; missing Rust-library source-location warnings may also appear.
