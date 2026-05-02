# AGENTS.md

## Environment

- **Everything runs inside `devenv shell`**. All build/test/service commands are defined there. Do not invoke `cargo` directly outside the shell.
- **Prerequisites**: Nix with flakes, devenv. No manual Rust/PostgreSQL/Redis installation.

## Service Startup (Required Before Run/Test)

```
services-up    # Starts PostgreSQL (127.0.0.1:5432) and Redis (127.0.0.1:6379)
db-migrate     # Applies SQLx migrations
```

Order matters. `db-migrate` will fail if `services-up` hasn't run.

## Developer Commands

| Command | What it does |
|---------|-------------|
| `run` | Start server on port 3000 |
| `dev` | Auto-reload server via bacon |
| `test` | Run tests with cargo-nextest |
| `test-one <pattern>` | Run a single test / subset |
| `test-debug <name>` | Run single test with `RUST_BACKTRACE=full` and `RUST_LOG=thalamus=debug` |
| `test-ci` | CI test profile (fail-fast, no retries) |
| `test-watch` | Continuous test runner via bacon |
| `check` | Type-check without building |
| `lint` | Clippy with `-D warnings` |
| `fmt` | Format with rustfmt |
| `ci` | Run `fmt-check` → `lint` → `test-ci` sequentially |
| `db-reset` | Drop public schema and re-run migrations |
| `db-create-migration <name>` | Add new SQLx migration file |
| `ui-dev` | Start Expo frontend in web mode (`cd ui && pnpm start --web`) |
| `ui-lint` / `ui-format` | Biome checks in `ui/` |

## Testing

- **Preferred runner**: `cargo nextest` (via `test`, `test-ci`, `test-one`). `cargo test` is available but slower.
- **Test isolation**: Use `#[sqlx::test]` for automatic transaction rollback. Prefer this over legacy `TestFixtures` (deprecated).
- **Mock backends**: Use `MockLlmBackend` + `init_test_state_with_backends(pool, &[&backend])` for E2E proxy tests.
- **Builder fixtures**: Use `TestUserBuilder`, `TestApiKeyBuilder`, `LlmRequestBuilder` for test data.
- **Nextest profiles** (`.config/nextest.toml`):
  - `default`: retries=2, fail-fast=false
  - `ci`: retries=0, fail-fast=true
  - `debug`: single-threaded, 300s timeout, no retries

## SQLx Offline Queries

- `.sqlx/` contains committed offline query metadata used by container builds (`SQLX_OFFLINE=true`).
- **If you add or modify `sqlx::query!` macros**, regenerate metadata with `cargo sqlx prepare -- --tests` (requires `services-up` + `db-migrate`). Commit the resulting `.sqlx/` changes.

## Architecture

- **Binary + lib**: `src/main.rs` is the entry point; the rest is a library crate.
- **Feature modules** under `src/features/`: `health`, `auth`, `authorization`, `teams`, `backends`, `routing`, `llm_proxy`.
- **Internal layout per feature**: `api.rs` (handlers), `domain.rs` (traits/logic), `infra.rs` (db/external), `dto.rs` (request/response types).
- **Shared**: `src/shared/config` (KCL), `src/shared/database` (SQLx pool), `src/shared/observability` (tracing).
- **Bootstrap**: `src/bootstrap.rs` wires DI, `AppState`, and the Axum router.
- **Config**: KCL-based (`config.k` / `config.k.example`). Schemas in `pkg/`. Supports hot-reload via file watcher.
- **Migrations**: `migrations/` using SQLx migrate. `casbin_model.conf` is the Casbin RBAC model.

## Frontend (`ui/`)

- Expo / React Native with file-based routing (`expo-router`).
- **Package manager**: pnpm. Do not use npm.
- **Lint/Format**: Biome (not ESLint/Prettier). Config in `ui/biome.json`.
- **Styling**: NativeWind + Tailwind CSS. Custom color tokens in `ui/tailwind.config.js`.
- **Metro**: Configured for NativeWind (`metro.config.js`).

## CI / Release

- **CI order**: `fmt-check` → `lint` → `test-ci`.
- **Coverage**: `test-coverage-xml` generates Cobertura XML. Runs on `main` and PRs to `main`.
- **Container**: `devenv container build prod` (Crane + nix2container). Dockerfile exists but is secondary.
- **Pre-commit hooks**: Managed by devenv (`devenv.nix`). Includes rustfmt, clippy, nixfmt, editorconfig, EOF/trailing-space fixes. Biome hook is currently disabled.

## Rust Notes

- **Edition**: 2024. **MSRV**: 1.89.0.
- **Default feature**: `caching` (pulls in `redis`). Disable with `--no-default-features` if Redis is unavailable.
- **Release profile**: LTO, single codegen unit, stripped.
- **Dependencies**: Axum 0.8, SQLx 0.8, KCL from custom forks (`oplik0/kcl` branches).

## Environment Variables (Set by `devenv shell`)

- `DATABASE_URL` → `postgresql://$USER@localhost:5432/thalamus`
- `TEST_DATABASE_URL` → `postgresql://$USER@localhost:5432/thalamus_test`
- `REDIS_URL` → `redis://localhost:6379`
