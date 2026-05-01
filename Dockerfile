FROM rust:1.94.1-slim-bookworm AS builder

WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Cache dependencies
COPY Cargo.toml Cargo.lock ./
COPY .sqlx/ .sqlx/

RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release --features caching
RUN rm -rf src

# Build application
COPY migrations/ ./migrations/
COPY pkg/ ./pkg/
COPY src/ ./src/
COPY casbin_model.conf ./

RUN rm -f target/release/deps/thalamus* target/release/thalamus*
RUN cargo build --release --features caching --locked

# Runtime — minimal distroless image
FROM gcr.io/distroless/cc-debian12:nonroot

COPY --from=builder /app/target/release/thalamus /usr/local/bin/
COPY --from=builder /app/migrations/ /migrations/
COPY --from=builder /app/pkg/ /pkg/
COPY --from=builder /app/casbin_model.conf /

ENV SQLX_OFFLINE=true
ENV RUST_LOG=info
EXPOSE 3000

USER nonroot

ENTRYPOINT ["thalamus"]
