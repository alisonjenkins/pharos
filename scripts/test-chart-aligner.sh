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
expect "shadow off renders no shadow" absent "aligner-shadow" --
expect "shadow on renders its deployment" present "name: p-pharos-aligner-shadow" -- \
  --set aligner.shadow.enabled=true --set aligner.shadow.image.tag=s
expect "shadow on runs the Rust image" present "pharos-aligner-rs:s" -- \
  --set aligner.shadow.enabled=true --set aligner.shadow.image.tag=s

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
