# Tiltfile — local k8s inner-loop for pharos.
#
# Targets a Tilt-managed local *kind* cluster (context `kind-pharos`, docker
# driver) and loads the nix-built OCI images directly (no registry). Brings
# up pharos + the jellyfin-web UI via the Helm chart (charts/pharos). The
# library is populated in-pod: a `mediaSeed` initContainer copies the nix CC
# test-media corpus into the media volume, then `serve` starts and its
# library poll tier indexes the corpus (~30s after boot — the dev poll
# interval). Tilt then seeds the playwright admin user.
#
# (A one-shot `scan` initContainer was tried first but its sqlite pool can't
# establish under kind's containerd runtime; the running server's warm pool
# scans fine. See docs/kubernetes.md "kind dev caveat".)
#
#   just tilt-up      # create the kind cluster (if absent) + `tilt up`
#   just tilt-down    # `tilt down` (+ optionally delete the cluster)
#
# Safety: refuse to run against anything but the local kind context.
allow_k8s_contexts('kind-pharos')

update_settings(k8s_upsert_timeout_secs=180)

# ── Images: build via nix, load into docker, hand Tilt a tagged ref.
#    Tilt auto-loads into kind (no registry needed).
def nix_oci(image_name, flake_attr):
    custom_build(
        image_name,
        # nix build → docker load → retag to the ref Tilt expects ($EXPECTED_REF).
        'set -euo pipefail; ' +
        'out=$(nix build ".#%s" --no-link --print-out-paths); ' % flake_attr +
        'docker load -i "$out"; ' +
        'docker tag %s:latest "$EXPECTED_REF"' % image_name,
        deps=['flake.nix', 'Cargo.toml', 'Cargo.lock', 'Cargo.nix', 'crates'],
        skips_local_docker=False,
        disable_push=True,
        tag='dev',
    )

nix_oci('pharos', 'oci')
nix_oci('pharos-jellyfin-web', 'jellyfinWebOci')
nix_oci('pharos-test-media', 'testMediaOci')

# ── Deploy the chart (dev values).
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
k8s_resource(
    'pharos-ui',
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
