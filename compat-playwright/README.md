# Playwright compat suite

T29 phase 3 — drives **unmodified jellyfin-web** in a headless browser against a running pharos instance. Catches client-side behaviour that the in-process Rust tests (`tests/client_compat.rs`) cannot: real browser cookie/storage, real DOM expectations, real CORS / SOP behaviour, real WebSocket upgrade from jellyfin-web's network layer.

## Reproducibility

- **jellyfin-web bundle**: pinned via `pkgs.jellyfin-web` in `flake.nix`. The devShell exports `JELLYFIN_WEB_DIR=${pkgs.jellyfin-web}/share/jellyfin-web` — `playwright.config.ts` reads it and hands the path to `http-server`. Bumps to the upstream version land via a flake input update, not a clone+build dance.
- **Chromium**: `pkgs.playwright-driver.browsers` → `PLAYWRIGHT_BROWSERS_PATH`. No `npx playwright install` step.
- **Node.js + Playwright runtime**: `pkgs.nodejs_22` + `npm install` to pull `@playwright/test` + `http-server`. The npm install is the only network step.

## One-time setup

```bash
nix develop --command bash compat-playwright/scripts/setup.sh
```

`npm install`s the Playwright + http-server bits and confirms the pharos binary is built.

## Running the suite

One-shot — spins up pharos against a tmp sqlite db, seeds the well-known test user, runs Playwright:

```bash
just compat-playwright-full
```

Two-shell variant when you want pharos running between iterations:

```bash
# shell 1
nix develop --command cargo run --bin pharos -- admin seed-playwright-user
nix develop --command cargo run --bin pharos -- serve

# shell 2
just compat-playwright
```

## What's covered

| Scenario | Notes |
|---|---|
| Connect server URL | jellyfin-web's first-run form posts `/System/Info/Public` + `/QuickConnect/Enabled`. |
| Login with valid creds | POSTs `/Users/AuthenticateByName`, expects PascalCase response. Asserts navigation to `home.html`. |
| Library tiles appear | `/Users/{uid}/Items/Latest` + `/Users/{uid}/Views`. Asserts at least one card renders. |
| Wrong password | Asserts error toast/modal appears within 10 s. |

## Not covered yet

- Actual media playback (`<video>` against `/Videos/{id}/stream`) — needs a real fixture file. Follow-up task.
- HLS — same story plus the segment loop.
- SyncPlay UI — needs a second browser context; tracked under T29 phase 3b.
- Image rendering — covered by Layer B and HEAD checks already.

## Updating jellyfin-web

Bump `flake.lock`'s nixpkgs input. New `pkgs.jellyfin-web` flows in automatically; re-baseline brittle selectors as needed.
