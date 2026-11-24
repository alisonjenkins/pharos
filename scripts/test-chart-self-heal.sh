#!/usr/bin/env bash
# Renders the chart's selfHeal CronJob and runs its script against a fake
# kubectl, checking which Deployments it restarts and which pods it stamps. Run as part of `just helm-test`.
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

rules="$(yq '[.rules[] | (.apiGroups[0]) + "/" + (.resources | join(",")) + ":" + (.verbs | join(","))] | join(" ")' <<<"$role")"
if [ "$rules" = "/pods:get,list,patch apps/deployments:get,list,patch" ]; then
  ok "the Role can only read and patch pods and deployments, and delete nothing"
else
  fail "the Role's rules are \"$rules\""
fi

container='.spec.jobTemplate.spec.template.spec.containers[0]'
selector="$(yq "$container.env[] | select(.name == \"SELECTOR\") | .value" <<<"$cronjob")"
if [ "$selector" = "app.kubernetes.io/instance=p,pharos.io/self-heal=true" ]; then
  ok "the reaper looks only at this release's labelled Deployments"
else
  fail "the reaper's selector is \"$selector\""
fi

# The aligner Deployments carry the label the reaper selects on.
aligner="$(helm template p "$chart" --show-only templates/aligner.yaml \
  --set aligner.enabled=true --set aligner.image.tag=t \
  --set aligner.shadow.enabled=true --set aligner.shadow.image.tag=s)"
labelled="$(yq 'select(.kind == "Deployment") | .metadata.labels["pharos.io/self-heal"]' <<<"$aligner" | grep -c true || true)"
if [ "$labelled" -eq 2 ]; then
  ok "primary and shadow aligner Deployments carry the self-heal label"
else
  fail "the aligner Deployments do not both carry the self-heal label"
fi

# So does the server Deployment, whose /readyz fails on a broken share too
# (B1531).
server="$(helm template p "$chart" --show-only templates/deployment.yaml)"
if [ "$(yq '.metadata.labels["pharos.io/self-heal"]' <<<"$server")" = "true" ]; then
  ok "the server Deployment carries the self-heal label"
else
  fail "the server Deployment does not carry the self-heal label"
fi

# ADR-0156: the server asks for its own restart, through a service account
# that may get and patch only its own Deployment.
spec='.spec.template.spec'
if [ "$(yq "$spec.serviceAccountName" <<<"$server")" = "p-pharos-self-restart" ]; then
  ok "the server pod runs as its self-restart service account"
else
  fail "the server pod's service account is \"$(yq "$spec.serviceAccountName" <<<"$server")\""
fi
env_of() { yq "$spec.containers[0].env[] | select(.name == \"$1\") | (.value // .valueFrom.fieldRef.fieldPath)" <<<"$2"; }
if [ "$(env_of PHAROS_SELF_RESTART_DEPLOYMENT "$server")" = "p-pharos" ] &&
  [ "$(env_of POD_NAMESPACE "$server")" = "metadata.namespace" ]; then
  ok "the server is told its Deployment and namespace"
else
  fail "the server's self-restart env is missing"
fi
restart_role="$(helm template p "$chart" --show-only templates/self-restart.yaml | yq 'select(.kind == "Role")')"
restart_rules="$(yq '[.rules[] | (.apiGroups | join(",")) + "/" + (.resources | join(",")) + "/" + (.resourceNames | join(",")) + ":" + (.verbs | join(","))] | join(" ")' <<<"$restart_role")"
if [ "$restart_rules" = "apps/deployments/p-pharos:get,patch" ]; then
  ok "the self-restart Role may only get and patch the server's own Deployment"
else
  fail "the self-restart Role's rules are \"$restart_rules\""
fi
for off in selfHeal.enabled=false selfHeal.serverSelfRestart=false; do
  plain="$(helm template p "$chart" --show-only templates/deployment.yaml --set "$off")"
  if [ "$(yq "$spec.serviceAccountName" <<<"$plain")" = "null" ] &&
    [ -z "$(env_of PHAROS_SELF_RESTART_DEPLOYMENT "$plain")" ] &&
    [ -z "$(helm template p "$chart" --show-only templates/self-restart.yaml --set "$off" 2>/dev/null || true)" ]; then
    ok "with $off the server gets no self-restart"
  else
    fail "with $off the server still gets a self-restart"
  fi
done

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
} >"$work/pods-app_aligner.json"

# A Deployment this reaper restarted a minute ago: left alone, stale pod
# or not, until its new pod has had the whole limit.
{
  echo '{"items":['
  pod recent-stale Running true False 900 400
  echo ']}'
} >"$work/pods-app_recent.json"
{
  echo '{"items":['
  pod calm-ready Running true True 3600
  echo ']}'
} >"$work/pods-app_calm.json"
deployment() { # name seconds-since-restart|-
  local annotations='{}'
  [ "$2" != - ] && annotations="{\"kubectl.kubernetes.io/restartedAt\":\"$(ago "$2")\"}"
  printf '{"metadata":{"name":"%s"},"spec":{"selector":{"matchLabels":{"app":"%s"}},"template":{"metadata":{"annotations":%s}}}}' \
    "$1" "$1" "$annotations"
}
{
  echo '{"items":['
  deployment aligner -; echo ,
  deployment recent 60; echo ,
  deployment calm 3600
  echo ']}'
} >"$work/deployments.json"

mkdir "$work/bin"
cat >"$work/bin/kubectl" <<KUBECTL
#!/usr/bin/env bash
selector=""
args=("\$@")
for ((i = 0; i < \${#args[@]}; i++)); do
  [ "\${args[i]}" = -l ] && selector="\${args[i + 1]}"
done
case "\$1 \$2" in
  "get deployments") cat "$work/deployments.json" ;;
  "get pods") cat "$work/pods-\${selector//[^a-z]/_}.json" ;;
  "patch deployment") echo "\$5 \$*" >>"$work/restarted" ;;
  "annotate pod") echo "\$5" >>"$work/stamped" ;;
  *) exit 2 ;;
esac
KUBECTL
chmod +x "$work/bin/kubectl"

touch "$work/restarted" "$work/stamped"
if PATH="$work/bin:$PATH" NAMESPACE=pharos SELECTOR=x NOT_READY_SECONDS=300 \
  bash "$work/script.sh" >"$work/out"; then
  ok "the reaper script runs"
else
  fail "the reaper script failed: $(cat "$work/out")"
fi
if [ "$(cut -d' ' -f1 "$work/restarted")" = "aligner" ] &&
  grep -q 'kubectl.kubernetes.io/restartedAt' "$work/restarted"; then
  ok "only the Deployment whose pod is NotReady past the limit is rolled out, once"
else
  fail "restarted: $(tr '\n' ';' <"$work/restarted")"
fi
if [ "$(sort "$work/stamped" | tr '\n' ' ')" = "first-seen relapsed restarted " ]; then
  ok "a pod first seen NotReady, NotReady again after recovering, or restarted, is stamped"
else
  fail "stamped: $(tr '\n' ';' <"$work/stamped")"
fi
if grep -q "self-heal: restarting deployment=aligner pod=stale not_ready_s=" "$work/out"; then
  ok "the rollout is logged with the Deployment, the pod and how long it was NotReady"
else
  fail "no rollout log line: $(cat "$work/out")"
fi
if grep -q "self-heal: waiting deployment=recent" "$work/out"; then
  ok "a Deployment restarted within the limit is left alone"
else
  fail "no waiting line for the recently restarted Deployment: $(cat "$work/out")"
fi

exit "$failed"
