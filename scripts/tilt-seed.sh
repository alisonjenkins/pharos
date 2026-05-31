#!/usr/bin/env bash
# Tilt dev helper: seed the playwright admin user once pharos is up, then wait
# for the library to populate and report the item count. Media population
# happens in-pod via the chart's mediaSeed initContainer (the distroless image
# has no `tar`, so `kubectl cp` is not usable); indexing is done by the running
# server's library poll tier (~30s after boot — the dev poll interval). A cold
# one-shot `scan` initContainer was tried first but can't establish its sqlite
# pool under kind's containerd runtime. Idempotent.
set -uo pipefail

NS=pharos
CFG=/etc/pharos/config.toml
USER=playwright
PW=playwright-test-pw

echo "==> waiting for the pharos pod to be Ready"
kubectl -n "$NS" rollout status deploy/pharos --timeout=180s

echo "==> seeding playwright admin user (idempotent)"
kubectl -n "$NS" exec deploy/pharos -c pharos -- \
  pharos --config "$CFG" admin create-playwright-user || true

# Authenticate to read /Items. The port-forward is owned by Tilt's `pharos`
# resource (8096); this script is gated behind it via resource_deps.
auth() {
  curl -s -X POST http://127.0.0.1:8096/Users/AuthenticateByName \
    -H 'Content-Type: application/json' \
    -H 'X-Emby-Authorization: MediaBrowser Client="tilt", Device="seed", DeviceId="tilt-seed", Version="0"' \
    -d "{\"Username\":\"$USER\",\"Pw\":\"$PW\"}" 2>/dev/null \
    | python3 -c 'import sys,json;print(json.load(sys.stdin).get("AccessToken",""))' 2>/dev/null
}

echo "==> waiting for the library poll tier to index the seeded media"
tok=""
count=0
for _ in $(seq 1 12); do
  [ -z "$tok" ] && tok=$(auth)
  if [ -n "$tok" ]; then
    count=$(curl -s "http://127.0.0.1:8096/Items?Recursive=true&Limit=0" \
      -H "X-Emby-Token: $tok" 2>/dev/null \
      | python3 -c 'import sys,json;print(json.load(sys.stdin).get("TotalRecordCount",0))' 2>/dev/null)
    [ "${count:-0}" -gt 0 ] 2>/dev/null && break
  fi
  sleep 6
done

if [ "${count:-0}" -gt 0 ] 2>/dev/null; then
  echo "==> library populated: $count item(s)"
else
  echo "==> WARNING: library still empty after ~72s; check 'kubectl -n $NS logs deploy/pharos'"
fi
echo "==> done. API: http://127.0.0.1:8096  UI: http://127.0.0.1:8097"
