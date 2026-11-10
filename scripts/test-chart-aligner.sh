#!/usr/bin/env bash
# Renders the chart's aligner with the GPU, and kernel cache on and off
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

exit "$failed"
