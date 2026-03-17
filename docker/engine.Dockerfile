# Multi-stage Dockerfile for feynman-engine
# Stage 1: Build
# Stage 2: Runtime

# ── Build Stage ──
FROM rust:1.82-bookworm AS builder

WORKDIR /build

# Copy dependency manifest
COPY Cargo.toml Cargo.lock ./

# Copy source code
COPY crates crates
COPY bins bins
COPY proto proto

# Build
RUN cargo build --release --bin feynman-engine

# ── Runtime Stage ──
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Non-root user
RUN groupadd -r feynman && useradd -r -g feynman feynman

# Copy binary from builder
COPY --from=builder /build/target/release/feynman-engine /usr/local/bin/

# Copy config template
COPY config/default.toml /config/default.toml

# Health probe
COPY --from=fullstorydev/grpc-health-probe:v0.4.28 \
    /bin/grpc_health_probe /usr/local/bin/

# Change ownership
RUN chown -R feynman:feynman /config

USER feynman

EXPOSE 50051 8080

ENTRYPOINT ["feynman-engine"]
CMD ["--config", "/config/default.toml"]
