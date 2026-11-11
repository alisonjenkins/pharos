#!/usr/bin/env bash
# Renders the chart's aligner with the GPU, kernel cache and shadow on and off
# and checks what each produces (spec 039, V1265). Run as `just helm-test`.
set -euo pipefail

chart="$(dirname "$0")/../charts/pharos"
failed=0

render() {
  helm template p "$chart" --show-only templates/aligner.yaml \
    --set aligner.enabled=true --set aligner.image.tag=t "$@"
}

# expect <name> <present|absent> <fixed string> -- <helm args>
expect() {
  local name="$1" want="$2" needle="$3"
  shift 4
  local out
  out="$(render "$@")"
  if grep -qF -- "$needle" <<<"$out"; then got=present; else got=absent; fi
  if [ "$got" = "$want" ]; then
    echo "ok   $name"
  else
    echo "FAIL $name: wanted \"$needle\" $want, it is $got" >&2
    failed=1
  fi
}

expect "gpu off asks for no render node" absent "devic.es/dri" --
expect "gpu off adds no group" absent "supplementalGroups" --
expect "gpu on asks for the render node" present "devic.es/dri: 1" -- \
  --set aligner.gpu.enabled=true
expect "gpu on adds the render group" present "- 303" -- \
  --set aligner.gpu.enabled=true --set "aligner.gpu.supplementalGroups={303}"
expect "device is passed through" present 'value: "vulkan"' -- \
  --set aligner.device=vulkan
expect "no device leaves the variable unset" absent "PHAROS_ALIGNER_DEVICE" --
expect "cache off has no claim" absent "PersistentVolumeClaim" --
expect "cache on mounts its claim" present "claimName: p-pharos-aligner-cache" -- \
  --set aligner.cache.enabled=true
expect "cache on mounts at R14's path" present "mountPath: /var/cache/pharos-aligner" -- \
  --set aligner.cache.enabled=true
expect "neither gpu nor cache keeps the default strategy" absent "strategy:" --
expect "gpu on recreates" present "type: Recreate" -- \
  --set aligner.gpu.enabled=true
expect "cache on recreates" present "type: Recreate" -- \
  --set aligner.cache.enabled=true
expect "a shadow on the gpu recreates" present "type: Recreate" -- \
  --set aligner.shadow.enabled=true --set aligner.shadow.image.tag=s --set aligner.shadow.gpu.enabled=true
expect "a shadow without gpu or cache keeps the default" absent "strategy:" -- \
  --set aligner.shadow.enabled=true --set aligner.shadow.image.tag=s
expect "shadow off renders no shadow" absent "aligner-shadow" --
expect "shadow on renders its deployment" present "name: p-pharos-aligner-shadow" -- \
  --set aligner.shadow.enabled=true --set aligner.shadow.image.tag=s
expect "shadow on runs the Rust image" present "pharos-aligner-rs:s" -- \
  --set aligner.shadow.enabled=true --set aligner.shadow.image.tag=s
expect "the aligner waits up to 10 minutes to start" present "failureThreshold: 60" --
# Both Deployments carry it: the primary's and the shadow's.
startup="$(render --set aligner.shadow.enabled=true --set aligner.shadow.image.tag=s)"
if [ "$(grep -c "startupProbe:" <<<"$startup")" -eq 2 ]; then
  echo "ok   the shadow has a startup probe too"
else
  echo "FAIL the shadow has no startup probe" >&2
  failed=1
fi

# The server is pointed at the shadow's Service only when there is one.
server() {
  helm template p "$chart" --show-only templates/deployment.yaml \
    --set aligner.enabled=true --set aligner.image.tag=t "$@"
}
# Rendered first: a failed render is empty and would pass the absent check.
shadow_url='value: "http://p-pharos-aligner-shadow:'
if ! with_shadow="$(server --set aligner.shadow.enabled=true --set aligner.shadow.image.tag=s)"; then
  echo "FAIL the server's render with the shadow on failed" >&2
  failed=1
elif ! without_shadow="$(server)"; then
  echo "FAIL the server's default render failed" >&2
  failed=1
elif grep -qF "$shadow_url" <<<"$with_shadow" \
  && ! grep -qF "PHAROS_LYRICS_ALIGNER_SHADOW_URL" <<<"$without_shadow"; then
  echo "ok   the server asks the shadow only when it is enabled"
else
  echo "FAIL the server's shadow URL does not follow aligner.shadow.enabled" >&2
  failed=1
fi

# The primary Service must not select the shadow's pods, or songs would
# reach the shadow as answers.
shadow="$(render --set aligner.shadow.enabled=true --set aligner.shadow.image.tag=s)"
if grep -A3 "^  selector:" <<<"$shadow" | grep -qx "    app.kubernetes.io/name: pharos-aligner-shadow" \
  && [ "$(grep -c "app.kubernetes.io/name: pharos-aligner$" <<<"$shadow")" -eq 3 ]; then
  echo "ok   shadow and primary select only their own pods"
else
  echo "FAIL shadow and primary selectors overlap" >&2
  failed=1
fi

exit "$failed"
