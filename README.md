# Thalamus v2

Thalamus v2 is an inference operations **control plane** for self-hosted Kubernetes deployments. It is not a router or inference proxy: the inference request path remains with components such as agentgateway, llm-d and vLLM. The design, ownership boundaries and milestone plan are in [docs/redesign.md](docs/redesign.md).

The v2 workspace currently contains the `thalamus-core`, `forge`, `thalamus-gitops` and `thalamus-k8s` crates, plus the Svelte UI in `ui-v2`. The v1 source and UI remain in the tree but are not members of this Rust workspace. A complete v2 server and deployment workflow are not implied by these library crates.

## GitOps inputs

No actual GitOps repository, deployment catalog or deployment history is included or available here. Examples and test fixtures are illustrative only; they are not imported deployments. To use GitOps functionality against real infrastructure, supply your own actual repository and catalog as inputs to the relevant integration. Do not use example data as production configuration. Changes to deployment logic are intended to go through reviewed Git changes; Thalamus is not in the inference request path.

To render the illustrative service example, install KCL and run from the repository root:

```sh
kcl main.k -D service_file=catalog/services/example-service.yaml
```

Use the root `main.k` entrypoint so KCL resolves its file-reading sandbox from the repository root. The render checks the example configuration; it does not import a deployment or establish production readiness.

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
