# Tiltfile — local k8s inner-loop for pharos.
#
# Targets a Tilt-managed local *kind* cluster (context `kind-pharos`) wired to
# a local OCI registry by ctlptl (`ctlptl-cluster.yaml`, via `just kind-up`).
# nix builds each image, Tilt pushes it to that registry, and the kind node
# pulls it from there — Tilt auto-detects the registry from the cluster's
# `local-registry-hosting` ConfigMap, so no `kind load`, no docker.io.
#
# Brings up pharos + TWO web UIs via the Helm chart (charts/pharos):
#   pharos-ui    — pharos's own Dioxus UI (http://127.0.0.1:8098/ui/)
#   jellyfin-web — the upstream Jellyfin client (http://127.0.0.1:8097/)
# Each UI is an angie image serving its static bundle + reverse-proxying the
# REST API to the pharos service, so the browser is same-origin (no manual
# "add server" step).
#
# The library is populated in-pod: a `mediaSeed` initContainer copies the nix
# CC test-media corpus into the media volume, then `serve` starts and its
# library poll tier indexes it (~30s after boot — the dev poll interval). Tilt
# then seeds the playwright admin user.
#
# (A one-shot `scan` initContainer was tried first but its sqlite pool can't
# establish under kind's containerd runtime; the running server's warm pool
# scans fine. See docs/kubernetes.md "kind dev caveat".)
#
#   just tilt-up      # create the kind cluster + registry (if absent) + `tilt up`
#   just tilt-down    # `tilt down` (delete=1 also removes cluster + registry)
#
# Safety: refuse to run against anything but the local kind context.
allow_k8s_contexts('kind-pharos')

update_settings(k8s_upsert_timeout_secs=180)

# ── Images: build via nix, load into docker, push to the local registry.
#    Tilt sets $EXPECTED_REF to a registry ref (localhost:<port>/<name>:<hash>);
#    the kind node pulls from that registry.
def nix_oci(image_name, flake_attr):
    custom_build(
        image_name,
        # nix build → docker load (→ <name>:latest) → tag + push the ref Tilt
        # expects ($EXPECTED_REF, in the ctlptl-managed local registry).
        'set -euo pipefail; ' +
        'out=$(nix build ".#%s" --no-link --print-out-paths); ' % flake_attr +
        'docker load -i "$out"; ' +
        'docker tag %s:latest "$EXPECTED_REF"; ' % image_name +
        'docker push "$EXPECTED_REF"',
        deps=['flake.nix', 'Cargo.toml', 'Cargo.lock', 'Cargo.nix', 'crates'],
        skips_local_docker=False,
    )

nix_oci('pharos', 'oci')
nix_oci('pharos-ui', 'pharosUiOci')
nix_oci('pharos-jellyfin-web', 'jellyfinWebOci')
nix_oci('pharos-test-media', 'testMediaOci')

# ── Deploy the chart (dev values). Tilt's helm() renders with --namespace but
#    doesn't create it (unlike `helm install --create-namespace`), so declare
#    the namespace ourselves; Tilt applies Namespace objects before the rest.
k8s_yaml(blob('''
apiVersion: v1
kind: Namespace
metadata:
  name: pharos
'''))
k8s_yaml(helm(
    'charts/pharos',
    name='pharos',
    namespace='pharos',
    values=['charts/pharos/values-dev.yaml'],
))

k8s_resource(
    'pharos',
    port_forwards=['8096:8096'],
    labels=['pharos'],
)
# pharos's own Dioxus UI (served under /ui/, proxies the API to pharos).
k8s_resource(
    'pharos-pharos-ui',
    new_name='pharos-ui',
    port_forwards=['8098:8098'],
    labels=['pharos'],
    resource_deps=['pharos'],
)
# Upstream jellyfin-web client (served under /web/, proxies the API to pharos).
k8s_resource(
    'pharos-jellyfin-web',
    new_name='jellyfin-web',
    port_forwards=['8097:8097'],
    labels=['pharos'],
    resource_deps=['pharos'],
)

# ── Seed the playwright admin user once pharos is up. (Media is copied in by
#    the mediaSeed initContainer; the server's poll tier indexes it ~30s after
#    boot — the seed script waits for the library to populate and reports it.)
local_resource(
    'seed-user',
    cmd='bash scripts/tilt-seed.sh',
    deps=['scripts/tilt-seed.sh'],
    resource_deps=['pharos'],
    labels=['pharos'],
)
