{
  pkgs,
  lib,
  config,
  inputs,
  ...
}:

{
  # https://devenv.sh/packages/
  packages = with pkgs; [
    git
    jq
    curl
    just
    kcl
  ];

  # https://devenv.sh/languages/
  languages.rust.enable = true;
  languages.typescript.enable = true;

  # https://devenv.sh/processes/
  # processes.dev.exec = "${lib.getExe pkgs.watchexec} -n -- ls -la";

  # https://devenv.sh/services/
  services.postgres = {
    enable = true;
    listen_addresses = "127.0.0.1";
    initialDatabases = [
      { name = "thalmus"; }
      { name = "thalmus_test"; }
    ];
    initialScript = ''
      CREATE USER IF NOT EXISTS postgres WITH PASSWORD 'postgres' SUPERUSER;
    '';
  };

  services.redis = {
    enable = true;
    bind = "127.0.0.1";
    port = 6379;
  };

  # https://devenv.sh/scripts/
  scripts = {
    # Build commands
    build.exec = "cargo build";
    build-release.exec = "cargo build --release";

    # Run commands
    run.exec = "cargo run";
    dev.exec = "cargo watch -x run";

    # Test commands
    test.exec = "cargo nextest run";
    test-verbose.exec = "cargo nextest run --success-output immediate";
    test-ci.exec = "cargo nextest run --profile ci";
    test-cargo.exec = "cargo test";

    # Code quality
    check.exec = "cargo check --all-targets";
    lint.exec = "cargo clippy --all-targets --all-features -- -D warnings";
    fmt.exec = "cargo fmt --all";
    fmt-check.exec = "cargo fmt --all -- --check";

    # CI - run all checks
    ci.exec = "cargo fmt --all -- --check && cargo clippy --all-targets --all-features -- -D warnings && cargo nextest run --profile ci";

    # Database commands
    db-migrate.exec = "sqlx migrate run";
    db-create-migration.exec = ''
      if [ -z "$1" ]; then
        echo "Usage: db-create-migration <migration_name>"
        exit 1
      fi
      sqlx migrate add "$1"
    '';
    db-reset.exec = ''
      psql -h localhost -U postgres -d thalmus_test -c "DROP SCHEMA public CASCADE; CREATE SCHEMA public;" || true
      sqlx migrate run --database-url "$TEST_DATABASE_URL"
    '';

    # Service management
    services-up.exec = "devenv up -d";
    services-down.exec = "devenv processes down";
    services-logs.exec = "devenv processes logs";

    # Utilities
    clean.exec = "cargo clean";
    update.exec = "cargo update";
  };

  # https://devenv.sh/basics/
  enterShell = ''
    export DATABASE_URL="postgresql://postgres:postgres@localhost:5432/thalmus"
    export TEST_DATABASE_URL="postgresql://postgres:postgres@localhost:5432/thalmus_test"
    export REDIS_URL="redis://localhost:6379"

    echo "╔════════════════════════════════════════════════════════════════╗"
    echo "║  🦀 Thalmus Development Environment                           ║"
    echo "╚════════════════════════════════════════════════════════════════╝"
    echo ""
    echo "📦 Services:"
    echo "  PostgreSQL: $DATABASE_URL"
    echo "  Redis:      $REDIS_URL"
    echo ""
    echo "🚀 Quick Commands:"
    echo "  build          - Build the project"
    echo "  run            - Run the server"
    echo "  dev            - Run with auto-reload (cargo watch)"
    echo "  test           - Run tests with nextest"
    echo "  check          - Check code without building"
    echo "  lint           - Run clippy linter"
    echo "  fmt            - Format code"
    echo "  ci             - Run all CI checks"
    echo ""
    echo "🗄️  Database:"
    echo "  db-migrate            - Run migrations"
    echo "  db-create-migration   - Create new migration"
    echo "  db-reset              - Reset test database"
    echo ""
    echo "⚙️  Services:"
    echo "  services-up    - Start PostgreSQL and Redis"
    echo "  services-down  - Stop services"
    echo "  services-logs  - View service logs"
    echo ""
    echo "Run 'devenv --help' for more options"
    echo ""
  '';

  # https://devenv.sh/tasks/
  tasks = {
    # Run migrations on shell entry if database is available
    "thalmus:db-check" = {
      exec = ''
        if pg_isready -h localhost -U postgres -d thalmus > /dev/null 2>&1; then
          echo "✓ Database is ready"
        else
          echo "⚠️  Database not running. Start with: services-up"
        fi
      '';
      after = [ "devenv:enterShell" ];
    };
  };

  # https://devenv.sh/tests/
  enterTest = ''
    git --version | grep --color=auto "${pkgs.git.version}"
  '';

  # https://devenv.sh/git-hooks/
  git-hooks = {
    package = pkgs.prek;
    hooks = {
      editorconfig-checker.enable = true;
      nixfmt.enable = true;
      clippy.enable = true;
      rustfmt.enable = true;
      # todo: uncomment once frontend is set up
      # biome.enable = true;
      end-of-file-fixer.enable = true;
      trim-trailing-whitespace.enable = true;
      check-added-large-files.enable = true;
      check-json.enable = true;
      mixed-line-endings.enable = true;
    };
  };

  # other integrations
  delta.enable = true;
  devcontainer.enable = true;
  # See full reference at https://devenv.sh/reference/options/
}
