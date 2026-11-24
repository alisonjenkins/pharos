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

if [ -n "$(helm template p "$chart" --show-only templates/self-heal.yaml 2>/dev/null || true)" ]; then
  fail "selfHeal off renders nothing"
else
  ok "selfHeal off renders nothing"
fi

rendered="$(helm template p "$chart" --namespace pharos --show-only templates/self-heal.yaml \
  --set selfHeal.enabled=true --set selfHeal.notReadyMinutes=5)"
cronjob="$(yq 'select(.kind == "CronJob")' <<<"$rendered")"
role="$(yq 'select(.kind == "Role")' <<<"$rendered")"

if [ "$(yq '.rules[0].verbs | join(",")' <<<"$role")" = "get,list,delete" ]; then
  ok "the Role can only get, list and delete pods"
else
  fail "the Role's verbs are not get,list,delete"
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

yq "$container.args[0]" <<<"$cronjob" >"$work/script.sh"

ago() { date -u -d "@$(($(date +%s) - $1))" +%Y-%m-%dT%H:%M:%SZ; }
pod() { # name phase started ready seconds-since-ready-changed [terminating]
  local deleting=null
  [ "${6:-}" = terminating ] && deleting="\"$(ago 1)\""
  printf '{"metadata":{"name":"%s","deletionTimestamp":%s},"status":{"phase":"%s","containerStatuses":[{"started":%s}],"conditions":[{"type":"Ready","status":"%s","lastTransitionTime":"%s"}]}}' \
    "$1" "$deleting" "$2" "$3" "$4" "$(ago "$5")"
}
{
  echo '{"items":['
  pod ready Running true True 3600; echo ,
  pod stale Running true False 600; echo ,
  pod starting Running false False 600; echo ,
  pod blip Running true False 60; echo ,
  pod leaving Running true False 600 terminating; echo ,
  pod pending Pending true False 600
  echo ']}'
} >"$work/pods.json"

mkdir "$work/bin"
cat >"$work/bin/kubectl" <<KUBECTL
#!/usr/bin/env bash
case "\$1" in
  get) cat "$work/pods.json" ;;
  delete) echo "\$*" >>"$work/deleted" ;;
  *) exit 2 ;;
esac
KUBECTL
chmod +x "$work/bin/kubectl"

touch "$work/deleted"
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
if grep -q "self-heal: deleting pod=stale not_ready_s=" "$work/out"; then
  ok "the deletion is logged with the pod and how long it was NotReady"
else
  fail "no deletion log line: $(cat "$work/out")"
fi

exit "$failed"
