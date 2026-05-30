# Tiltfile — local k8s inner-loop for pharos.
#
# Targets a Tilt-managed local *kind* cluster (context `kind-pharos`) and
# loads the nix-built OCI images directly (no registry). Brings up pharos +
# the jellyfin-web UI via the Helm chart in charts/pharos, then mirrors the
# docker dev-stack: copies the CC test-media corpus into the pod, scans it,
# and seeds the playwright admin user.
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

# ── Populate media + scan + seed (mirrors scripts/dev-stack.sh).
local_resource(
    'seed-media',
    cmd='bash scripts/tilt-seed.sh',
    deps=['scripts/tilt-seed.sh'],
    resource_deps=['pharos'],
    labels=['pharos'],
)
