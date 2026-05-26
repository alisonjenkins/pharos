#!/usr/bin/env bash
# T29 phase 3 — one-shot setup for the Playwright compat suite.
#
# Steps:
#   1. npm install in compat-playwright/
#   2. Clone + build jellyfin-web into compat-playwright/jellyfin-web
#      (uses upstream master; pin to a tag for stability)
#   3. Verify pharos binary is reachable
#
# Run via:  nix develop --command bash compat-playwright/scripts/setup.sh
#
# Assumes nix devShell is active (provides nodejs + playwright browsers
# via PLAYWRIGHT_BROWSERS_PATH).

set -euo pipefail

PHAROS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SUITE_DIR="$PHAROS_DIR/compat-playwright"
JF_WEB_DIR="$SUITE_DIR/jellyfin-web"
JF_WEB_REPO="https://github.com/jellyfin/jellyfin-web.git"
# Pin to a known-good tag. Bump deliberately and re-baseline tests when
# upstream changes selectors.
JF_WEB_REF="${JF_WEB_REF:-v10.10.7}"

echo "==> npm install in $SUITE_DIR"
cd "$SUITE_DIR"
npm install

echo "==> Skipping Playwright browser install (using nix-pinned browsers via PLAYWRIGHT_BROWSERS_PATH)"

if [ ! -d "$JF_WEB_DIR" ]; then
    echo "==> Cloning jellyfin-web @ $JF_WEB_REF"
    git clone --depth 1 --branch "$JF_WEB_REF" "$JF_WEB_REPO" "$JF_WEB_DIR.src"
    cd "$JF_WEB_DIR.src"
    echo "==> Building jellyfin-web (this can take a while)"
    npm ci
    npm run build:production
    mv dist "$JF_WEB_DIR"
    cd "$SUITE_DIR"
    rm -rf "$JF_WEB_DIR.src"
else
    echo "==> jellyfin-web already built at $JF_WEB_DIR — skipping"
fi

echo "==> Verifying pharos binary"
if ! "$PHAROS_DIR/target/debug/pharos" --version >/dev/null 2>&1; then
    echo "(building pharos in debug mode)"
    (cd "$PHAROS_DIR" && cargo build --bin pharos)
fi

echo "==> Setup complete."
echo
echo "To run the suite:"
echo "  1. Start pharos in another shell, seeded with the test user:"
echo "       cargo run --bin pharos -- admin seed-playwright-user"
echo "       cargo run --bin pharos -- serve"
echo "  2. Run: just compat-playwright"
