FROM rust:1.83-slim AS builder

WORKDIR /app

# Install build dependencies for OpenSSL / native TLS
RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

# Cache dependency builds: copy manifests first, then do a dummy build
COPY Cargo.toml Cargo.lock* ./
RUN mkdir src && echo "fn main() {}" > src/main.rs && cargo build --release 2>/dev/null || true && rm -rf src

# Copy full source and build for real
COPY src/ src/
RUN cargo build --release

# --- Runtime stage ---
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates libssl3 && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/copytrading-bot /usr/local/bin/copytrading-bot

EXPOSE 8080

ENTRYPOINT ["copytrading-bot"]
