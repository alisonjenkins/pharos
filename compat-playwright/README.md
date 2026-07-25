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

## Diagnostic probes (`tools/`)

Not part of the suite — reach for these when a *player* symptom needs
attributing to *server* bytes. They exist because a byte-level diagnosis is not
a diagnosis until a real player has agreed with it: in one session a confident
root cause ("a stray MP4 chapter track makes hls.js throw") survived inspection
and died the moment hls.js was handed the ladder carrying it.

They drive the same hls.js jellyfin-web ships, and report whether playback
advanced *and* whether the player refetched a fragment — the signature of a
segment it cannot use. Production once served one segment 17 times inside a
single second, all `200`.

| recipe | question |
| --- | --- |
| `just probe-flags --variant a: --variant b:-flag,value` | can a player tell two ffmpeg output-flag variants apart? |
| `just probe-capture <base> <item> <key> <session>` | grab the exact bytes a running pharos serves |
| `just probe-bytes <dir> [rung] [browser]` | replay those bytes through a real player |

Both exit non-zero when a probe stalls or storms, so they gate like a test.

**Browser choice matters.** Playwright's Firefox ships **no H.264 decoder**, so
it cannot judge the `h264cmaf` rung at all — the exact cell a desktop-Firefox
user occupies. Use `--browser system-firefox` (the `firefox` on `PATH`, driven
headless with the page posting its verdict back) for that rung; Playwright's
browsers are fine for VP9 and for Chromium.

**Capturing from the deployment.** Segment routes `410` without a registered
`PlaySessionId`, so reuse a live one — it and the `api_key` appear in the
server's `http.target` span field, which means a wedged session can be captured
while it is still wedged:

```bash
kubectl port-forward -n pharos <pod> 18080:8096 &
just probe-capture http://127.0.0.1:18080 <item-id> <api-key> <play-session-id> ./capture 18-21
just probe-bytes ./capture h264cmaf system-firefox
```
