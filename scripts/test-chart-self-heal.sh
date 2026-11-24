#!/usr/bin/env bash
# Renders the chart's selfHeal CronJob and runs its script against a fake
# kubectl, checking which pods it deletes. Run as part of `just helm-test`.
set -euo pipefail

chart="$(dirname "$0")/../charts/pharos"
failed=0
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

ok() { echo "ok   $1"; }
fail() { echo "FAIL $1" >&2; failed=1; }

if [ -n "$(helm template p "$chart" --show-only templates/self-heal.yaml \
  --set selfHeal.enabled=false 2>/dev/null || true)" ]; then
  fail "selfHeal off renders nothing"
else
  ok "selfHeal off renders nothing"
fi

# On by default: a plain install gets the reaper.
rendered="$(helm template p "$chart" --namespace pharos --show-only templates/self-heal.yaml)"
cronjob="$(yq 'select(.kind == "CronJob")' <<<"$rendered")"
role="$(yq 'select(.kind == "Role")' <<<"$rendered")"

if [ "$(yq '.rules[0].verbs | join(",")' <<<"$role")" = "get,list,patch,delete" ]; then
  ok "the Role can only get, list, patch and delete pods"
else
  fail "the Role's verbs are not get,list,patch,delete"
fi

container='.spec.jobTemplate.spec.template.spec.containers[0]'
selector="$(yq "$container.env[] | select(.name == \"SELECTOR\") | .value" <<<"$cronjob")"
if [ "$selector" = "app.kubernetes.io/instance=p,pharos.io/self-heal=true" ]; then
  ok "the reaper looks only at this release's labelled pods"
else
  fail "the reaper's selector is \"$selector\""
fi

# The aligner pods carry the label the reaper selects on.
aligner="$(helm template p "$chart" --show-only templates/aligner.yaml \
  --set aligner.enabled=true --set aligner.image.tag=t \
  --set aligner.shadow.enabled=true --set aligner.shadow.image.tag=s)"
if [ "$(grep -c 'pharos.io/self-heal: "true"' <<<"$aligner")" -eq 2 ]; then
  ok "primary and shadow aligner pods carry the self-heal label"
else
  fail "the aligner pods do not both carry the self-heal label"
fi

# So does the server pod, whose /readyz fails on a broken share too (B1531).
server="$(helm template p "$chart" --show-only templates/deployment.yaml)"
if [ "$(yq '.spec.template.metadata.labels["pharos.io/self-heal"]' <<<"$server")" = "true" ]; then
  ok "the server pod carries the self-heal label"
else
  fail "the server pod does not carry the self-heal label"
fi

yq "$container.args[0]" <<<"$cronjob" >"$work/script.sh"

ago() { date -u -d "@$(($(date +%s) - $1))" +%Y-%m-%dT%H:%M:%SZ; }
pod() { # name phase started ready seconds-since-ready-changed [seconds-since-stamp|-] [terminating|replaced]
  # A stamp names container aaaaaaaaaaaa; the pod's container is that one
  # unless the 7th argument says it was replaced. restartCount stays 0 either
  # way, as after a node restart that lost kubelet state.
  local deleting=null annotations='{}' id=aaaaaaaaaaaa
  case "${7:-}" in
    terminating) deleting="\"$(ago 1)\"" ;;
    replaced) id=bbbbbbbbbbbb ;;
  esac
  [ -n "${6:-}" ] && [ "$6" != - ] &&
    annotations="{\"pharos.io/not-ready-since\":\"$(($(date +%s) - $6))/aaaaaaaaaaaa\"}"
  printf '{"metadata":{"name":"%s","deletionTimestamp":%s,"annotations":%s},"status":{"phase":"%s","containerStatuses":[{"started":%s,"restartCount":0,"containerID":"containerd://%s0123456789"}],"conditions":[{"type":"Ready","status":"%s","lastTransitionTime":"%s"}]}}' \
    "$1" "$deleting" "$annotations" "$2" "$3" "$id" "$4" "$(ago "$5")"
}
{
  echo '{"items":['
  pod ready Running true True 3600; echo ,
  # Seen NotReady by an earlier run 400 s ago, and still NotReady.
  pod stale Running true False 900 400; echo ,
  # NotReady since it was made (startup took 10 min) and not yet seen
  # started: stamped, not deleted.
  pod first-seen Running true False 600; echo ,
  # Stamped 600 s ago, then Ready, then NotReady again 30 s ago: restamped.
  pod relapsed Running true False 30 600; echo ,
  # Stamped 400 s ago, then its container was replaced (Ready stayed False,
  # restartCount unchanged): its startup ran again, so the count starts again.
  pod restarted Running true False 900 400 replaced; echo ,
  pod starting Running false False 600; echo ,
  pod blip Running true False 900 60; echo ,
  pod leaving Running true False 900 400 terminating; echo ,
  pod pending Pending true False 900 400; echo ,
  # A node that stopped reporting: Unknown is not a pod fault.
  pod unknown Running true Unknown 900 400
  echo ']}'
} >"$work/pods.json"

mkdir "$work/bin"
cat >"$work/bin/kubectl" <<KUBECTL
#!/usr/bin/env bash
case "\$1" in
  get) cat "$work/pods.json" ;;
  delete) echo "\$*" >>"$work/deleted" ;;
  annotate) echo "\${5}" >>"$work/stamped" ;;
  *) exit 2 ;;
esac
KUBECTL
chmod +x "$work/bin/kubectl"

touch "$work/deleted" "$work/stamped"
if PATH="$work/bin:$PATH" NAMESPACE=pharos SELECTOR=x NOT_READY_SECONDS=300 \
  bash "$work/script.sh" >"$work/out"; then
  ok "the reaper script runs"
else
  fail "the reaper script failed: $(cat "$work/out")"
fi
if [ "$(cat "$work/deleted")" = "delete pod -n pharos stale --wait=false" ]; then
  ok "only the started pod NotReady past the limit is deleted"
else
  fail "deleted: $(tr '\n' ';' <"$work/deleted")"
fi
if [ "$(sort "$work/stamped" | tr '\n' ' ')" = "first-seen relapsed restarted " ]; then
  ok "a pod first seen NotReady, NotReady again after recovering, or restarted, is stamped"
else
  fail "stamped: $(tr '\n' ';' <"$work/stamped")"
fi
if grep -q "self-heal: deleting pod=stale not_ready_s=" "$work/out"; then
  ok "the deletion is logged with the pod and how long it was NotReady"
else
  fail "no deletion log line: $(cat "$work/out")"
fi

exit "$failed"
