#!/usr/bin/env bash
# Reproducible local dev-stack: pharos + jellyfin-web for manual testing.
#
# Both halves come from the flake — reproducible inputs, deterministic
# outputs:
#
# * Linux host: pharos runs as an OCI container built from `.#oci`,
#   jellyfin-web runs as nginx:alpine bind-mounting the pinned nixpkgs
#   jellyfin-web bundle.
# * macOS / non-Linux host: docker desktop runs linux containers, but
#   the flake's pharos derivation produces a host-OS binary, so we run
#   pharos as a host process (still via `nix build .#pharos`). jellyfin-web
#   stays in a container. The reproducibility argument is the same —
#   both binaries are pinned by flake.lock.
#
# Usage:
#   nix run .#dev-stack            # via flake app
#   just dev-stack                 # via justfile
#   ./scripts/dev-stack.sh         # direct
#
# Ports (host):
#   8096  -> pharos backend
#   8097  -> jellyfin-web static bundle (configure to point at
#            http://localhost:8096 via the connect-server flow)
#
# Persistent state lives under $XDG_DATA_HOME/pharos-dev-stack so the
# seeded user + DB survive across runs. Set CLEAN=1 to wipe.

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

UNAME=$(uname -s)
case "$UNAME" in
  Linux) IS_LINUX=1 ;;
  *)     IS_LINUX=0 ;;
esac

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

STATE_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/pharos-dev-stack"
if [ "${CLEAN:-0}" = "1" ]; then
  echo ">>> wiping $STATE_DIR"
  rm -rf "$STATE_DIR"
fi
mkdir -p "$STATE_DIR/db" "$STATE_DIR/media" "$STATE_DIR/cache"

# Resolve jellyfin-web from the pinned nixpkgs.
echo ">>> resolving jellyfin-web bundle"
JELLYFIN_WEB_OUT=$(nix build --no-link --print-out-paths "nixpkgs#jellyfin-web")
JELLYFIN_WEB_DIR="$JELLYFIN_WEB_OUT/share/jellyfin-web"
if [ ! -d "$JELLYFIN_WEB_DIR" ]; then
  echo "error: jellyfin-web bundle not found at $JELLYFIN_WEB_DIR" >&2
  exit 1
fi
echo "    jellyfin-web -> $JELLYFIN_WEB_DIR"

# Resolve pharos. Linux: OCI; otherwise: host binary.
if [ "$IS_LINUX" = "1" ]; then
  echo ">>> building pharos OCI image"
  OCI_TARBALL=$(nix build .#oci --no-link --print-out-paths)
  echo ">>> loading pharos:latest into $DOCKER"
  $DOCKER load < "$OCI_TARBALL" >/dev/null
else
  echo ">>> building pharos host binary (containers on $UNAME run linux only;"
  echo "    pharos runs on the host so it's the right OS/arch)"
  PHAROS_BIN_PATH=$(nix build .#pharos --no-link --print-out-paths)
  PHAROS_BIN="$PHAROS_BIN_PATH/bin/pharos"
  if [ ! -x "$PHAROS_BIN" ]; then
    echo "error: pharos binary not found at $PHAROS_BIN" >&2
    exit 1
  fi
fi

# Write a config.toml. Paths differ depending on whether pharos runs
# in-container (Linux) or on the host (other).
if [ "$IS_LINUX" = "1" ]; then
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
else
  CONFIG_PATH="$STATE_DIR/config.toml"
  cat > "$CONFIG_PATH" <<TOML
[server]
bind = "127.0.0.1:8096"
name = "pharos-dev"
transcode_cache_dir = "$STATE_DIR/cache"

[obs]
log_level = "info"

[media]
roots = ["$STATE_DIR/media"]

[database]
url = "sqlite://$STATE_DIR/db/pharos.db?mode=rwc"
TOML
fi

# Seed playwright user + fixture.
echo ">>> seeding playwright user + fixture"
if [ "$IS_LINUX" = "1" ]; then
  $DOCKER run --rm \
    -v "$STATE_DIR/db:/var/lib/pharos/db" \
    -v "$STATE_DIR/media:/var/lib/pharos/media" \
    -v "$STATE_DIR/cache:/var/lib/pharos/cache" \
    -v "$CONFIG_PATH:/etc/pharos/config.toml:ro" \
    pharos:latest \
    --config /etc/pharos/config.toml admin seed-playwright-user \
    || echo "    (seed may have already happened; continuing)"
else
  "$PHAROS_BIN" --config "$CONFIG_PATH" admin seed-playwright-user \
    || echo "    (seed may have already happened; continuing)"
fi

# Start pharos.
if [ "$IS_LINUX" = "1" ]; then
  echo ">>> starting pharos container"
  $DOCKER rm -f pharos-dev-stack >/dev/null 2>&1 || true
  $DOCKER run -d \
    --name pharos-dev-stack \
    --network=host \
    --restart=unless-stopped \
    -v "$STATE_DIR/db:/var/lib/pharos/db" \
    -v "$STATE_DIR/media:/var/lib/pharos/media" \
    -v "$STATE_DIR/cache:/var/lib/pharos/cache" \
    -v "$CONFIG_PATH:/etc/pharos/config.toml:ro" \
    pharos:latest \
    --config /etc/pharos/config.toml serve >/dev/null
else
  echo ">>> starting pharos (host process)"
  "$PHAROS_BIN" --config "$CONFIG_PATH" serve &
  PHAROS_PID=$!
fi

# Start jellyfin-web (always a container; static-only, host-OS-agnostic).
echo ">>> starting jellyfin-web container"
$DOCKER rm -f pharos-jellyfin-web >/dev/null 2>&1 || true
NGINX_PORT_FLAG=""
if [ "$IS_LINUX" = "1" ]; then
  NGINX_NET_FLAG="--network=host"
else
  # macOS / podman-machine etc. — host networking isn't transparent;
  # publish the port instead.
  NGINX_NET_FLAG=""
  NGINX_PORT_FLAG="-p 127.0.0.1:8097:8097"
fi
NGINX_CONF=$(cat <<'NGINX'
server {
  listen 8097 default_server;
  root /usr/share/jellyfin-web;
  index index.html;
  location / { try_files $uri $uri/ /index.html; }
}
NGINX
)
# shellcheck disable=SC2086  # we intentionally want word-splitting on
# $NGINX_NET_FLAG / $NGINX_PORT_FLAG so an empty value contributes
# nothing.
$DOCKER run -d \
  --name pharos-jellyfin-web \
  $NGINX_NET_FLAG \
  $NGINX_PORT_FLAG \
  --restart=unless-stopped \
  -v "$JELLYFIN_WEB_DIR:/usr/share/jellyfin-web:ro" \
  nginx:alpine \
  sh -c "echo '$NGINX_CONF' > /etc/nginx/conf.d/default.conf && exec nginx -g 'daemon off;'" \
  >/dev/null

echo
echo "    pharos       -> http://localhost:8096"
echo "    jellyfin-web -> http://localhost:8097"
echo "    seeded user  -> playwright / playwright-test-pw"
echo "    state dir    -> $STATE_DIR"
echo
echo "Ctrl-C to stop. Containers + host process clean up on exit."
echo

cleanup() {
  echo
  echo ">>> stopping containers"
  $DOCKER rm -f pharos-jellyfin-web >/dev/null 2>&1 || true
  if [ "$IS_LINUX" = "1" ]; then
    $DOCKER rm -f pharos-dev-stack >/dev/null 2>&1 || true
  else
    if [ -n "${PHAROS_PID:-}" ]; then
      kill "$PHAROS_PID" 2>/dev/null || true
    fi
  fi
}
trap cleanup INT TERM EXIT

if [ "$IS_LINUX" = "1" ]; then
  # Stream pharos container logs as the foreground process.
  $DOCKER logs -f pharos-dev-stack
else
  # pharos host process is already foregrounded via `wait`.
  wait "${PHAROS_PID}"
fi
