FROM node:24-bookworm-slim AS ui-build
WORKDIR /src/ui
RUN corepack enable && corepack prepare pnpm@10.17.1 --activate
COPY ui/package.json ui/pnpm-lock.yaml ./
RUN pnpm install --frozen-lockfile
COPY ui/ ./
RUN pnpm run build

FROM rust:1.94.1-slim-bookworm AS build
WORKDIR /src
# Functional smoke: avoid release/LTO cost and unused debug symbols.
ENV CARGO_BUILD_JOBS=2 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
RUN apt-get update && apt-get install -y --no-install-recommends build-essential pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/
RUN cargo build --locked -p thalamus-server -p thalamus-cli
FROM build AS identity-test
RUN cargo test --locked -p thalamus-identity --no-run
CMD ["cargo", "test", "--locked", "-p", "thalamus-identity", "keycloak_offboarding_revokes_existing_keys", "--", "--ignored"]


FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates git libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 10001 --create-home thalamus
COPY --from=build /src/target/debug/thalamus-server /usr/local/bin/thalamus-server
COPY --from=build /src/target/debug/thalamus /usr/local/bin/thalamus
COPY --from=ui-build /src/ui/build /app/ui
ENV THALAMUS_UI_DIR=/app/ui
USER thalamus
WORKDIR /app
ENTRYPOINT ["thalamus-server"]
