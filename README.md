# Thalmus - Backend-Centric LLM Router

[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue)](LICENSE-MIT)
[![Rust](https://img.shields.io/badge/rust-1.89%2B-orange)](https://www.rust-lang.org/)

A configuration-based, backend-centric LLM router and load balancer built with Rust and Axum. Thalmus provides advanced routing capabilities, team-based access control, and comprehensive observability for LLM deployments.

## Features (Planned)

### 🎯 Backend-Centric Architecture
- **Model Loading Awareness**: Route requests to backends that already have models loaded
- **Dynamic Capacity Management**: Real-time monitoring of backend queue depth and processing capacity
- **Tag-Based Filtering**: Flexible backend selection through client-specified tags

### 🔐 Security & Authentication
- **Multi-layered Authentication**: API keys, HTTP Signatures (RFC 9421), and PASETO tokens
- **Casbin-Based Authorization**: Flexible RBAC with domain (team) support
- **Team-Based Access Control**: Hierarchical team management with granular permissions
- **Service Accounts**: Machine-to-machine authentication support

### 🚀 Advanced Features
- **Priority Queues**: Separate queues for real-time and batch processing
- **Rate Limiting**: Multi-dimensional rate limiting per team, user, and globally
- **Pluggable Architecture**: WASM-based plugin system for routing strategies and filters
- **Comprehensive Observability**: OpenTelemetry support with first-class Langfuse integration

## Development Setup

This project uses [devenv](https://devenv.sh/) for a reproducible development environment.

### Prerequisites

- [Nix](https://nixos.org/download.html) with flakes enabled
- [devenv](https://devenv.sh/getting-started/) installed

### Quick Start

```bash
# Clone the repository
git clone https://github.com/yourusername/thalmus.git
cd thalmus

# Enter the development environment
devenv shell

# Start PostgreSQL and Redis services
services-up

# Run database migrations (when available)
db-migrate

# Run the server
run

# Or run with auto-reload
dev
```

## Available Commands

When in the devenv shell, you have access to these commands:

### Build & Run
- `build` - Build the project in debug mode
- `build-release` - Build in release mode (optimized)
- `run` - Run the server
- `dev` - Run with auto-reload on file changes

### Testing
- `test` - Run tests with nextest
- `test-verbose` - Run tests with immediate output
- `test-ci` - Run tests in CI mode (fail-fast, no retries)
- `test-cargo` - Run tests with standard cargo test

### Code Quality
- `check` - Check code without building
- `lint` - Run clippy linter
- `fmt` - Format code with rustfmt
- `fmt-check` - Check formatting without modifying
- `ci` - Run all CI checks (fmt + lint + test)

### Database
- `db-migrate` - Run database migrations
- `db-create-migration <name>` - Create a new migration
- `db-reset` - Reset the test database

### Services
- `services-up` - Start PostgreSQL and Redis in background
- `services-down` - Stop services
- `services-logs` - View service logs

### Utilities
- `clean` - Clean build artifacts
- `update` - Update dependencies

## Project Structure

```
thalmus/
├── src/
│   ├── features/          # Feature modules (clean architecture)
│   │   ├── health/        # Health check
│   │   ├── auth/          # Authentication
│   │   ├── authorization/ # Casbin authz
│   │   ├── teams/         # Team management
│   │   ├── backends/      # Backend management
│   │   ├── routing/       # LLM routing
│   │   └── llm_proxy/     # OpenAI-compatible API
│   ├── shared/            # Shared infrastructure
│   │   ├── config/        # KCL configuration
│   │   ├── database/      # SQLx pool
│   │   └── observability/ # Tracing & metrics
│   ├── middleware/        # Global middleware
│   ├── bootstrap.rs       # Dependency injection
│   ├── error.rs           # Error types
│   └── main.rs            # Entry point
├── tests/                 # Integration tests
├── migrations/            # SQL migrations
├── devenv.nix             # Development environment
└── Cargo.toml             # Dependencies
```

## Architecture

Thalmus follows **clean architecture** principles:

- **Features**: Organized by domain (health, auth, teams, etc.)
  - `api.rs` - HTTP handlers and routing
  - `domain.rs` - Business logic and trait definitions
  - `infra.rs` - Infrastructure implementations
  - `dto.rs` - Data transfer objects

- **Dependency Inversion**: Domain defines traits, infrastructure implements them
- **Test-Driven Development**: Write tests first, then implementation
- **Modular Design**: Each feature is independent and composable

## Configuration

Configuration uses [KCL](https://kcl-lang.io/) for type-safe, modular configuration with hot-reload support.

```kcl
# config.k (example - not yet implemented)
server = {
    host = "0.0.0.0"
    port = 3000
}

database = {
    url = env.DATABASE_URL
    max_connections = 10
}
```

## Testing

```bash
# Run all tests
test

# Run tests with output
test-verbose

# Run specific test
cargo nextest run test_health_check

# Run integration tests with database
# (Make sure services are running: services-up)
test
```

## Current Status

🚧 **Early Development** - This project is in active development.

- ✅ Project structure and dependencies
- ✅ Development environment (devenv)
- ✅ Health check endpoint
- ✅ Error handling foundation
- ✅ Observability setup (tracing)
- ⏳ Database schema and migrations
- ⏳ KCL configuration loading
- ⏳ Authentication (API keys, PASETO)
- ⏳ Authorization (Casbin)
- ⏳ Team management
- ⏳ Backend management
- ⏳ LLM routing strategies
- ⏳ OpenAI-compatible endpoints

## Contributing

Contributions are welcome! Please see [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines (coming soon).

## License

Thalmus is dual-licensed under MIT and Apache 2.0.

## Acknowledgments

- Inspired by [LiteLLM](https://github.com/BerriAI/litellm)
- Clean architecture pattern from [clean_axum_demo](https://github.com/sukjaelee/clean_axum_demo/)
