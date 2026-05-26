set shell := ["bash", "-cu"]

# Run the full nextest suite via the nix devShell.
test:
    nix develop --command cargo nextest run --workspace

# Clippy under workspace lints (denies warnings).
lint:
    nix develop --command cargo clippy --workspace --all-targets -- -D warnings

# Supply-chain checks (T45). Runs cargo-audit (RustSec advisories) and
# cargo-deny (licenses + bans + sources) under the policies in
# deny.toml.
audit:
    nix develop --command bash -c 'cargo audit && cargo deny check'

# Boot pharos + jellyfin-web as containers for manual testing. Uses
# `nix build .#oci` (pharos) + nginx:alpine + the pinned nixpkgs
# jellyfin-web bundle bind-mounted in. Requires docker or podman on
# the host. See scripts/dev-stack.sh for state-dir / port / cleanup.
dev-stack:
    ./scripts/dev-stack.sh

# Boot pharos with a known config, run schemathesis against the live
# port, then shut down. Layer A of T29. Requires `pkgs.schemathesis` from
# the devShell.
compat-openapi addr="127.0.0.1:18096":
    @echo "Fetching Jellyfin OpenAPI spec to target/jellyfin-openapi.json"
    mkdir -p target
    curl -fsSL https://api.jellyfin.org/openapi/jellyfin-openapi-stable.json \
        -o target/jellyfin-openapi.json
    @echo "Run pharos under PHAROS_BIND={{addr}} in another shell, then:"
    @echo "  nix develop --command schemathesis run \\"
    @echo "      --base-url http://{{addr}} \\"
    @echo "      --checks all \\"
    @echo "      --hypothesis-max-examples 50 \\"
    @echo "      target/jellyfin-openapi.json"

# Run the in-process Jellyfin client roundtrip test (Layer B).
compat-client:
    nix develop --command cargo nextest run --workspace --test client_compat

# Playwright driving headless jellyfin-web (Phase 3). Assumes pharos is
# already running on PHAROS_URL (default http://127.0.0.1:8096) and that
# `compat-playwright/scripts/setup.sh` has run at least once.
compat-playwright:
    nix develop --command bash -c 'cd compat-playwright && npx playwright test'

# Convenience: spin up pharos with seeded data + run Playwright in one
# shot. Uses a fresh tmp sqlite db so prior state doesn't leak.
compat-playwright-full:
    #!/usr/bin/env bash
    set -euo pipefail
    TMP=$(mktemp -d)
    trap 'rm -rf "$TMP"' EXIT
    cat > "$TMP/pharos.toml" <<EOF
    [server]
    bind = "127.0.0.1:8096"
    name = "pharos-playwright"

    [obs]
    log_level = "warn"

    [media]
    roots = []

    [database]
    url = "sqlite://$TMP/pharos.db?mode=rwc"
    EOF
    PHAROS_CONFIG="$TMP/pharos.toml"
    nix develop --command cargo run -q --bin pharos -- --config "$PHAROS_CONFIG" admin seed-playwright-user
    nix develop --command bash -c "cargo run -q --bin pharos -- --config '$PHAROS_CONFIG' serve" &
    SERVER_PID=$!
    trap 'kill $SERVER_PID 2>/dev/null || true; rm -rf "$TMP"' EXIT
    sleep 2
    nix develop --command bash -c 'cd compat-playwright && npx playwright test'
