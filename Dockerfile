# syntax=docker/dockerfile:1.6
#
# Multi-stage build for the pharos backend. Produces a small linux
# image with the release binary + ffmpeg, runnable on any docker host
# (linux native or darwin/windows via docker-desktop's linux VM).
#
# Reproducibility: pinned by rust-toolchain.toml + Cargo.lock. BuildKit
# cache mounts speed iteration without leaking artefacts into the
# image. `nix build .#oci` remains the alternative pure-nix path on
# linux hosts; this Dockerfile is what `scripts/dev-stack.sh` uses
# because it works cross-OS unmodified.

ARG RUST_VERSION=1.95.0
ARG DEBIAN_VERSION=bookworm-slim

############################
# 1. Build stage
############################
FROM rust:${RUST_VERSION}-${DEBIAN_VERSION} AS builder

WORKDIR /src

# Native build deps. pkg-config + libssl-dev cover sqlx + reqwest TLS
# (we use rustls in pharos but a few transitive crates probe for
# OpenSSL via pkg-config; cheap to install).
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      pkg-config \
      libssl-dev \
      ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# Bring in the lockfile + manifests first so the dep-build layer is
# cacheable across source edits.
COPY rust-toolchain.toml ./
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY .config ./.config

# Build the release binary. BuildKit cache mounts persist the cargo
# registry + target dir across image rebuilds.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p pharos-server --bin pharos \
 && cp /src/target/release/pharos /pharos

############################
# 2. Runtime stage
############################
FROM debian:${DEBIAN_VERSION} AS runtime

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ffmpeg \
      ca-certificates \
      tini \
 && rm -rf /var/lib/apt/lists/*

# Non-root user owns the data dirs so volume mounts don't leak root
# inode ownership back onto the host bind path.
RUN groupadd --system pharos \
 && useradd --system --gid pharos --home /var/lib/pharos --shell /usr/sbin/nologin pharos \
 && mkdir -p /var/lib/pharos/db /var/lib/pharos/media /var/lib/pharos/cache /etc/pharos \
 && chown -R pharos:pharos /var/lib/pharos /etc/pharos

COPY --from=builder /pharos /usr/local/bin/pharos

USER pharos
WORKDIR /var/lib/pharos

EXPOSE 8096

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/pharos"]
CMD ["--config", "/etc/pharos/config.toml", "serve"]
