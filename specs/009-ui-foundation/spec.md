# Feature Specification: pharos-ui foundation — rendering, shell, routing, data

**Feature directory**: `specs/009-ui-foundation/`
**Created**: 2026-11-01
**Status**: draft (`/speckit-specify` stage; next: `/speckit-clarify`)
**Input**: "We need to start working on making a modern UI for Pharos using
webgl or similar in the browser … a first class UI." (Alison, 2026-11-01)
**Depends on**: ADR-0012 (Dioxus + dx WASM), ADR-0028 + 017 (pharos-native
Cap'n Proto API, which pharos-ui speaks instead of the Jellyfin REST API)
**Enables**: 010, 011, 012, 013, 014, 015
**Gap record**: `docs/ui-gap-assessment.md`

## Goal

Turn `crates/pharos-ui` from a set of form-like panes into an application
platform that every later UI spec builds on: a GPU-composited visual layer, a
URL router, a cached data layer fed by the server websocket, a design-token
system, and measurable performance and accessibility budgets.

This spec ships **no new end-user feature pages**. It ships the ground they
stand on, plus one proof screen (a poster rail) that exercises every layer.

## Clarifications

### Session 2026-11-01

- Q: Which renderer should the GPU layer use first? → A: Raw WebGL2 via `glow` (WebGPU later behind the `Renderer` port).
- Q: Which browsers must v1 support? → A: Desktop (Chrome, Firefox, Safari, Edge) + Meta Quest Browser + Steam Frame browser. No smart-TV or phone browsers.
- Q: Which UI languages must v1 ship with? → A: English only, i18n-ready (catalogue + pseudo-locale test).
- Q: How strict should the first-load size budget be? → A: Measure in the plan spike, then gate from the measurement; CI catches regressions.
- Q: Where should the new UI live once enabled? → A: At `/`; jellyfin-web stays at `/web/`. UI routes must not collide with API paths (FR-002).

## Rendering approach (decision to record as an ADR when it lands)

"WebGL or similar" is met by a **hybrid** renderer, not a full-canvas app:

- **GPU layer** (WebGL2, behind a small Rust renderer trait so WebGPU can
  replace it later): poster walls and rails, backdrops and their blur/vignette,
  hero transitions, focus glow, parallax, shader-driven theme effects
  (gradients, scanlines, grain, "matrix" backgrounds for Kali-style themes),
  trickplay scrub previews, audio waveform and visualiser.
- **DOM layer** (Dioxus): all text, forms, menus, dialogs, the player chrome,
  and everything that must be selectable, screen-reader visible, translatable
  or typed into.
- The DOM layer is the **source of truth for structure and focus**; the GPU
  layer draws what the DOM layout says is there (rects read from layout), so
  keyboard focus, accessibility and hit-testing never depend on the canvas.
- When WebGL2 is unavailable, or the user picks "reduced effects", the GPU
  layer is skipped and the same screens render as plain DOM/CSS. Every screen
  MUST be fully usable in that mode.

Why not full canvas (as Netflix's TV apps do): text input, IME, selection,
find-in-page, screen readers and browser zoom all break, and we would have to
rebuild a layout engine. Why not DOM only: hundreds of animated posters with
blur and glow is where DOM/CSS drops frames on modest laptops.

**Renderer (clarified)**: raw WebGL2 via `glow`, behind the `Renderer` port so
WebGPU can replace it later.

## Boundaries: every external touch point is swappable

**Requested by Alison, 2026-11-01**: "make all the touch points with external
things easy to change." This is a structural rule for 009–016, not a feature.

**Rule**: application code (views, state, queue, sync, themes, telemetry logic)
never calls an external library, browser API or wire format directly. It calls
a **port**, a narrow Rust trait owned by pharos-ui and named in pharos terms.
Each port has:

1. one or more **adapters** in their own module (`adapters/<port>/<impl>.rs`);
   the only place that imports the external crate, JS binding or `web-sys` type;
2. a **fake** adapter used by SSR and unit tests, so every view is testable
   without a browser;
3. **selection in one place** (`adapters/mod.rs`), by build feature or runtime
   config, so swapping an implementation is one line plus the new module;
4. a **contract test suite** run against every adapter, so a replacement proves
   it behaves like the one it replaces;
5. **telemetry at the port** (016): calls, failures and latency recorded by the
   port wrapper, labelled with the adapter name, so two adapters can be compared
   on the same signals.

No external type (an hls.js event name, a `web_sys::WebGl2RenderingContext`,
a Jellyfin DTO field casing) appears in a port's signature.

### Port inventory (v1)

| Port | Purpose | v1 adapter | Likely replacements | Spec |
|---|---|---|---|---|
| `PharosApi` | All UI↔server calls, as pharos domain operations (`resume_items`, `play_info`, …) | **native Cap'n Proto RPC via the shared `pharos-client` core (017 FR-001a)**; the current Jellyfin REST `client.rs` stays as a transitional adapter until each behaviour has a native method (017 FR-012) | other transports | 009, 017 |
| `LiveChannel` | Server push: library/user-data/session/sync events | native `Events` subscription (017 FR-004); Jellyfin `/socket` as transitional adapter | — | 009, 017 |
| `Renderer` | GPU layer surfaces, sprites, effects | WebGL2 via `glow` | wgpu/WebGPU, null | 009 |
| `ImageSource` | Sized artwork + placeholders | `/Items/{id}/Images` | CDN / pre-sized cache | 009 |
| `Storage` | Device-local settings, saved servers, offline queue | `localStorage` / IndexedDB | OPFS | 009 |
| `Router` / `History` | URL ↔ route, history entries | browser History API | — (fake for tests) | 009 |
| `Messages` | i18n catalogue + formatting | Fluent | ICU4X | 009 |
| `Clock` | Wall + monotonic time (sync, telemetry) | `performance.now` / `Date` | fake for tests | 009, 014 |
| `ThemeStore` | Load/save/import themes | native `Prefs` interface (017) | — | 010 |
| `Fonts` | Font loading | bundled + system | user-uploaded | 010 |
| `KeySource` / `MediaKeys` | Keyboard, gamepad, OS media keys | DOM events, Media Session | Gamepad API | 011 |
| `PlaybackEngine` | Load, seek, tracks, quality, buffer/error events, stats | hls.js via wasm-bindgen | **all-Rust/WASM HLS→MSE engine** (planned) | 013 |
| `CapabilityProbe` | Codec/HDR/display capability → DeviceProfile | MSE + `mediaCapabilities` | — | 013 |
| `SubtitleRenderer` | Cue rendering | DOM WebVTT; JASSUB for ASS | Rust renderer | 013 |
| `BookReader` (×3) | epub / pdf / comic | epub.js / pdf.js / libarchive.js | Rust readers | 013 |
| `XrPlayer` | VR playback | vendored jellyfinvr | own WebXR view | 013 |
| `GroupSync` | Group create/join/commands/state | native `Group` interface carrying the pharos group-sync protocol (017, V20) | — | 014 |
| `RemoteSessions` | Control other sessions | native `Sessions` interface (017) | — | 014 |
| `TelemetrySink` | Ship events/spans | native `Telemetry` interface (017) | direct OTLP/HTTP, null | 016 |
| `TraceContext` | Span ids + `traceparent` | in-house W3C impl | OpenTelemetry-wasm | 016 |

New external dependencies added later get a row here and a port in the same PR.
A third-party dependency that shapes the design also gets an ADR (CLAUDE.md).

### Server side

The same rule applies on the server: native RPC methods (017) and Jellyfin
handlers are thin wire adapters over shared domain services (V172), and sinks
such as telemetry (Loki via `tracing`, OTLP via the existing exporter) sit
behind traits so they can be swapped without touching a handler.

## User Stories

### US1 — every screen has a URL (P1)

A user can bookmark, refresh, share or press Back on any screen and land in the
same place: `/`, `/browse/{id}?sort=…&filter=…`, `/title/{id}`,
`/watch/{id}?t=…`, `/find?q=…`, `/settings/{tab}`, `/manage/{pane}` (final
names in `/speckit-plan`; FR-002 constrains them).

**Independent test**: open a filtered library URL in a fresh tab; the same items,
sort and scroll anchor render. Browser Back returns to the previous screen with
its scroll and focus restored.

### US2 — the app feels instant (P1)

Navigating between screens shows cached content immediately and refreshes it in
the background; poster rails prefetch the next page before the user reaches it.

**Independent test**: navigate home → library → home; the second home render
issues no blocking request and paints in under one frame budget.

### US3 — the app stays live (P1)

Changes made elsewhere appear without reload: a new item finishes scanning, an
item is marked played on the TV, another session starts playing, an admin task
completes.

**Independent test**: mark an item played from jellyfin-web; within 2 s its
badge updates in pharos-ui with no user action.

### US4 — the GPU layer is beautiful and optional (P1)

A rail of 50 posters scrolls, focuses and transitions with blur/glow at 60 fps;
switching on "reduced effects" (or a browser without WebGL2, or
`prefers-reduced-motion`) gives a still-complete, still-pretty static version.

**Independent test**: proof rail renders identically in both modes in content
and layout; only effects differ. Playwright snapshot of both.

### US5 — one design system (P2)

Every component reads its colours, type, spacing, radii, elevation and motion
from design tokens (consumed by 010 theming); no component hard-codes a colour.

**Independent test**: a lint/test fails when a component stylesheet contains a
literal colour outside the token file.

### US6 — accessible and translatable (P2)

All interactive elements have roles and names; contrast meets WCAG 2.2 AA in the
default theme; all user-visible strings come from a message catalogue.

**Independent test**: axe-core run in Playwright reports zero serious/critical
violations on the proof screen; a pseudo-locale build renders every string
transformed.

## Functional Requirements

- **FR-001** URL router at the site root `/` with typed routes (Rust enum ↔
  path), history push/replace, scroll and focus restoration per history entry.
  Replaces `Signal<AppRoute>` in `app_state.rs`.
- **FR-002** pharos-ui is served at `/` when enabled; jellyfin-web stays at
  `/web/`. The SPA fallback serves `index.html` only for a `GET` that matched
  no server route and accepts `text/html`, so deep links survive refresh
  without shadowing any API path. Because pharos matches Jellyfin paths
  case-insensitively (`middleware/path_case.rs`), **no UI route's first segment
  may equal, case-insensitively, any Jellyfin or pharos route segment**
  (`Items`, `Users`, `Library`, `Search`, `Admin`, `Videos`, `Shows`,
  `System`, `web`, `pharos`, `metrics`, `healthz`, …). A test enumerates the
  actix route table and the UI route enum and fails on any collision. Verify in
  both the in-process `ui_dir` mount and the angie sidecar. Dioxus `base_path`
  changes from `ui` to the root; `/ui/*` redirects to `/` for old bookmarks.
- **FR-003** Data layer: a query cache keyed by (endpoint, params, user) with
  stale-while-revalidate, request de-duplication, cancellation on navigation,
  pagination helpers, and optimistic mutation with rollback (favorite, played,
  rating).
- **FR-004** Live channel over the 017 RPC connection: connect, keep-alive,
  reconnect with backoff and re-subscribe, and dispatch of typed library,
  user-data, session, remote-control, group, task and activity events into
  cache invalidation and app events (the Jellyfin `/socket` only as the
  transitional adapter). Connection state is visible in the shell.
- **FR-005** Renderer trait (`Surface`, `Layer`, `Sprite`, `Effect`) with a
  WebGL2 implementation and a null implementation; one shared full-viewport
  canvas behind the DOM; sprites positioned from DOM layout rects with a
  `ResizeObserver`/scroll sync that does not lag the DOM by more than a frame.
- **FR-006** Image pipeline: requests `/Items/{id}/Images/{type}?fillWidth=&quality=`
  sized to device pixels; blurhash/low-quality placeholder first; GPU texture
  atlas with LRU eviction under a configurable VRAM budget; DOM `<img>` fallback.
- **FR-007** Design tokens: a single typed token set (colour roles, type scale,
  spacing, radii, elevation, motion durations/easings, effect parameters)
  emitted as CSS custom properties AND as uniforms for the GPU layer. Shape is
  owned by 010.
- **FR-008** App shell: collapsible side rail (Home, libraries, Search, Live TV,
  Together, Settings, Admin), top bar (search entry, now-playing chip,
  connection state, user menu), toasts, modal/sheet host, and a persistent
  mini-player slot (used by 013).
- **FR-009** Split `app_state.rs` into `router`, `shell`, `data`, `ws`, `render`
  modules and per-route view modules; keep SSR testability (dioxus-ssr) for
  every DOM view.
- **FR-010** i18n: message catalogue (Fluent or equivalent), locale chosen from
  user prefs then browser; dates/numbers/durations formatted per locale; RTL
  layout not required in v1. **v1 ships English only**, but every user-visible
  string goes through the catalogue; a pseudo-locale build in CI renders every
  string transformed and fails on any literal that bypassed it.
- **FR-011** Accessibility baseline: semantic roles, visible focus ring (also
  drawn by the GPU layer as glow, never instead of the DOM ring), ARIA live
  region for toasts and playback state, respects `prefers-reduced-motion`,
  `prefers-contrast`, browser zoom to 200 %.
- **FR-012** Error surfaces: every failed request shows the server's message and
  a retry; offline/unauthorised states route to a recoverable screen; client
  errors are posted to the server's client-log endpoint (ODD symmetry — the
  failure path records as much as the success path).
- **FR-014** Ports and adapters per §Boundaries: every row of the port inventory exists as a trait with a real adapter, a fake, and a contract test before any feature spec depends on it. A test fails the build if a view module imports an adapter module, `web-sys`, `js-sys` or `gloo-*` directly.
- **FR-013** Feature flag and coexistence: jellyfin-web stays deployed at `/web/`;
  pharos-ui is at `/` when enabled (FR-002), and `/` keeps its current
  behaviour when it is not. A setting in each lets the user open the same
  item in the other client.

## Non-functional requirements / budgets

- **NFR-001** First load: the `/speckit-plan` spike measures the compressed
  wasm + JS + CSS of the proof screen (with hls.js, readers and non-default
  shaders lazy-loaded) and sets the size gate from it, recorded in
  `quickstart.md`; CI then fails a PR that grows the first-load bundle more than
  5 % past the gate without updating it in the same PR, with a stated reason.
  Interactive in ≤ 2 s on a 2020 laptop on LAN.
- **NFR-002** Route change to first contentful paint ≤ 100 ms from cache.
- **NFR-003** Rails/grids: 60 fps scroll and focus on integrated GPUs with
  ≥ 500 posters in a grid (virtualised).
- **NFR-004** GPU memory ≤ 256 MB texture budget by default.
- **NFR-005** Supported browsers (v1): current desktop Chrome, Firefox, Safari
  and Edge; the **Meta Quest Browser**; and the browser on Valve's **Steam
  Frame** headset. Both headsets get the full UI in their 2D browser window
  (input via controllers/hand pointer acting as a mouse, plus the 011 keyboard
  model with a paired keyboard) and the 013 US9 WebXR entry. Which engine the
  Steam Frame browser uses, and its WebXR and codec support, is established in
  the `/speckit-plan` research rather than assumed. Smart-TV and phone browsers
  are out of scope: pharos's own TV and phone apps will cover them.

## Observability

The UI reports, through the client-log endpoint or a small metrics beacon:
renderer mode chosen and why (`webgl2` / `fallback:no_webgl2` /
`fallback:user_reduced` / `fallback:context_lost`), long frames over budget,
websocket reconnects with reason, and request failures with endpoint + status.
Server-side, a counter by `mode` lets us see real fallback rates by query.

## Success criteria

- **SC-001** Proof rail passes Playwright in both renderer modes, and the axe run
  is clean.
- **SC-002** Deep-link, refresh and Back work on every route in FR-001.
- **SC-003** A `UserDataChanged` from another client updates the UI in ≤ 2 s.
- **SC-004** Budgets NFR-001..004 measured and recorded in `quickstart.md`.

## Edge cases

- WebGL context lost (GPU reset, tab backgrounded on mobile): drop to fallback,
  rebuild on restore, no blank screen.
- Very high DPR (3×) and very low-end GPUs: texture sizing caps, atlas eviction.
- Token expired mid-session: websocket and REST both route to re-auth without
  losing the current URL.
- Two pharos servers in one browser (T59 multi-server): cache keyed per server.

## Out of scope

Feature pages (012–015), theme editor (010), shortcut map (011).
