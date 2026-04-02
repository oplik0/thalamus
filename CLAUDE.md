# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## About Thalamus

Thalamus is a backend-centric LLM router and load balancer built with Rust and Axum. It provides intelligent routing, team-based access control, and comprehensive observability for LLM deployments.

## Common Commands

### Build & Run

```bash
# Build the project
build

# Build with optimizations
build-release

# Run the server
run

# Run with auto-reload on file changes (using bacon)
dev
```

### Testing

```bash
# Run all tests with nextest
test

# Run tests with immediate output
test-verbose

# Run tests in CI mode (fail-fast, no retries)
test-ci

# Run a single test
cargo nextest run test_name

# Run standard cargo tests
test-cargo
```

### Code Quality

```bash
# Check code without building
check

# Run clippy linter
lint

# Format code
fmt

# Check formatting without modifying
fmt-check

# Run all CI checks (format + lint + test)
ci
```

### Database

```bash
# Run database migrations
db-migrate

# Create a new migration
db-create-migration <name>

# Reset the test database
db-reset
```

### Services

```bash
# Start PostgreSQL and Redis services
services-up

# Stop services
services-down
```

## Architecture

Thalamus follows **clean architecture** principles with a modular, feature-based structure.

### Feature Module Pattern

Each feature is organized in `src/features/<feature_name>/` with:
- `api.rs` - HTTP handlers and route definitions
- `domain.rs` - Business logic and trait definitions (interfaces)
- `infra.rs` - Infrastructure implementations (database, external APIs)
- `dto.rs` - Data transfer objects for request/response

Features communicate through well-defined domain traits (dependency inversion).

### Current Features

- **health** - Health check endpoint (fully implemented)
- **auth** - Authentication (API keys, PASETO tokens) - in progress
- **authorization** - Casbin-based RBAC with team support - planned
- **teams** - Team management and memberships - planned
- **backends** - Backend registration and health monitoring - planned
- **routing** - Intelligent LLM routing strategies - planned
- **llm_proxy** - OpenAI-compatible proxy endpoints - planned

### Shared Infrastructure

Located in `src/shared/`:
- **config** - KCL configuration loading and hot-reload watching
- **database** - SQLx connection pool
- **observability** - Tracing and metrics setup

### Bootstrap & Dependency Injection

`src/bootstrap.rs` wires together all application components:
- `init_app_state()` - Initializes database, config, and services
- `build_router()` - Constructs the Axum router with all routes

### Error Handling

Central error type in `src/error.rs`:
- Custom `Result<T>` type for all fallible operations
- Automatic HTTP status code mapping
- Structured error responses with tracing

## Configuration System

Thalamus uses **KCL** (KCL Configuration Language) for type-safe, validated configuration with hot-reload support.

### Configuration Schema

KCL schemas are defined in `pkg/schemas.k` with built-in validation:
- Server, database, and service configuration
- Backend and endpoint definitions with capacity and model awareness
- Routing strategies (model-aware least-busy, round-robin, weighted, etc.)
- Authentication, health checks, and retry policies
- Observability (tracing, metrics), caching, and rate limiting

### Loading Configuration

Configuration is loaded via `src/shared/config/loader.rs`:
- Validates schema constraints at load time
- Supports environment variable interpolation
- Hot-reload capability via file watcher

## Database Schema

PostgreSQL schema (see `migrations/20250122000001_initial_schema.up.sql`):

### Core Tables
- **teams** - Team definitions with budgets, rate limits, and access policies
- **users** - Users and service accounts
- **team_memberships** - Many-to-many user-team relationships with roles
- **api_keys** - API key management with scopes and expiration
- **casbin_rule** - Casbin RBAC policies

### Observability Tables
- **usage_logs** - Token usage, costs, latency, and status tracking
- **request_logs** - Full request/response logs (respecting team logging policy)

### Key Design Decisions
- Soft deletes for teams (`deleted_at`)
- Team-based logging policies (full, metadata, zero, compliance)
- Automatic `updated_at` triggers
- Comprehensive indexing for performance

## Development Environment

Uses **devenv** (Nix-based) for reproducible development:
- Auto-starts PostgreSQL and Redis when running `devenv up`
- Environment variables auto-set on shell entry:
  - `DATABASE_URL` - Main database connection
  - `TEST_DATABASE_URL` - Test database
  - `REDIS_URL` - Redis connection
- Process management with health checks and auto-restart

## Testing Strategy

- Unit tests in feature modules (e.g., `src/features/health/api.rs`)
- Integration tests in `tests/` directory
- Use `cargo nextest` for parallel test execution
- Database-dependent tests require services to be running (`services-up`)
- Test database auto-reset with `db-reset`

## Key Dependencies

- **axum** - Web framework with `#[derive(Router)]` support
- **sqlx** - Compile-time checked SQL with PostgreSQL
- **casbin** - Flexible RBAC authorization
- **pasetors** - PASETO token authentication
- **kcl-lang** - KCL configuration language runtime
- **reqwest** - HTTP client for backend communication
- **governor** - Rate limiting
- **opentelemetry** - Distributed tracing

## Current Development Status

Early development phase:
- ✅ Project structure and clean architecture
- ✅ Health check endpoint
- ✅ Error handling foundation
- ✅ Observability setup (tracing)
- ✅ Database schema and migrations
- ✅ KCL configuration schemas
- ⏳ Configuration loading implementation
- ⏳ Authentication implementation
- ⏳ Team management API
- ⏳ Backend management and health monitoring
- ⏳ Routing strategies
- ⏳ OpenAI-compatible endpoints

## Development Notes

- **Rust Edition**: Uses Rust Edition 2024 (requires Rust 1.89+)
- **Clean Architecture**: Always define domain traits before infrastructure
- **Feature Flags**: Optional `caching` feature for Redis support
- **KCL Dependency**: Custom fork for serde_json compatibility (temporary)
- **Container Builds**: Production container defined in `devenv.nix` using Crane
