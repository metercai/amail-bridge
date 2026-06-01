# ── Build stage ──
FROM rust:1.80-slim-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && \
    cargo build --release 2>/dev/null; true
COPY src/ src/
RUN cargo build --release

# ── Runtime stage ──
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/amail-bridge /usr/local/bin/
COPY amail_bridge.toml /etc/amail-bridge.toml

WORKDIR /etc
EXPOSE 38080

ENTRYPOINT ["/usr/local/bin/amail-bridge"]
