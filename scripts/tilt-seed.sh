#!/usr/bin/env bash
# Tilt dev helper: populate the pharos pod's (emptyDir) media volume with the
# nix CC test-media corpus, scan it into the store, and seed the playwright
# admin user. Mirrors the docker dev-stack flow (scripts/dev-stack.sh) for the
# kind/Tilt deployment. Idempotent.
set -euo pipefail

NS=pharos
SEL=app.kubernetes.io/name=pharos,app.kubernetes.io/instance=pharos
MEDIA=/var/lib/pharos/media
CFG=/etc/pharos/config.toml

echo "==> waiting for the pharos pod to be Ready"
kubectl -n "$NS" rollout status deploy/pharos --timeout=180s

POD=$(kubectl -n "$NS" get pod -l "$SEL" -o jsonpath='{.items[0].metadata.name}')
echo "==> pod: $POD"

echo "==> building CC test-media corpus (nix)"
MEDIA_TREE=$(nix build ".#testMediaTree" --no-link --print-out-paths)
echo "    $MEDIA_TREE"

# Already populated? (idempotent re-runs)
if [ "$(kubectl -n "$NS" exec "$POD" -c pharos -- sh -c "ls -1 $MEDIA 2>/dev/null | wc -l" || echo 0)" -ge 4 ]; then
  echo "==> media already populated, skipping copy"
else
  echo "==> copying fixtures into $MEDIA"
  # kubectl cp the directory contents (resolve symlinks; the store tree uses them)
  tmp=$(mktemp -d)
  cp -rL "$MEDIA_TREE"/. "$tmp"/
  kubectl -n "$NS" cp "$tmp/." "$POD:$MEDIA" -c pharos
  rm -rf "$tmp"
fi

echo "==> scanning library"
kubectl -n "$NS" exec "$POD" -c pharos -- pharos --config "$CFG" scan

echo "==> seeding playwright admin user (idempotent)"
kubectl -n "$NS" exec "$POD" -c pharos -- pharos --config "$CFG" admin create-playwright-user || true

echo "==> done. API: http://127.0.0.1:8096  UI: http://127.0.0.1:8097"
