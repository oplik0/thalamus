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

# Start PostgreSQL and Valkey
mise run services:up

# Run database migrations
mise run db:migrate

# Start the server (listens on port 3000)
mise run run
```

For development with automatic rebuild on file changes:

```bash
mise run dev
```

This uses [bacon](https://github.com/Canop/bacon) to watch for changes and restart the server.

## Commands

All commands are run via `mise run <task>`. Run `mise tasks` to see all available tasks.

| Command | Description |
|---|---|
| `mise run build` | Build in debug mode |
| `mise run build:release` | Build in release mode (LTO, stripped) |
| `mise run run` | Run the server |
| `mise run dev` | Run with auto-reload via bacon |
| `mise run test` | Run tests with cargo-nextest |
| `mise run test:verbose` | Run tests with immediate output |
| `mise run test:ci` | Run tests in CI mode (fail-fast) |
| `mise run test:cargo` | Run tests with standard `cargo test` |
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
