#!/usr/bin/env bash
# Reproducible local dev-stack: pharos + jellyfin-web, both as
# containers, on every host.
#
# pharos is built from `./Dockerfile` (multi-stage rust → debian) so
# the runtime is linux regardless of build host. Pinning lives in
# `rust-toolchain.toml` + `Cargo.lock`. jellyfin-web is nginx:alpine
# bind-mounting the pinned nixpkgs bundle.
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
# Persistent state: $XDG_DATA_HOME/pharos-dev-stack. CLEAN=1 wipes.

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

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

STATE_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/pharos-dev-stack"
if [ "${CLEAN:-0}" = "1" ]; then
  echo ">>> wiping $STATE_DIR"
  rm -rf "$STATE_DIR"
fi
mkdir -p "$STATE_DIR/db" "$STATE_DIR/media" "$STATE_DIR/cache"

# Resolve jellyfin-web from the pinned nixpkgs.
echo ">>> resolving jellyfin-web bundle (nixpkgs#jellyfin-web)"
JELLYFIN_WEB_OUT=$(nix build --no-link --print-out-paths "nixpkgs#jellyfin-web")
JELLYFIN_WEB_DIR="$JELLYFIN_WEB_OUT/share/jellyfin-web"
if [ ! -d "$JELLYFIN_WEB_DIR" ]; then
  echo "error: jellyfin-web bundle not found at $JELLYFIN_WEB_DIR" >&2
  exit 1
fi
echo "    jellyfin-web -> $JELLYFIN_WEB_DIR"

# Build pharos image from the local Dockerfile. BuildKit cache mounts
# inside the Dockerfile reuse the cargo registry + target dir across
# rebuilds.
echo ">>> building pharos image (docker build)"
DOCKER_BUILDKIT=1 $DOCKER build -t pharos:dev -f Dockerfile .

# Write a config.toml the container reads from /etc/pharos.
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

# Linux can use --network=host so localhost works inside both
# containers + on the host. Other OSes get -p publish-port; the host
# reaches the services at localhost:<port> but containers can't see
# each other via localhost. jellyfin-web is static + connects from the
# browser, so it doesn't need to call pharos directly anyway.
UNAME=$(uname -s)
if [ "$UNAME" = "Linux" ]; then
  PHAROS_NET=(--network=host)
  NGINX_NET=(--network=host)
else
  PHAROS_NET=(-p 127.0.0.1:8096:8096)
  NGINX_NET=(-p 127.0.0.1:8097:8097)
fi

# Seed the playwright user + fixture via a one-shot pharos container.
echo ">>> seeding playwright user + fixture"
$DOCKER run --rm \
  -v "$STATE_DIR/db:/var/lib/pharos/db" \
  -v "$STATE_DIR/media:/var/lib/pharos/media" \
  -v "$STATE_DIR/cache:/var/lib/pharos/cache" \
  -v "$CONFIG_PATH:/etc/pharos/config.toml:ro" \
  pharos:dev \
  --config /etc/pharos/config.toml admin seed-playwright-user \
  || echo "    (seed may have already happened; continuing)"

cleanup() {
  echo
  echo ">>> stopping dev-stack"
  $DOCKER rm -f pharos-jellyfin-web >/dev/null 2>&1 || true
  $DOCKER rm -f pharos-dev-stack    >/dev/null 2>&1 || true
}
trap cleanup INT TERM EXIT

# Start pharos.
echo ">>> starting pharos container"
$DOCKER rm -f pharos-dev-stack >/dev/null 2>&1 || true
$DOCKER run -d \
  --name pharos-dev-stack \
  "${PHAROS_NET[@]}" \
  --restart=unless-stopped \
  -v "$STATE_DIR/db:/var/lib/pharos/db" \
  -v "$STATE_DIR/media:/var/lib/pharos/media" \
  -v "$STATE_DIR/cache:/var/lib/pharos/cache" \
  -v "$CONFIG_PATH:/etc/pharos/config.toml:ro" \
  pharos:dev \
  --config /etc/pharos/config.toml serve >/dev/null

# Start jellyfin-web.
echo ">>> starting jellyfin-web container"
$DOCKER rm -f pharos-jellyfin-web >/dev/null 2>&1 || true
NGINX_CONF=$(cat <<'NGINX'
server {
  listen 8097 default_server;
  root /usr/share/jellyfin-web;
  index index.html;
  location / { try_files $uri $uri/ /index.html; }
}
NGINX
)
$DOCKER run -d \
  --name pharos-jellyfin-web \
  "${NGINX_NET[@]}" \
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
echo "Ctrl-C to stop. Containers tear down via the trap."
echo

# Stream pharos logs as the foreground process.
$DOCKER logs -f pharos-dev-stack
