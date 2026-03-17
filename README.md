# Thalamus

[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue)](LICENSE-MIT)
[![Rust](https://img.shields.io/badge/rust-1.89%2B-orange)](https://www.rust-lang.org/)

A backend-centric LLM router and load balancer. Routes requests across multiple LLM backends with team-based access control, configurable routing strategies, and OpenTelemetry observability. Built with Rust, Axum, and PostgreSQL.

## Prerequisites

- [Nix](https://nixos.org/download.html) with flakes enabled
- [devenv](https://devenv.sh/getting-started/)

The development environment is fully managed by devenv. All toolchains, services, and shell scripts are declared in `devenv.nix` - no manual installation of Rust, PostgreSQL, or Redis is required.

## Getting Started

```bash
git clone https://github.com/yourusername/thalamus.git
cd thalamus

# Enter the development shell (installs all dependencies automatically)
devenv shell

# Start PostgreSQL and Redis
services-up

# Run database migrations
db-migrate

# Start the server (listens on port 3000)
run
```

For development with automatic rebuild on file changes:

```bash
dev
```

This uses [bacon](https://github.com/Canop/bacon) to watch for changes and restart the server.

## Commands

All commands are available inside `devenv shell`.

| Command | Description |
|---|---|
| `build` | Build in debug mode |
| `build-release` | Build in release mode (LTO, stripped) |
| `run` | Run the server |
| `dev` | Run with auto-reload via bacon |
| `test` | Run tests with cargo-nextest |
| `test-verbose` | Run tests with immediate output |
| `test-ci` | Run tests in CI mode (fail-fast) |
| `test-cargo` | Run tests with standard `cargo test` |
| `check` | Type-check without building |
| `lint` | Run clippy with all warnings denied |
| `fmt` | Format code with rustfmt |
| `fmt-check` | Check formatting without modifying files |
| `ci` | Run format check, lint, and tests sequentially |
| `db-migrate` | Run SQLx migrations |
| `db-create-migration <name>` | Create a new migration file |
| `db-reset` | Drop and recreate the database schema |
| `services-up` | Start PostgreSQL and Redis in background |
| `services-down` | Stop all services |
| `clean` | Remove build artifacts |
| `update` | Update Cargo dependencies |

## Services

devenv manages two services:

- **PostgreSQL** -- `127.0.0.1:5432`, databases `thalamus` and `thalamus_test`
- **Redis** -- `127.0.0.1:6379`

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

A production container image is defined in `devenv.nix` using [Crane](https://crane.dev/). It produces a minimal image with only the compiled binary, CA certificates, and migration files:

```bash
devenv container build prod
```
