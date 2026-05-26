#!/usr/bin/env bash
# Reproducible local dev-stack: pharos + jellyfin-web as Nix-built
# distroless OCI containers, orchestrated via docker compose.
#
# Both images come from `dockerTools.buildLayeredImage` in flake.nix —
# only nix store paths inside, no debian/alpine base, no upstream
# Docker Hub pulls.
#
# * pharos:latest             — rust release binary + ffmpeg + cacert
#                               + tzdata + rootfs skel.
# * pharos-jellyfin-web:latest — darkhttpd serving the pinned
#                               nixpkgs#jellyfin-web bundle.
#
# On darwin the builds dispatch to the configured linux-builder
# (`/etc/nix/machines`) so the binaries inside the images are linux
# ELF regardless of build host.
#
# Usage:
#   nix run .#dev-stack            # via flake app
#   just dev-stack                 # via justfile
#   ./scripts/dev-stack.sh         # direct
#
# Ports (host):
#   8096  -> pharos backend
#   8097  -> jellyfin-web static bundle
#
# Persistent state (sqlite db + media fixtures + transcode cache)
# lives in docker volumes (`pharos_db`, `pharos_media`, `pharos_cache`)
# so it survives across runs. Set `CLEAN=1` to wipe them before
# starting.

set -euo pipefail

# Pick docker / podman + verify the daemon answers.
if command -v docker >/dev/null 2>&1; then
  DOCKER=docker
elif command -v podman >/dev/null 2>&1; then
  DOCKER=podman
else
  echo "error: neither docker nor podman in PATH" >&2
  exit 1
fi
if ! $DOCKER info >/dev/null 2>&1; then
  echo "error: $DOCKER CLI present but daemon is unreachable." >&2
  echo "  - macOS:   start Docker Desktop, or 'colima start'." >&2
  echo "  - Linux:   'systemctl --user start docker' / start dockerd." >&2
  echo "  - podman:  'podman machine start'." >&2
  exit 1
fi

# docker compose v2 ships as a CLI plugin (`docker compose`); v1 is
# the legacy standalone `docker-compose`. Prefer v2.
if $DOCKER compose version >/dev/null 2>&1; then
  COMPOSE=("$DOCKER" compose)
elif command -v docker-compose >/dev/null 2>&1; then
  COMPOSE=(docker-compose)
else
  echo "error: docker compose (v2 plugin or v1 standalone) required." >&2
  exit 1
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

STATE_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/pharos-dev-stack"
mkdir -p "$STATE_DIR"

# Pick the linux system attr that matches the docker host. On linux
# this is the local system; on darwin it dispatches to linux-builder.
HOST=$(uname -m)
case "$HOST" in
  arm64|aarch64) LINUX_SYSTEM="aarch64-linux" ;;
  x86_64|amd64)  LINUX_SYSTEM="x86_64-linux" ;;
  *)
    echo "error: unsupported host arch $HOST" >&2
    exit 1
    ;;
esac

# Build both OCI images via nix dockerTools. Identical inputs +
# flake.lock → identical image bytes.
echo ">>> building pharos OCI image"
PHAROS_OCI=$(nix build ".#packages.${LINUX_SYSTEM}.oci" --no-link --print-out-paths)
echo ">>> building jellyfin-web OCI image"
JELLYFIN_OCI=$(nix build ".#packages.${LINUX_SYSTEM}.jellyfinWebOci" --no-link --print-out-paths)
echo ">>> resolving compose manifest"
COMPOSE_SRC=$(nix build ".#packages.${LINUX_SYSTEM}.composeFile" --no-link --print-out-paths)

echo ">>> loading images into $DOCKER"
$DOCKER load < "$PHAROS_OCI"   >/dev/null
$DOCKER load < "$JELLYFIN_OCI" >/dev/null

# Materialise the compose file + pharos config under $STATE_DIR
# (a host path the daemon shares unconditionally). The compose
# manifest references a host bind-mount for the pharos config so
# edits don't need a rebuild.
CONFIG_PATH="$STATE_DIR/config.toml"
cat > "$CONFIG_PATH" <<TOML
[server]
bind = "0.0.0.0:8096"
name = "pharos-dev"
transcode_cache_dir = "/var/lib/pharos/cache"

[obs]
log_level = "info"

[media]
roots = ["/var/lib/pharos/media"]

[database]
url = "sqlite:///var/lib/pharos/db/pharos.db?mode=rwc"
TOML
COMPOSE_FILE="$STATE_DIR/docker-compose.yml"
cp -f "$COMPOSE_SRC" "$COMPOSE_FILE"
chmod u+w "$COMPOSE_FILE"
export PHAROS_CONFIG_HOST="$CONFIG_PATH"

if [ "${CLEAN:-0}" = "1" ]; then
  echo ">>> wiping volumes"
  "${COMPOSE[@]}" -f "$COMPOSE_FILE" down -v >/dev/null 2>&1 || true
fi

# Seed playwright user + WebM fixture in a one-shot pharos container
# sharing the same docker volumes the long-running serve container
# will mount.
echo ">>> seeding playwright user + fixture"
"${COMPOSE[@]}" -f "$COMPOSE_FILE" run --rm \
  --entrypoint /bin/pharos \
  pharos \
  --config /etc/pharos/config.toml admin seed-playwright-user \
  || echo "    (seed may have already happened; continuing)"

cleanup() {
  echo
  echo ">>> stopping dev-stack"
  "${COMPOSE[@]}" -f "$COMPOSE_FILE" down >/dev/null 2>&1 || true
}
trap cleanup INT TERM EXIT

echo ">>> starting stack"
"${COMPOSE[@]}" -f "$COMPOSE_FILE" up -d

echo
echo "    pharos       -> http://localhost:8096"
echo "    jellyfin-web -> http://localhost:8097"
echo "    seeded user  -> playwright / playwright-test-pw"
echo "    state dir    -> $STATE_DIR"
echo "    volumes      -> pharos_db, pharos_media, pharos_cache  (docker volume ls)"
echo
echo "Ctrl-C to stop. Containers tear down via the trap."
echo

# Stream pharos logs as the foreground process.
"${COMPOSE[@]}" -f "$COMPOSE_FILE" logs -f pharos
