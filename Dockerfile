FROM node:24-bookworm-slim AS ui-build
WORKDIR /src/ui
RUN corepack enable && corepack prepare pnpm@10.17.1 --activate
COPY ui/package.json ui/pnpm-lock.yaml ./
RUN pnpm install --frozen-lockfile
COPY ui/ ./
RUN pnpm run build

FROM rust:1.94.1-slim-bookworm AS rust-build
WORKDIR /src
RUN apt-get update && apt-get install -y --no-install-recommends build-essential pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/
RUN cargo build --release --locked -p thalamus-server -p thalamus-worker

FROM debian:bookworm-slim AS kcl-download
ARG TARGETARCH
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
# Digests published by the KCL CLI v0.13.2 release. Fail closed on other architectures.
RUN case "$TARGETARCH" in \
      amd64) digest=16db3412892e83ae7a4b34953cadcd99649f3abaebbd42f2fb830705173e7ae4 ;; \
      arm64) digest=a6dd6b6cee05fc71f01c9e71e9c65c946d195a15fccfd3945186326869667656 ;; \
      *) echo "Unsupported KCL architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && curl -fsSL "https://github.com/kcl-lang/cli/releases/download/v0.13.2/kcl-v0.13.2-linux-${TARGETARCH}.tar.gz" -o /tmp/kcl.tar.gz \
    && echo "$digest  /tmp/kcl.tar.gz" | sha256sum -c - \
    && tar -xzf /tmp/kcl.tar.gz -C /usr/local/bin kcl

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl git libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 thalamus
COPY --from=kcl-download /usr/local/bin/kcl /usr/local/bin/kcl
COPY --from=rust-build /src/target/release/thalamus-server /usr/local/bin/thalamus-server
COPY --from=rust-build /src/target/release/thalamus-worker /usr/local/bin/thalamus-worker
COPY --from=ui-build /src/ui/build /app/ui
ENV THALAMUS_UI_DIR=/app/ui THALAMUS_BIND=0.0.0.0:8080
WORKDIR /app
USER thalamus
EXPOSE 8080
CMD ["thalamus-server"]
