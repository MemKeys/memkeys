# syntax=docker/dockerfile:1

# Multi-stage build: rust builder -> minimal runtime, non-root.
# PR CI builds this image with buildx (load only, no push) via docker.yml.
# Multi-arch push to ghcr.io/memkeys/memkeys-server runs on main/tags only
# (release.yml, ISI-6304). arm64 is built under QEMU emulation for v1 because
# cargo compiles inside the image; the gha build cache keeps it tractable.
# Later optimization: cargo cross-compilation to drop the arm64 emulation.

# --- ui --------------------------------------------------------------------
# Inspector SPA (ISI-6476, parent ISI-6395 "website without nginx"): build
# the Vite bundle once and ship it INSIDE the server image. The server serves
# it at / on the memory-transport listener (MEMKEYS_UI_DIR below), so no
# nginx sidecar is needed. The server binary reads the dir at runtime — no
# compile-time coupling: `cargo build` in the builder stage is unaffected.
FROM node:20-slim AS uibuild
WORKDIR /ui
COPY frontend/package.json frontend/package-lock.json ./
RUN npm ci
COPY frontend ./
RUN npm run build

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

# Inspector SPA bundled nginx-free (see uibuild stage above).
COPY --from=uibuild /ui/dist /usr/share/memkeys/ui
ENV MEMKEYS_UI_DIR=/usr/share/memkeys/ui

USER memkeys

# Required at runtime: postgres connection string for migrations + FactStore.
# Optional: OTEL_EXPORTER_OTLP_ENDPOINT enables OTLP telemetry.
ENV RUST_LOG=info

# Health surface (GET /healthz, /readyz) — see crates/memkeys-server/src/health.rs.
EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/memkeys-server"]
