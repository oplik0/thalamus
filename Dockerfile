FROM rust:1-slim-trixie AS builder

WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock* ./
COPY .sqlx/ .sqlx/

RUN rm -rf src && mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release --features caching

COPY migrations/ ./migrations/
COPY pkg/ ./pkg/
COPY src/ ./src/
COPY casbin_model.conf ./

RUN rm -f target/release/.cargo-lock target/release/thalamus
RUN cargo build --release --features caching --locked

FROM debian:trixie-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --shell /bin/bash app

WORKDIR /app

COPY --from=builder /app/target/release/thalamus /usr/local/bin/
COPY --from=builder /app/migrations/ /app/migrations/
COPY --from=builder /app/pkg/ /app/pkg/
COPY --from=builder /app/casbin_model.conf /app/

RUN mkdir -p /app/config && chown -R app:app /app

USER app

EXPOSE 3000

ENV SQLX_OFFLINE=true
ENV RUST_LOG=debug

ENTRYPOINT ["thalamus"]
