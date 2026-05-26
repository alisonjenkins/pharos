# Playwright compat suite

T29 phase 3 — drives **unmodified jellyfin-web** in a headless browser against a running pharos instance. Catches client-side behaviour that the in-process Rust tests (`tests/client_compat.rs`) cannot: real browser cookie/storage, real DOM expectations, real CORS / SOP behaviour, real WebSocket upgrade from jellyfin-web's network layer.

## One-time setup

From the nix devShell at the repo root:

```bash
nix develop --command bash compat-playwright/scripts/setup.sh
```

This:

1. `npm install`s the suite's dependencies.
2. Clones `jellyfin-web` at a pinned tag (`v10.10.7` by default; override with `JF_WEB_REF=…`).
3. Builds `jellyfin-web/dist` and moves it to `compat-playwright/jellyfin-web/`.
4. Builds the pharos binary in debug mode.

Playwright browsers come from `pkgs.playwright-driver.browsers` in the devShell — no `npx playwright install` needed.

## Running the suite

In two shells:

```bash
# shell 1 — start pharos with the test user pre-seeded.
nix develop --command bash -c '
  cargo run --bin pharos -- admin seed-playwright-user
  cargo run --bin pharos -- serve --config compat-playwright/pharos.toml
'

# shell 2 — run Playwright. http-server fires up automatically per
# playwright.config.ts webServer block.
just compat-playwright
```

Or, one-shot:

```bash
just compat-playwright-full
```

(see the `justfile` — wraps both steps; uses tmp dir for sqlite + auto-shutdown).

## What's covered

| Scenario | Notes |
|---|---|
| Connect server URL | jellyfin-web's first-run "Connect" form posts to `/System/Info/Public`, then `/QuickConnect/Enabled`. Verifies both endpoints respond shape-correctly. |
| Login with valid creds | POSTs `/Users/AuthenticateByName`, expects PascalCase response. Asserts navigation to `home.html`. |
| Library tiles appear | `/Users/{uid}/Items/Latest` + `/Users/{uid}/Views`. Asserts at least one card renders. |
| Wrong password | 401 path. Asserts error toast/modal appears within 10 s. |

## What it does NOT cover (yet)

- Actual media playback — `<video>` against `/Videos/{id}/stream` needs a real fixture file. Add a small FFmpeg-generated 5 s MP4 in `tests/fixtures/` and a follow-up test.
- HLS — same story plus the segment loop.
- SyncPlay UI — jellyfin-web's group sync needs a second browser context; tracked under T29 phase 3b.
- Image rendering — covered by Layer B and HEAD checks already; UI assertions are brittle.

## Updating

If the suite breaks after `jellyfin-web` upstream changes:

1. Bump `JF_WEB_REF` in `scripts/setup.sh`.
2. Re-run setup (clean: `rm -rf jellyfin-web`).
3. Re-baseline brittle selectors. Prefer `getByRole` / `getByLabel` over CSS selectors.
