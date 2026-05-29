set shell := ["bash", "-cu"]

# Run the full nextest suite via the nix devShell.
# P47 — strip macOS Gatekeeper quarantine attr off freshly-linked
# test binaries before nextest lists them. Cold first-launch on
# macOS triggers a synchronous Gatekeeper scan per binary (~1-5s
# each); the workspace's ~60 test bins burn 1-3 min there.
# Stripping the attr collapses that overhead to zero. No-op on
# Linux + CI.
test:
    -xattr -dr com.apple.quarantine target/debug/ 2>/dev/null || true
    nix develop --command cargo nextest run --workspace

# P47 — same as `test` but with full proptest case counts. Default
# `test` runs proptests at 32 cases each via PROPTEST_CASES unset
# (the test config falls back to 32); CI + nightly should run with
# 512 to catch the rare shrink seeds.
test-thorough:
    -xattr -dr com.apple.quarantine target/debug/ 2>/dev/null || true
    PROPTEST_CASES=512 nix develop --command cargo nextest run --workspace

# Clippy under workspace lints (denies warnings).
lint:
    nix develop --command cargo clippy --workspace --all-targets -- -D warnings

# Supply-chain checks (T45). Runs cargo-audit (RustSec advisories) and
# cargo-deny (licenses + bans + sources) under the policies in
# deny.toml.
audit:
    nix develop --command bash -c 'cargo audit && cargo deny check'

# Regenerate Cargo.nix from Cargo.lock. crate2nix turns every
# Cargo.lock entry into its own nix derivation, so the /nix/store
# becomes the dep cache + dedupes shared deps across projects. Run
# after any change to `Cargo.toml` / `Cargo.lock`; commit the result.
regen-cargo-nix:
    nix develop --command crate2nix generate

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

# T51 phase 3 — drive the pharos Dioxus UI under headless chromium.
# Assumes pharos is running with `[server].ui_dir` pointed at a
# `dx build` output. Skips cleanly when `/ui/` is not served.
# Build the bundle once via:
#   nix develop --command dx build --package pharos-ui --release
# then point `[server].ui_dir` at `target/dx/pharos-ui/release/web/public`.
compat-dioxus:
    nix develop --command bash -c 'cd compat-playwright && npx playwright test --config playwright.dioxus.config.ts'

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
