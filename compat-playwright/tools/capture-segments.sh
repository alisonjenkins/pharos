#!/usr/bin/env bash
# Capture the EXACT bytes a running pharos serves for a window of segments, in
# the layout `hlsjs-bytes-probe.cjs` replays.
#
# A player symptom is not attributable to the server until the server's own
# output has been replayed through a player. Grabbing the bytes by hand — one
# curl per segment, per rung — is how that gets skipped.
#
# Usage:
#   capture-segments.sh --base http://127.0.0.1:8096 --item <32-hex-id> \
#                       --key <api_key> --session <PlaySessionId> \
#                       --out ./capture [--segs 18-21] [--rungs h264cmaf,vp9]
#
# Against the deployment, point --base at a port-forward:
#   kubectl port-forward -n pharos <pod> 18080:8096
#
# The api_key and PlaySessionId of a live session appear in the server's
# `http.target` span field, so a wedged session can be captured as it wedges.
# Segment routes 410 with an unregistered PlaySessionId — reuse a real one.
set -euo pipefail

BASE=""; ITEM=""; KEY=""; SESSION=""; OUT="./capture"; SEGS="18-21"; RUNGS="h264cmaf,vp9"
while [ $# -gt 0 ]; do
  case "$1" in
    --base) BASE="$2"; shift 2;;
    --item) ITEM="$2"; shift 2;;
    --key) KEY="$2"; shift 2;;
    --session) SESSION="$2"; shift 2;;
    --out) OUT="$2"; shift 2;;
    --segs) SEGS="$2"; shift 2;;
    --rungs) RUNGS="$2"; shift 2;;
    *) echo "unknown arg: $1" >&2; exit 2;;
  esac
done
for v in BASE ITEM KEY SESSION; do
  [ -n "${!v}" ] || { echo "missing --${v,,}" >&2; exit 2; }
done

FIRST="${SEGS%-*}"; LAST="${SEGS#*-}"
QS="api_key=${KEY}&PlaySessionId=${SESSION}"

# The audio group lives under <rung>/audio/ with `aN.m4s` names; it is captured
# as its own `aud` rung so the probe can pair it with the video.
fetch() { # url dest
  local code
  code=$(curl -s -o "$2" -w '%{http_code}' "$1")
  printf '  %-28s %s %s bytes\n' "$(basename "$2")" "$code" "$(stat -c%s "$2" 2>/dev/null || echo 0)"
  [ "$code" = "200" ] || rm -f "$2"
}

for rung in ${RUNGS//,/ }; do
  echo "rung ${rung}:"
  mkdir -p "${OUT}/${rung}"
  fetch "${BASE}/videos/${ITEM}/${rung}/init.mp4?${QS}" "${OUT}/${rung}/init.mp4"
  for s in $(seq "$FIRST" "$LAST"); do
    fetch "${BASE}/videos/${ITEM}/${rung}/${s}.m4s?${QS}" "${OUT}/${rung}/seg${s}.m4s"
  done
done

echo "audio rendition:"
mkdir -p "${OUT}/aud"
fetch "${BASE}/videos/${ITEM}/vp9/audio/init.mp4?${QS}" "${OUT}/aud/init.mp4"
for s in $(seq "$FIRST" "$LAST"); do
  fetch "${BASE}/videos/${ITEM}/vp9/audio/a${s}.m4s?${QS}" "${OUT}/aud/seg${s}.m4s"
done

echo
echo "captured to ${OUT}; replay it with:"
echo "  just probe-bytes ${OUT} h264cmaf system-firefox"
