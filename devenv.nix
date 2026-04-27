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
    tokei
    curl
    just
    kcl
    sqlx-cli
    bacon
    cargo-nextest # advanced test runner
    cargo-tarpaulin # code coverage tool
    cargo-fuzz # fuzz testing
    cargo-rr # debugging tool
    # cargo-dist # TODO: consider adding for releases

    gh

    kcl-language-server

    glib
    glibc

    pnpm
    nodejs_24
  ];

  # https://devenv.sh/languages/
  languages.rust.enable = true;
  languages.typescript.enable = true;

  # https://devenv.sh/processes/
  processes.thalamus = {
    exec = ''
      ${lib.getExe pkgs.bacon} --headless --config-toml '
      default_job = "run"
      [jobs.run]
      command = ["cargo", "run"]
      need_stdout = true
      background = false
      on_change_strategy = "kill_then_restart"
      kill = ["kill", "-s", "INT"]
      '
    '';
    ready = {
      http.get = {
        port = 3000;
        path = "/health";
      };
      period = 10;
      failure_threshold = 3;
    };
  };

  # https://devenv.sh/services/
  services.postgres = {
    enable = true;
    listen_addresses = "127.0.0.1";
    initialDatabases = [
      { name = "thalamus"; }
      { name = "thalamus_test"; }
    ];
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
    dev.exec = ''
      ${lib.getExe pkgs.bacon} --config-toml '
            default_job = "run"
            [jobs.run]
            command = ["cargo", "run"]
            need_stdout = true
            background = false
            on_change_strategy = "kill_then_restart"
            kill = ["kill", "-s", "INT"]
            '
    '';

    # Test commands
    test.exec = "cargo nextest run";
    test-verbose.exec = "cargo nextest run --success-output immediate";
    test-ci.exec = "cargo nextest run --profile ci";
    test-cargo.exec = "cargo test";

    # Coverage commands
    test-coverage.exec = ''
      echo "Running tests with coverage instrumentation..."
      cargo tarpaulin --out Html --output-dir coverage \
        --ignore-tests --features all \
        --exclude-files "ui/*" --timeout 120
      echo ""
      echo "Coverage report generated at: coverage/tarpaulin-report.html"
      echo "Open with: xdg-open coverage/tarpaulin-report.html (Linux) or open coverage/tarpaulin-report.html (macOS)"
    '';
    test-coverage-xml.exec = ''
      echo "Running tests with coverage (XML output for CI)..."
      cargo tarpaulin --out Xml --output-dir coverage \
        --ignore-tests --features all \
        --exclude-files "ui/*" --timeout 120
      echo ""
      echo "Coverage XML report generated at: coverage/cobertura.xml"
    '';
    test-coverage-lcov.exec = ''
      echo "Running tests with coverage (LCOV output)..."
      cargo tarpaulin --out Lcov --output-dir coverage \
        --ignore-tests --features all \
        --exclude-files "ui/*" --timeout 120
      echo ""
      echo "Coverage LCOV report generated at: coverage/lcov.info"
    '';

    # Debugging and development helpers
    test-watch.exec = ''
      echo "Starting test watcher (ignores target/, ui/, .devenv/, .git/)..."
      ${lib.getExe pkgs.bacon} --config-toml '
            default_job = "test"
            [jobs.test]
            command = ["cargo", "nextest", "run"]
            need_stdout = true
            background = false
            on_change_strategy = "kill_then_restart"
            kill = ["kill", "-s", "INT"]
            watch = ["src", "tests"]
            '
    '';
    test-one.exec = ''
      if [ -z "$1" ]; then
        echo "Usage: test-one <test_pattern>"
        echo "Example: test-one test_api_key_lifecycle"
        exit 1
      fi
      echo "Running test matching pattern: $1"
      cargo nextest run -- "$1"
    '';
    test-debug.exec = ''
      if [ -z "$1" ]; then
        echo "Usage: test-debug <test_name>"
        echo "Example: test-debug test_api_key_lifecycle"
        exit 1
      fi
      echo "Running test with full debugging output: $1"
      echo "Set RUST_LOG=thalamus=debug,sqlx=trace for more verbose output"
      RUST_BACKTRACE=full RUST_LOG=thalamus=debug,sqlx=warn cargo nextest run --profile debug -- "$1"
    '';
    test-seed.exec = ''
      echo "Seeding test data into development database..."
      echo "This will insert sample users, teams, API keys, and backends"
      cargo test --test test_data -- --ignored --nocapture 2>/dev/null || true
      echo ""
      echo "Test data seeded successfully!"
      echo "You can now use the seeded data for manual testing."
    '';

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
      psql "$DATABASE_URL" -c "DROP SCHEMA public CASCADE; CREATE SCHEMA public;" || true
      sqlx migrate run --database-url "$DATABASE_URL"
    '';

    # Container
    container-build.exec = "devenv build container-prod";
    docker-build.exec = ''
      store_path=$(devenv build outputs.container-prod-docker 2>/dev/null | ${lib.getExe pkgs.jq} -r '."outputs.container-prod-docker"')
      exec "$store_path/bin/copy-to-docker-daemon"
    '';
    podman-build.exec = ''
      store_path=$(devenv build outputs.container-prod-podman 2>/dev/null | ${lib.getExe pkgs.jq} -r '."outputs.container-prod-podman"')
      exec "$store_path/bin/copy-to-podman"
    '';

    # Service management
    services-up.exec = "devenv up -d postgres redis";
    services-down.exec = "devenv processes down";

    # Utilities
    clean.exec = "cargo clean";
    update.exec = "cargo update";

    # Frontend (ui)
    ui-dev.exec = "cd ui && pnpm start --web";
    ui-lint.exec = "cd ui && pnpm run lint";
    ui-format.exec = "cd ui && pnpm run format";
  };

  # https://devenv.sh/basics/
  enterShell = ''
    export DATABASE_URL="postgresql://$USER@localhost:5432/thalamus"
    export TEST_DATABASE_URL="postgresql://$USER@localhost:5432/thalamus_test"
    export REDIS_URL="redis://localhost:6379"
    echo ""
    echo "Services:"
    echo "  PostgreSQL: $DATABASE_URL"
    echo "  Redis:      $REDIS_URL"
    echo ""
    echo "Quick Commands:"
    echo "  build          - Build the project"
    echo "  run            - Run the server"
    echo "  dev            - Run with auto-reload (cargo watch)"
    echo "  test           - Run tests with nextest"
    echo "  check          - Check code without building"
    echo "  lint           - Run clippy linter"
    echo "  fmt            - Format code"
    echo "  ci             - Run all CI checks"
    echo ""
    echo "Testing & Coverage:"
    echo "  test-verbose         - Run tests with immediate output"
    echo "  test-ci              - Run tests with CI profile"
    echo "  test-watch           - Run tests continuously on file changes"
    echo "  test-one <pattern>   - Run a single test by name/pattern"
    echo "  test-debug <name>    - Run single test with backtrace and logging"
    echo "  test-seed            - Insert sample test data into dev database"
    echo ""
    echo "Coverage (requires services-up + db-migrate):"
    echo "  test-coverage        - Run coverage and generate HTML report"
    echo "  test-coverage-xml    - Generate XML report for CI integration"
    echo "  test-coverage-lcov   - Generate LCOV report"
    echo ""
    echo "Database:"
    echo "  db-migrate            - Run migrations"
    echo "  db-create-migration   - Create new migration"
    echo "  db-reset              - Reset test database"
    echo ""
    echo "Services:"
    echo "  services-up    - Start PostgreSQL and Redis"
    echo "  services-down  - Stop services"
    echo ""
    echo "Run 'devenv --help' for more options"
    echo ""
  '';

  # https://devenv.sh/tasks/
  tasks = {
    # Run migrations on shell entry if database is available
    "thalamus:db-check" = {
      exec = ''
        if pg_isready -h localhost -U $USER -d thalamus > /dev/null 2>&1; then
          echo "Database is ready"
        else
          echo "Database not running. Start with: services-up"
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
      check-json.enable = true;
      mixed-line-endings.enable = true;
    };
  };

  # Production container — uses nix2container directly to avoid devenv's
  # shell environment wrapping (which pulls in the entire dev closure).
  outputs =
    let
      nix2container = inputs.nix2container.packages.${pkgs.system}.nix2container;

      # Create Crane library instance
      craneLib = inputs.crane.mkLib pkgs;

      # Extract rustc from crane's scope for disallowedReferences
      rustc = craneLib.callPackage ({ rustc, ... }: rustc) { };

      # Filter source to only include relevant files for Rust builds
      src = lib.cleanSourceWith {
        src = ./.;
        filter =
          path: type:
          # Include migrations, pkg, and sqlx offline cache directories
          (lib.hasSuffix "/migrations" path)
          || (lib.hasSuffix "/pkg" path)
          || (lib.hasInfix "/.sqlx/" path)
          ||
            # Include all Rust source files
            (craneLib.filterCargoSources path type);
      };

      # Common arguments for crane builds
      commonArgs = {
        inherit src;
        pname = "thalamus";
        version = "0.1.0";
        strictDeps = true;

        # Skip tests in container builds (no database available)
        doCheck = false;

        # Enable sqlx offline mode (no database needed at compile time)
        SQLX_OFFLINE = "true";

        nativeBuildInputs = with pkgs; [
          pkg-config
        ];

        buildInputs = lib.optionals pkgs.stdenv.isDarwin [
          pkgs.darwin.apple_sdk.frameworks.SystemConfiguration
        ];
      };

      # Build dependencies first for better caching
      cargoArtifacts = craneLib.buildDepsOnly commonArgs;

      # Build the actual application
      thalamusApp = craneLib.buildPackage (
        commonArgs
        // {
          inherit cargoArtifacts;

          # Fail the build if build-time dependencies leak into the output
          disallowedReferences = [
            rustc
            pkgs.stdenv.cc
          ];

          # Copy additional assets alongside the binary
          postInstall = ''
            mkdir -p $out/migrations
            mkdir -p $out/pkg

            # Copy migrations if they exist
            if [ -d migrations ]; then
              cp -r migrations/* $out/migrations/ 2>/dev/null || true
            fi

            # Copy KCL schemas if they exist
            if [ -d pkg ]; then
              cp -r pkg/* $out/pkg/ 2>/dev/null || true
            fi
          '';

          # Strip embedded Nix store path references to build-time dependencies
          # Note: crane's buildPackage already strips rustc refs via
          # removeReferencesToRustToolchainHook, but we also strip cc
          postFixup = ''
            find $out -type f -executable -exec \
              ${pkgs.removeReferencesTo}/bin/remove-references-to \
                -t ${rustc} \
                -t ${pkgs.stdenv.cc} \
                {} \;
          '';

          meta = {
            description = "Backend-centric LLM router and load balancer";
            license = with lib.licenses; [
              mit
              asl20
            ];
          };
        }
      );

      rootfs = pkgs.buildEnv {
        name = "thalamus-root";
        paths = [
          pkgs.cacert # CA certificates for HTTPS
          pkgs.busybox # Optional: minimal shell + utils for debugging. Remove for smallest image.
          thalamusApp
        ];
        pathsToLink = [
          "/bin"
          "/migrations"
          "/pkg"
          "/etc"
        ];
      };

      image = nix2container.buildImage {
        name = "thalamus";
        tag = "latest";

        copyToRoot = [ rootfs ];

        # Separate stable deps from app binary for better incremental push/pull
        maxLayers = 100;

        config = {
          Entrypoint = [ "${thalamusApp}/bin/thalamus" ];
          Env = [
            "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
          ];
        };
      };
    in
    {
      container-prod = image;
      container-prod-docker = image.copyToDockerDaemon;
      container-prod-podman = image.copyToPodman;
    };

  # other integrations
  delta.enable = true;
  devcontainer = {
    enable = true;
    settings.customizations.vscode.extensions = [
      "mkhl.direnv"
      "rust-lang.rust-analyzer"
      "vadimcn.vscode-lldb"
      "kcl.kcl-vscode-extension"
      "tamasfe.even-better-toml"
    ];
  };
  # See full reference at https://devenv.sh/reference/options/
}
