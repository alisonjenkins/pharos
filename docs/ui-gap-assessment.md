# pharos-ui gap assessment — towards a first-class, themeable, keyboard-first UI

**Date**: 2026-11-01
**Direction (Alison, 2026-11-01)**: pharos is the next-generation server, not a
Jellyfin clone. Jellyfin client compatibility stays; first-party clients (this
web UI first, pharos phone and TV apps later) use pharos's own API (017).

**Goal (Alison, 2026-11-01)**: a modern browser UI for pharos, rendered with
WebGL or similar, inspired by Netflix and Kali's prettiest themes; themeable by
the user; efficient by keyboard and by mouse; supports everything jellyfin-web
does and more, tailored to pharos (some Jellyfin features may be dropped).

**Sources read**: `CLAUDE.md`, `docs/adr/0012-dioxus-wasm-ui.md`,
`docs/dioxus-parity-audit.md` (T52), `docs/jellyfin-web-feature-matrix.md`,
`specs/001-pharos-baseline/tasks.md` (T51, T52–T60), specs 002–008 (008 reserves
"spec 009" for pharos-ui), `crates/pharos-ui/**`.

**Constitution**: checked against `.specify/memory/constitution.md` v1.0.0 and
the invariant register; see §6 for the one conflict found.

---

## 1. Where we are

### 1.1 The server is ready; the UI is not

The server side of Jellyfin parity is far ahead of the first-party UI. The
feature matrix marks the large majority of jellyfin-web's endpoints DONE
(home hubs, items/filters/sort/letter-jump, genres/studios/persons, music tabs,
seasons/episodes, similar, PlaybackInfo, HLS, trickplay, MediaSegments,
subtitles, collections, search, Quick Connect, devices, API keys, logs,
branding, named configs). Pharos-only capabilities have shipped too: books
(004/005), skip-intro detection (002), remote URL sources (008), the richer
group-sync path (ADR-0014/0016), waveform, cache quarantine/purge admin
(T118–T123), self-tuning playback and device spread (006/007).

Every one of those is reachable today only through **unmodified jellyfin-web**,
which remains the primary browser client (ADR-0001, ADR-0012).

### 1.2 `crates/pharos-ui` today (≈8 k lines)

| Area | State |
|---|---|
| Framework | Dioxus 0.7 → WASM via `dx`, served at `/ui/*` (ADR-0012). Good foundation; keep it. |
| Rendering | Plain DOM, one 96-line stylesheet (`assets/pharos.css`) with six CSS variables. No canvas, no WebGL, no motion system. |
| Routing | In-memory `Signal<AppRoute>`; **no URLs**, so no deep links, no back/forward, no refresh-safe state. |
| App shell | Single header with a row of buttons (Library, Search, Live TV, Prefs, Remote, Server, QC, Admin). |
| Home | None. `LibraryView` is a flat list (82 lines); no hub rows, no posters. |
| Detail | Phase 1: title, runtime, actions, people list; images parsed (`has_primary_image`) but not drawn. |
| Playback | Bare `<video src=…>`. **No MSE / hls.js**, so desktop Chrome/Firefox cannot play any transcoded item (008 §Out of scope). Side-picker for audio/subs does not switch streams. Chapter strip exists. No trickplay, no skip-intro, no quality picker. |
| Music | No album/artist views, no queue, no persistent mini-player. |
| Books | None (jellyfin-web's readers are used). |
| SyncPlay | `views/group.rs` is a 155-line presentational stub that is never mounted; **no websocket anywhere in the crate**. |
| Live TV | Channel list + tune; no guide grid. |
| Prefs | Display/Playback/Subtitles/Home/Languages tabs scaffolded (T55). |
| Admin | Users/Libraries/Devices/Activity/Tasks/Plugins/Logs/API keys (T58 phases 1–3). |
| Auth | Login, server picker, Quick Connect guest + authorize (T59). |
| Theming | None beyond the dark CSS variables. |
| Keyboard | No key handling at all (`onkeydown` appears nowhere). No focus model. |
| i18n / a11y | None. |
| Tests | 88 SSR tests (`tests/ssr_render.rs`), 3 Playwright tests (T51). |

T52's own count puts the UI at **~15 % of jellyfin-web's surface**. Against the
new goal (which adds rendering, theming and input requirements jellyfin-web
does not meet either) it is closer to 10 %.

### 1.3 Structural problems to fix before adding features

1. **No URL router** — every later feature (deep links, back key, command
   palette "go to", shareable SyncPlay invites) needs one.
2. **No data layer** — each pane calls `client.rs` directly and re-fetches; no
   cache, no invalidation from websocket events, no prefetch for rails.
3. **No websocket** — blocks SyncPlay, remote control, live library updates,
   admin activity, "now playing" elsewhere.
4. **No media engine** — blocks every transcoded play, quality switching,
   audio-track switching, live TV in Chrome/Firefox.
5. **No design system** — styling by ad-hoc class; nothing to theme.
6. **`app_state.rs` is 2.3 k lines** mixing routing, panes and web-sys glue.

---

## 2. Gap by goal pillar

| Pillar | Gap | Spec |
|---|---|---|
| Modern rendering ("WebGL or similar") | Whole layer missing | **009** |
| App shell, routing, data layer, websocket, perf budgets, a11y, i18n | Missing | **009** |
| User themeability (Netflix / Kali-inspired presets, custom themes) | Missing | **010** |
| Keyboard + mouse efficiency (focus model, shortcuts, command palette, remapping) | Missing | **011** |
| Browse & discover (home, libraries, detail, search, filters, collections, playlists, music, books, live TV guide, people) | ~20 % | **012** |
| Playback & reading (video engine, tracks, quality, trickplay, skip segments, music player/queue, book readers, live TV, VR) | ~10 % | **013** |
| Together (SyncPlay, watch-a-URL, remote control/cast, sessions) | ~5 % | **014** |
| Account, preferences, admin, metadata editing | ~40 % | **015** |
| Native API: efficient, typed, push-capable UI↔server contract (Cap'n Proto), shared with future pharos phone/TV apps | None (UI uses Jellyfin REST) | **017** |
| UI observability: client↔server trace correlation, client telemetry to Loki/Prometheus, playback timelines, problem reports, debug overlay | None (UI emits nothing) | **016** |

---

## 3. Jellyfin-web features proposed for DROPPING (Alison to confirm)

Each is listed with the reason it does not fit pharos. "Drop" means the new UI
will not build it; the server endpoints stay as they are for jellyfin-web and
native apps.

| # | jellyfin-web feature | Proposal | Reason |
|---|---|---|---|
| D1 | Plugins page, plugin catalogue / repositories / install | **Drop** | pharos has no plugin host and does not want one (004 §Goal). Server stub stays for client compat. |
| D2 | Startup wizard | **Drop** (replace with a first-run "create admin + add library" sheet only if no user exists) | pharos is configured by `config.toml` / Helm; the wizard's language/remote-access steps do nothing. |
| D3 | Forgot password + PIN-file flow | **Drop** | Jellyfin writes a PIN file to the server's disk; on a k8s deployment nobody reads it. Admin reset covers it. |
| D4 | Dashboard → Networking page | **Drop** | Ports, TLS, proxies are Helm/ingress concerns. Show read-only "how this server is reached" instead. |
| D5 | Dashboard → Transcoding/ffmpeg knobs (HW accel picker, thread count, CRF, throttle) | **Drop as editable**; replace with a read-only **Transcode devices** status page (capacity, in-use, learned allowance) | 003/006/007 made the server measure and self-tune; hand knobs would fight it. |
| D6 | Dashboard → Branding (custom CSS, login disclaimer) | **Replace** with 010 theming (server-default theme + login message) | Theming supersedes raw CSS injection. |
| D7 | Per-library metadata/image fetcher ordering, Kodi NFO saver settings | **Drop** | pharos's metadata pipeline (embedded → NFO → TMDB/MusicBrainz) is fixed by design; pharos does not write NFOs. |
| D8 | Cinema mode (intros/trailers before a film) | **Drop** | `/Intros` is a stub; low value. |
| D9 | Photo libraries + slideshow | **Drop for now** | `MediaKind` has no photo kind (Movie/Episode/Audio/Book). Revisit with a server spec. |
| D10 | Live TV DVR: recordings, timers, series timers, tuner + listing-provider setup | **Drop DVR**, keep guide + watch live | Server side is THIN; discovery already finds tuners. Confirm whether you record TV. |
| D11 | Offline downloads / sync to device | **Drop** (keep plain "Download file") | Browser-only UI; no service-worker media store planned. |
| D12 | Notifications page (notification services) | **Drop** | Server stub only; observability goes to Prometheus/Loki. |
| D13 | Backups page | **Drop** | Database is sqlite/CNPG-postgres; backups belong to the platform. |
| D14 | DLNA settings page | **Drop as editable** | DLNA (T48) is config-driven; show status only. |
| D15 | Chromecast sender | **Defer** (needs Google Cast SDK + a registered receiver) | Remote control of other pharos sessions (014) covers "play on the TV". Confirm. |
| D16 | Display "layout mode" (TV / mobile / desktop / experimental) | **Replace** with one responsive layout + 011 input modes | One codebase, adapting to input instead of a mode switch. |
| D17 | Help/tooltip dialogs linking to jellyfin docs | **Replace** with the shortcut sheet (`?`) and inline hints | |
| D18 | Remote subtitle / image / metadata *provider search* (OpenSubtitles etc.) | **Defer** until a server provider exists | Endpoints return empty (T76). |

---

## 4. Pharos-only features the UI should add ("and more")

- **Watch a URL together** — paste a link, it plays, invite the group (008 + 014).
- **Pharos-native group sync** with drift indicator, member buffering, chat (ADR-0014, T27/T49 phase 2).
- **Explain playback** — "why is this transcoding?" panel from `pharos_playback_decision_total` inputs (direct-play block, downgrade), per session.
- **Transcode-device status** and per-session placement (003/006/007).
- **Skip intro/outro** with detected segments, plus a per-user auto-skip preference (002).
- **Audio waveform scrubbing** for music and audiobooks (waveform endpoint).
- **Native book readers** inside the UI, including converted Kindle files (004/005).
- **Cache admin**: segment purge by time range, quarantine browser (T118–T123).
- **Command palette** reaching every item, route and admin action (011).
- **User themes** shareable as files, live-previewed (010).
- **VR** entry for headsets (ADR-0019) — reuse the vendored jellyfinvr player.

---

## 5. Recommended order

1. **017 native API + 009 foundation + 016 observability** (together: the API is the UI's data source, and every later screen is born instrumented) — router, data layer, websocket, media engine skeleton, render layer decision + spike, design tokens.
2. **010 theming** and **011 input** in parallel — both are cross-cutting and cheap to adopt early, expensive to retrofit.
3. **013 playback (video engine)** — unblocks real use in Chrome/Firefox.
4. **012 browse** — home, libraries, detail on the new render layer.
5. **014 together** — SyncPlay over the websocket from 009.
6. **015 account/admin** — migrate the existing T55/T58/T59 panes onto the design system.

Each spec is at the `/speckit-specify` stage: user stories, requirements,
success criteria and `[NEEDS CLARIFICATION]` markers. Next step for each is
`/speckit-clarify`.

The existing T52 audit (`docs/dioxus-parity-audit.md`) and T54–T60 rows remain
the record of what shipped; 009–015 supersede T52's roadmap rows.

---

## 6. Constitution check

| Principle / invariant | Specs | Verdict |
|---|---|---|
| I. Wire compatibility; jellyfin-web stays the compat baseline | 009 FR-013 keeps jellyfin-web at `/web/`; pharos-ui moves to `/` | OK |
| II. Group sync beats Jellyfin's; V20 allows a second WS path for pharos-native clients | 014 FR-001 (pharos protocol on the native API, bridged with SyncPlay members) | OK |
| III. Test-first + ODD | Every spec names its signals; 016 makes the UI side of ODD concrete | OK |
| IV. Never panics, never leaks (V8, V17) | 016 FR-010 panic hook, FR-013 client redaction | OK |
| V. Traits at every IO edge, generics over `dyn` | 009 §Boundaries (ports and adapters) | OK; `/speckit-plan` should prefer generics for ports where wasm size allows |
| Tech: Dioxus → WASM, Rust frontend | All | OK. hls.js / epub.js / pdf.js / libarchive.js / JASSUB are JS behind ports (009 §Boundaries); each gets an ADR when it lands |
| **V16: the Dioxus UI consumes only the public Jellyfin-compat API** | All UI specs | **Resolved 2026-11-01 (Alison): pharos is the next generation, not a Jellyfin client. ADR-0028: pharos-ui and future pharos phone/TV apps use the pharos-native API (017); the Jellyfin API stays fully supported for third-party clients; both sit on shared domain services (V172). V16 amended, constitution v1.1.0.** |
