#!/usr/bin/env bash
# T29 phase 3 — one-shot setup for the Playwright compat suite.
#
# Just two things:
#   1. npm install in compat-playwright/ (Playwright runtime + http-server).
#   2. Verify the pharos binary is built.
#
# jellyfin-web is *not* cloned or built locally — the nix devShell pins
# `pkgs.jellyfin-web` (the upstream prebuilt static bundle) and exports
# JELLYFIN_WEB_DIR so playwright.config.ts can hand it to http-server.
#
# Browser binaries come from `pkgs.playwright-driver.browsers` via
# PLAYWRIGHT_BROWSERS_PATH — `npx playwright install` is unnecessary.
#
# Run via:  nix develop --command bash compat-playwright/scripts/setup.sh

set -euo pipefail

PHAROS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SUITE_DIR="$PHAROS_DIR/compat-playwright"

echo "==> npm install in $SUITE_DIR"
cd "$SUITE_DIR"
npm install --no-audit --no-fund

if [ -z "${JELLYFIN_WEB_DIR:-}" ]; then
    echo "ERROR: JELLYFIN_WEB_DIR is not set."
    echo "Enter the nix devShell first (it exports JELLYFIN_WEB_DIR from pkgs.jellyfin-web)."
    exit 1
fi
echo "==> jellyfin-web bundle: $JELLYFIN_WEB_DIR"

echo "==> Verifying pharos binary"
if ! "$PHAROS_DIR/target/debug/pharos" --version >/dev/null 2>&1; then
    echo "(building pharos in debug mode)"
    (cd "$PHAROS_DIR" && cargo build --bin pharos)
fi

echo "==> Setup complete."
echo
echo "Run the suite:"
echo "  just compat-playwright-full"
