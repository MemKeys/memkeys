# syntax=docker/dockerfile:1

# Multi-stage build: rust builder -> minimal runtime, non-root.
# PR CI builds this image with buildx (load only, no push).
# GHCR push to ghcr.io/memkeys/memkeys runs on main/tags only (later phase).

# --- builder -------------------------------------------------------------
FROM rust:1-trixie AS builder
WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

RUN cargo build --release --locked --bin memkeys-server

# --- runtime -------------------------------------------------------------
# trixie (current Debian stable) rather than bookworm: bookworm's base
# packages currently carry several critical CVEs that will not be fixed
# there (grype gate fails on critical).
FROM debian:trixie-slim AS runtime

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 # pull in security updates published after the base image snapshot
 && apt-get upgrade -y \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --no-create-home --shell /usr/sbin/nologin memkeys

COPY --from=builder /build/target/release/memkeys-server /usr/local/bin/memkeys-server

USER memkeys

# Required at runtime: postgres connection string for migrations + FactStore.
# Optional: OTEL_EXPORTER_OTLP_ENDPOINT enables OTLP telemetry.
ENV RUST_LOG=info

# Health surface (GET /healthz, /readyz) — see crates/memkeys-server/src/health.rs.
EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/memkeys-server"]
