#!/usr/bin/env bash
# Reproducible local dev-stack: pharos + jellyfin-web as containers.
#
# Both images come from the Nix flake — `nix build .#oci` produces the
# pharos image, and the jellyfin-web bundle is materialised from the
# pinned `pkgs.jellyfin-web` derivation into a temp dir that nginx
# mounts. Tooling required: docker (or podman) + a working `nix build`.
#
# Usage:
#   nix run .#dev-stack           # via flake app
#   ./scripts/dev-stack.sh        # direct
#
# Ports (host):
#   8096  -> pharos backend
#   8097  -> jellyfin-web static bundle (configures itself to point at
#           http://localhost:8096 via the connect-server flow)
#
# Persistent state lives under $XDG_DATA_HOME/pharos-dev-stack (default
# ~/.local/share/pharos-dev-stack) so the seeded user + DB survive
# across runs. Set CLEAN=1 to wipe before starting.

set -euo pipefail

# Pick docker / podman.
if command -v docker >/dev/null 2>&1; then
  DOCKER=docker
elif command -v podman >/dev/null 2>&1; then
  DOCKER=podman
else
  echo "error: neither docker nor podman in PATH" >&2
  exit 1
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

STATE_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/pharos-dev-stack"
if [ "${CLEAN:-0}" = "1" ]; then
  echo ">>> wiping $STATE_DIR"
  rm -rf "$STATE_DIR"
fi
mkdir -p "$STATE_DIR/db" "$STATE_DIR/media" "$STATE_DIR/cache"

# 1. Build the pharos OCI image and load it into the docker daemon.
echo ">>> building pharos OCI image"
OCI_TARBALL=$(nix build .#oci --no-link --print-out-paths)
echo ">>> loading pharos:latest into $DOCKER"
$DOCKER load < "$OCI_TARBALL" >/dev/null

# 2. Materialise jellyfin-web into a host path nginx can bind-mount.
echo ">>> resolving jellyfin-web bundle"
JELLYFIN_WEB_SRC=$(nix eval --raw \
  --expr "(builtins.getFlake (toString ./.)).inputs.nixpkgs.legacyPackages.\${builtins.currentSystem}.jellyfin-web" \
  2>/dev/null || true)
if [ -z "$JELLYFIN_WEB_SRC" ] || [ ! -d "$JELLYFIN_WEB_SRC/share/jellyfin-web" ]; then
  # Fallback: ask nix to print the package's outPath directly via
  # `nix build` without linking.
  JELLYFIN_WEB_OUT=$(nix build --no-link --print-out-paths \
    "nixpkgs#jellyfin-web")
  JELLYFIN_WEB_SRC="$JELLYFIN_WEB_OUT"
fi
JELLYFIN_WEB_DIR="$JELLYFIN_WEB_SRC/share/jellyfin-web"
echo "    jellyfin-web -> $JELLYFIN_WEB_DIR"

# Write a config.toml the OCI container reads at startup.
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

# 3. Seed a known user + a small video fixture so manual testing has
#    something to click. Runs inside a one-shot container that shares
#    the same volumes as the long-running serve container.
echo ">>> seeding playwright user + fixture"
$DOCKER run --rm \
  -v "$STATE_DIR/db:/var/lib/pharos/db" \
  -v "$STATE_DIR/media:/var/lib/pharos/media" \
  -v "$STATE_DIR/cache:/var/lib/pharos/cache" \
  -v "$CONFIG_PATH:/etc/pharos/config.toml:ro" \
  pharos:latest \
  admin seed-playwright-user \
  || echo "    (seed may have already happened; continuing)"

# 4. Run docker compose to bring up both services. We synthesise the
#    compose file inline so the script is self-contained.
COMPOSE_FILE="$STATE_DIR/compose.yml"
cat > "$COMPOSE_FILE" <<YAML
services:
  pharos:
    image: pharos:latest
    container_name: pharos-dev-stack
    network_mode: host
    restart: unless-stopped
    volumes:
      - $STATE_DIR/db:/var/lib/pharos/db
      - $STATE_DIR/media:/var/lib/pharos/media
      - $STATE_DIR/cache:/var/lib/pharos/cache
      - $CONFIG_PATH:/etc/pharos/config.toml:ro
  jellyfin-web:
    image: nginx:alpine
    container_name: pharos-jellyfin-web
    network_mode: host
    restart: unless-stopped
    command: >
      sh -c "echo 'server { listen 8097 default_server; root /usr/share/jellyfin-web; index index.html; location / { try_files \$\$uri \$\$uri/ /index.html; } }' > /etc/nginx/conf.d/default.conf && exec nginx -g 'daemon off;'"
    volumes:
      - $JELLYFIN_WEB_DIR:/usr/share/jellyfin-web:ro
YAML

echo ">>> starting stack"
echo "    pharos       -> http://localhost:8096"
echo "    jellyfin-web -> http://localhost:8097"
echo "    seeded user  -> playwright / playwright-test-pw"
echo "    state dir    -> $STATE_DIR"
echo

trap '$DOCKER compose -f "$COMPOSE_FILE" down' INT TERM
$DOCKER compose -f "$COMPOSE_FILE" up
