# Thalamus

[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue)](LICENSE-MIT)
[![Rust](https://img.shields.io/badge/rust-1.89%2B-orange)](https://www.rust-lang.org/)

A backend-centric LLM router and load balancer. Routes requests across multiple LLM backends with team-based access control, configurable routing strategies, and OpenTelemetry observability. Built with Rust, Axum, and PostgreSQL.

## Prerequisites

- [mise](https://mise.run) (manages all tools, env vars, and tasks)
- [Docker](https://docs.docker.com/get-docker/) (for dev services)

The development environment is fully managed by mise. All toolchains, services, and tasks are declared in `mise.toml` — no manual installation of Rust, PostgreSQL, or Redis is required.

## Getting Started

```bash
git clone https://github.com/yourusername/thalamus.git
cd thalamus

# Install all tools (rust, node, sqlx-cli, bacon, etc.)
mise install

# Check local tool readiness and see the quick-start commands
mise run setup

# Start PostgreSQL and Valkey
mise run services:up

# Run database migrations
mise run db:migrate

# Start the server (listens on port 3000)
mise run run
```

For development with automatic backend reload and the web UI dev server:

```bash
mise run dev
```

This starts the UI dev server in the background and uses [bacon](https://github.com/Canop/bacon) to watch backend changes and restart the server.

After starting a fresh instance, open the UI and create the first admin account. If there are no OAuth providers configured and no password users exist, the UI redirects to `/login/setup`. The setup endpoint creates the first admin user, default team, and OPAQUE password:

```bash
curl -X POST http://localhost:3000/v1/auth/setup \
  -H "Content-Type: application/json" \
  -d '{
    "username": "admin",
    "email": "admin@example.com",
    "password": "SuperSecret1!"
  }'
```

The response includes a PASETO token. Use it as `Authorization: Bearer <token>` to create API keys or manage the instance. After setup, sign in with the username and password you created.

## Commands

All commands are run via `mise run <task>`. Run `mise tasks` to see all available tasks.

| Command | Description |
|---|---|
| `mise run build` | Build in debug mode |
| `mise run build:release` | Build in release mode (LTO, stripped) |
| `mise run run` | Run the server |
| `mise run dev` | Run backend auto-reload and the UI dev server |
| `mise run dev:server` | Run backend auto-reload only |
| `mise run ui:dev` | Run the UI dev server only |
| `mise run test` | Run tests with cargo-nextest |
| `mise run test:verbose` | Run tests with immediate output |
| `mise run test:ci` | Run tests in CI mode (fail-fast) |
| `mise run test:cargo` | Run tests with standard `cargo test` |
| `mise run test:coverage` | Run tests with an HTML coverage report |
| `mise run test:watch` | Re-run tests on file changes |
| `mise run check` | Type-check without building |
| `mise run lint` | Run clippy with all warnings denied |
| `mise run fmt` | Format code with rustfmt |
| `mise run fmt:check` | Check formatting without modifying files |
| `mise run ci` | Run format check, lint, and tests sequentially |
| `mise run db:migrate` | Run SQLx migrations |
| `mise run db:create-migration <name>` | Create a new migration file |
| `mise run db:reset` | Drop and recreate the database schema |
| `mise run services:up` | Start PostgreSQL and Valkey in background |
| `mise run services:down` | Stop all services |
| `mise run clean` | Remove build artifacts |
| `mise run update` | Update Cargo dependencies |

## Services

Docker Compose manages two services:

- **PostgreSQL** -- `127.0.0.1:5432`, databases `thalamus` and `thalamus_test`
- **Valkey** -- `127.0.0.1:6379` (Redis-compatible)

Connection strings are set automatically on shell entry via `DATABASE_URL`, `TEST_DATABASE_URL`, and `REDIS_URL` environment variables.

## Authentication

Thalamus supports API keys, PASETO session tokens, username/password login through OPAQUE, and optional OAuth providers.

For the default local setup, leave `oauth_providers` unset in `config.k`. A fresh database then reports setup is required:

```bash
curl http://localhost:3000/v1/auth/setup-status
```

Create the first admin account with `POST /v1/auth/setup`, or use the setup screen in the UI. Normal username/password login then uses the OPAQUE flow exposed by `/v1/auth/login/start` and `/v1/auth/login/finish`; the UI handles those requests for you.

If you enable an OAuth provider, first-run setup is disabled and authentication is expected to start through that provider instead.

## Project Structure

```
src/
  features/           Feature modules (clean architecture)
    health/           Health check endpoint
    auth/             Authentication (API keys, PASETO, OPAQUE, OAuth)
    authorization/    Casbin RBAC
    teams/            Team management
    backends/         Backend registration and health monitoring
    routing/          LLM routing strategies
    llm_proxy/        OpenAI-compatible proxy API
  shared/
    config/           KCL configuration loading and hot-reload
    database/         SQLx connection pool
    observability/    Tracing and metrics
  middleware/         Global middleware
  bootstrap.rs       Dependency injection and app wiring
  error.rs           Central error types
  main.rs            Entry point
migrations/           SQL migrations
pkg/                  KCL configuration schemas
tests/                Integration tests
```

Each feature module follows the same internal layout: `api.rs` (HTTP handlers), `domain.rs` (traits and business logic), `infra.rs` (database and external service implementations), and `dto.rs` (request/response types).

## Configuration

Thalamus uses [KCL](https://kcl-lang.io/) for type-safe configuration. Schemas are defined in `pkg/` and validated at load time. The configuration system supports environment variable interpolation and file-watch-based hot-reload.

## Container Build

A production container image is built with Docker using a multi-stage build and a distroless base:

```bash
mise run container:build
```
