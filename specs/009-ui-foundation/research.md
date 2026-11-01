# Research: 009-ui-foundation

**Date**: 2026-11-01. Sizes were measured with host `cargo` 1.99.1 in a cloud
container, not the Nix devShell (flake inputs unreachable there). T001
re-measures in the devShell before any gate is set.

## R1 — baseline bundle (Clarifications Q4: measure, then gate)

| Build of today's `pharos-ui-web` | raw | gzip |
|---|---|---|
| workspace release profile (opt 3, no LTO, not stripped) | 3.53 MB | 724 KB |
| `opt-level=z`, LTO, 1 CGU, `strip` | 1.86 MB | 525 KB |

Neither has had `wasm-opt` or `wasm-bindgen`'s section stripping, which `dx build
--release` applies; expect the shipped figure to be lower. **Decision**: add a
`[profile.wasm-release]` (z, LTO, 1 CGU, strip, `panic = "abort"`) used by the dx
release build. The gate is set from the devShell `dx build --release` output
(T001), with first-load = `index.html` + wasm + JS glue + CSS, compressed.

## R2 — GPU layer: WebGL2 via `glow` (Clarifications Q1)

- **Evidence**: a standalone `glow` + `web-sys` crate that compiles shaders,
  uploads a buffer and texture and draws: **917 KB raw / 200 KB gzip**
  stripped. Most of that is std, panic machinery and `web-sys` glue the UI
  already carries, so this is an upper bound on the increment. T001 measures the
  real increment inside pharos-ui.
- **Decision**: `glow` behind the `Renderer` port. A shader catalogue (named
  shaders with bounded uniforms; 010 themes refer to them by name). One
  full-viewport canvas behind the DOM (`position: fixed; z-index: -1`,
  `pointer-events: none`).
- **DOM↔GPU sync**: sprites are registered by the DOM component that owns them
  (a `use_gpu_sprite(node_ref, texture)` hook). Each frame, one
  `requestAnimationFrame` pass reads `getBoundingClientRect` for **visible**
  registered nodes only (an `IntersectionObserver` maintains the visible set),
  then draws. Reads happen in the same rAF before paint, so the GPU never lags
  the DOM by a frame. Layout reads are batched (no interleaved writes) to avoid
  forced reflow.
- **Fallback**: `Renderer` null implementation + CSS (`backdrop-filter`,
  gradients) when WebGL2 is missing, the context is lost, the user picks
  reduced effects, or `prefers-reduced-motion` is set. The reason is recorded
  (016 FR-011).

## R3 — router

- **Decision**: `dioxus-router` 0.7 with a `#[derive(Routable)]` enum at the
  root (`/`). It gives typed routes, history mode and nested layouts out of the
  box, matching Dioxus 0.7 already in the tree. Scroll/focus restoration is
  added on top: a per-history-entry key in `history.state` maps to saved
  scroll offsets and the 011 focus path.
- **Alternative**: our own router over the History API. Rejected: same
  capability, more code.

## R4 — serving at `/` without shadowing the API (Clarifications Q5)

- **Finding**: the server wraps every request in `LowercasePath`
  (`main.rs:2078`), so `/Library/…` and `/library/…` are the same route. Today
  `ui_dir` is mounted with `Files::new("/ui", …)` (`main.rs:2086`), and its
  comment assumes hash routing, which 009 replaces with history mode.
- **Decision**: mount static assets at `/assets/*` + `/index.html`, and add a
  `default_service` that serves `index.html` only for `GET`/`HEAD` with
  `Accept: text/html` that matched no route. Redirect `/ui/*` → `/*` (308).
  `/web/*` (jellyfin-web) is unchanged. A test walks the actix route table
  (`router::configure` plus extra scopes) and the `Routable` enum, and fails if
  any UI first segment equals any server first segment after lowercasing.
- **angie sidecar** (`charts/pharos/templates/pharos-ui.yaml`): same
  `try_files`-style fallback, `/ui/` redirect, and **WebSocket upgrade
  proxying for `/pharos/rpc`** (017), with `proxy_read_timeout` above the RPC
  keep-alive interval.

## R5 — data layer lives in `pharos-client`, reactivity in pharos-ui

- **Decision**: the query cache (keys, stale-while-revalidate, dedupe,
  invalidation by event) is platform-neutral and lives in `pharos-client`
  (017 FR-001a), so phone/TV apps get it too. pharos-ui adds a thin Dioxus
  binding: `use_query(key, fetch)` returning a `Signal<QueryState<T>>`,
  subscribed to cache invalidations. Optimistic mutations (favourite, played)
  apply locally, send, then roll back on error and show a toast.
- **Native-first** (017 Q2): new views only see `PharosApi`'s native adapter.
  The REST adapter remains for the existing panes until they migrate.

## R6 — ports: generics vs `dyn` in wasm

- **Finding**: the constitution prefers generics over `dyn Trait`. In Dioxus,
  ports are reached through context (`use_context::<T>()`), which needs a
  concrete type per port.
- **Decision**: each port is a trait, and the app is assembled by one
  `Adapters` type alias set (`type Api = NativeApi;` and so on) in
  `adapters/mod.rs`. Components use the aliases, so switching an adapter is one
  line and there is no dynamic dispatch. Test builds swap the aliases for fakes
  via `cfg(test)`. Generics stay at the trait boundary, satisfying constitution
  V without threading type parameters through every component.

## R7 — i18n (Clarifications Q3)

- **Decision**: `fluent-bundle` with `.ftl` files embedded per locale; English
  only in v1. A `pseudo` locale (accented, padded strings) is built in CI, and
  a Playwright test fails if any visible text node is not pseudo-transformed.
- Size: measured in T001 (fluent's parser pulls `unic-langid`; acceptable if
  under 50 KB gzip, else switch to a precompiled message table).

## R8 — accessibility checks

- **Decision**: `@axe-core/playwright` in `compat-playwright/` (npm, already a
  Node project there) for the axe run (US6). Contrast tokens are checked in Rust
  unit tests (010 FR-006 shares the checker).

## R9 — headsets (Clarifications Q2)

- **Quest Browser**: Chromium-based, with WebGL2 and WebXR available.
- **Steam Frame**: the browser engine, WebGL2/WebXR and codec support are
  **not yet established** and must not be assumed. T002 records them from the
  device (user agent, `WebGL2RenderingContext`, `navigator.xr`,
  `MediaSource.isTypeSupported` for H.264/HEVC/AV1). Until then, Steam Frame
  support means "the 2D UI works with a pointer", which the fallback renderer
  guarantees.
