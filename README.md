# Thalamus - Backend-Centric LLM Router

[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue)](LICENSE-MIT)
[![Rust](https://img.shields.io/badge/rust-1.89%2B-orange)](https://www.rust-lang.org/)

A configuration-based, backend-centric LLM router and load balancer built with Rust and Axum. Thalamus provides advanced routing capabilities, team-based access control, and comprehensive observability for LLM deployments.

## Development Setup

This project uses [devenv](https://devenv.sh/) for a reproducible development environment.

### Prerequisites

- [Nix](https://nixos.org/download.html) with flakes enabled
- [devenv](https://devenv.sh/getting-started/) installed

### Quick Start

```bash
# Clone the repository
git clone https://github.com/yourusername/thalamus.git
cd thalamus

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
thalamus/
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

## Acknowledgments

- Inspired by [LiteLLM](https://github.com/BerriAI/litellm)
- Architecture pattern from [clean_axum_demo](https://github.com/sukjaelee/clean_axum_demo/)
