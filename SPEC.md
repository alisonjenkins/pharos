# SPEC.md — pharos

## §G goal

Rust media server. Wire-compat with Jellyfin clients (Phase 1) and Plex clients (Phase 2). Better perf/reliability than both. Group watch + group listen first-class, not bolt-on. Group-sync is a **primary motivation** — Jellyfin's SyncPlay is buggy in practice (late-joiner desync, poor-network member drags whole group, buffer storms on leader handoff). pharos must improve on those, not replicate them.

**Phase 1 priority order**: (1) Jellyfin client compat, (2) Dioxus web UI, (3) core (scan/store/group-sync). Plex (T11–T14) waits until Phase 1 is solid.

## §C constraints

- Lang: Rust stable. Backend + frontend both Rust.
- HTTP framework: actix-web (perf over axum).
- CLI: clap derive (declarative) mode. No builder API.
- DB: sqlx. All DB access behind `pharos-core` traits (e.g. `MediaStore`). Backend impls (sqlite default, postgres optional) live in adapter crate. Call sites depend on traits only — swap via wiring, not refactor.
- Async traits: native `async fn` in traits (stable Rust 1.75+). No `async_trait` crate. Prefer generics over `dyn Trait` for swappable backends.
- SIMD: use SIMD-accelerated crates where applicable. JSON parsing via `sonic-rs` (ByteDance; 1.5–2× faster than `simd-json` on common workloads, no intermediate tape). Hashing via `xxhash-rust` (xxh3) for stable IDs and `blake3` for content hashing. Image decode via `image` (default SIMD). Fall back gracefully on non-supported archs.
- Benchmarks: every SIMD-accelerated or otherwise hot code path has a `criterion` benchmark under the owning crate's `benches/`. Bench gates regressions, not unit correctness.
- Test runner: `cargo nextest run --workspace`. Config in `.config/nextest.toml`. Doctests via `cargo test --doc` separately.
- Web UI: Dioxus (Rust → WASM). Single-lang stack.
- API compat: existing Jellyfin clients (Finamp, Infuse, Jellyfin web/mobile/TV) work unmodified — Phase 1 hard requirement.
- API compat: existing Plex clients (Plex web/mobile/TV, Plexamp) work unmodified — **Phase 2; deprioritised**. T11–T14 hold but no Plex work blocks Jellyfin progress.
- Phase 1: feature-parity with Jellyfin + pharos-native group-sync.
- Phase 2: Plex client compat + extensions past Jellyfin parity.
- Transcoding via ffmpeg (subprocess, no FFI initially).
- Single binary deploy. Config file + env vars. No external runtime deps beyond ffmpeg.
- TDD: failing test before impl change. No "test later".
- Design: mine Jellyfin structure/feature list for ideas. Adapt idiomatic Rust — traits at IO/abstraction boundaries, not OOP class translation.
- Observability baked from T1. Not retrofit. Traces + metrics + structured logs always-on.
- Concurrency: actor pattern (tokio task + mpsc) for mutable runtime state. Locks only for one-shot init or proven read-only caches.
- Dev env: Nix flake canonical. `nix develop` → reproducible shell with rust toolchain, clippy, rustfmt, ffmpeg. CI uses same flake.
- Build artifacts: `nix build .#pharos` → server binary. `nix build .#oci` → OCI image via `dockerTools`. No `Dockerfile`.

## §I interfaces

- jellyfin-api — HTTP/REST surface matching Jellyfin schemas (auth, system, library, items, playback, sessions).
- plex-api — HTTP/XML+JSON surface matching Plex schemas (auth, library, hubs, streaming, sessions).
- group-sync — websocket protocol for synced playback across clients in shared session.
- media-fs — filesystem scan target. Configured roots. Watch for changes.
- ffmpeg — subprocess for transcode/probe/thumbnail.
- store — sqlite (default) or postgres for metadata/users/sessions.
- config — TOML file + env override. Path via `--config` or `PHAROS_CONFIG`.
- cli — `pharos serve`, `pharos scan`, `pharos admin <subcommand>`.
- dioxus-ui — Dioxus web frontend, served by backend. WASM bundle. Replaces Jellyfin-web role.
- obs — OpenTelemetry traces (OTLP exporter) + Prometheus metrics (`/metrics`) + structured logs via `tracing` crate.
- health-api — `/healthz` liveness, `/readyz` readiness, `/info` build+version.
- nix-flake — `nix develop` devShell; `nix build .#pharos` server; `nix build .#oci` OCI image (dockerTools).

## §V invariants

- V1: unmodified Jellyfin client connects, browses library, plays media (direct + transcoded).
- V2: unmodified Plex client connects, browses library, plays media (direct + transcoded).
- V3: group session syncs play/pause/seek across members within 500ms p95.
- V4: HTTP handler never panics. Errors return structured response matching target API schema.
- V5: library scan never blocks playback or API requests. Scan runs in dedicated task pool.
- V6: ffmpeg subprocess crash never crashes server. Failed transcode returns error to client.
- V7: API response shapes match Jellyfin/Plex reference schemas byte-equivalent for shared fields.
- V8: auth tokens never logged. Secrets redacted in all log output.
- V9: media file path never leaks to unauthenticated client. Path traversal blocked at boundary.
- V10: store writes atomic per logical op. No partial library entry visible to readers.
- V11: every public behavior covered by test written before impl (TDD). No merge without failing-test-first evidence.
- V12: trait boundary between domain logic and IO. Domain layer testable without DB/fs/network/ffmpeg.
- V13: every inbound request traced. Every outbound IO (db, ffmpeg, fs walk, http client) spanned. trace_id propagated to logs.
- V14: `/healthz` returns 200 if process alive. `/readyz` returns 200 only when store reachable + scanner initialized. `/metrics` exposes Prometheus format.
- V15: structured logs only. No `println!`/`eprintln!` in non-test code. `tracing` crate sole logging surface. CLI subcommands may write to stdout/stderr via explicit `std::io::Write` (not logging).
- V16: Dioxus UI consumes only public Jellyfin-compat API. No backdoor endpoints. UI swappable for any Jellyfin client.
- V17: no `unwrap()` / `expect()` in non-test code. Enforced via `clippy::unwrap_used` + `clippy::expect_used` = deny at workspace level. Tests may opt-out.
- V18: mutable runtime state owned by exactly one task. Mutation via message-passing (tokio mpsc). No `Mutex<State>` on request path. Locks permitted only for one-shot init or immutable-after-init caches.
- V19: group-sync must improve on Jellyfin SyncPlay failure modes. Specifically: (a) late joiner does not desync existing members; (b) one poor-network member does not stall the group (per-member buffering isolation); (c) leader handoff causes ≤1 corrective Pause, no buffer storm; (d) network blip <2 s reconverges automatically without member rejoin.
- V20: group-sync wire protocol stays Jellyfin SyncPlay-compatible so unmodified phone/TV clients participate. Improvements live in server-side algorithm. A second WS path may carry an extended protocol for pharos-native clients, but the Jellyfin-shaped socket is the canonical entry point.

## §T tasks

```
id|status|desc|cites
T1|x|cargo workspace skeleton, actix-web app, config loader, tracing init, OTel+Prom exporters, test harness, trait scaffolding, clippy lints (V17), nix flake (devShell + package + OCI)|I.config,I.cli,I.obs,I.nix-flake,V11,V12,V13,V15,V17,V18
T2|x|sqlx-backed `MediaStore` impl in `pharos-store-sqlx` crate (sqlite default, postgres feature-gated), migrations via `sqlx::migrate!`, wired through core trait — no call site knows backend|I.store,V10,V12
T3|x|media-fs scanner: walk roots, extract metadata via ffprobe|I.media-fs,I.ffmpeg,V5,V12,V18
T4|x|user/auth model + token issuance: User, UserPolicy, Argon2 password hash, TokenStore (sqlite) issuing opaque UUIDv4 tokens, AuthBackend trait, BuiltinAuth impl|V8,V12,V17
T5|x|jellyfin-api: /System/Info, /Users/AuthenticateByName, /Users/Me + AppState wiring + auth extractor for Emby/MediaBrowser headers|I.jellyfin-api,V1,V7,V4
T6|x|jellyfin-api: /Library/* + /Items/* (browse, search, details). Phase 1: GET /Items, /Items/{id}, /Users/{uid}/Items, /Library/VirtualFolders. Phase 2: SearchTerm + IncludeItemTypes filters + SortBy (SortName / Random) + SortOrder on /Items.|I.jellyfin-api,V1,V7,V9
T7|x|jellyfin-api: /Videos/{id}/stream, /Audio/{id}/universal (direct play) via actix-files NamedFile + Range support. Auth extractor accepts api_key query param.|I.jellyfin-api,V1,V9
T8|x|transcode pipeline phase 1: `pharos-transcode` crate, FfmpegTranscoder spawns subprocess, exposes byte stream via tokio_util::io::ReaderStream over ChildStdout, Drop kills child (V6), TranscodeOptions covers container/video-codec/audio-codec/start-position. Format negotiation matrix + HLS segments are T8 phase 2 + T9.|I.ffmpeg,V6,V12,V18
T9|x|jellyfin-api: transcoded streaming + HLS. Phase 1: /Videos/{id}/master.m3u8 + /main.m3u8 generated server-side with fixed 6-s segments; /Videos/{id}/hls1/main/{seg}.ts spawns ffmpeg via pharos-transcode. Duration probed per request (caching is T9 phase 2). Adaptive bitrate + DeviceProfile-driven format negotiation are phase 2.|I.jellyfin-api,V1,V6
T10|x|jellyfin-api: /Sessions, /PlayState (playback reporting). Actor-owned SessionRegistry; POST Playing/Progress/Stopped + Capabilities accept body and update state; GET /Sessions returns active list.|I.jellyfin-api,V1,V18
T11|.|plex-api: identity, /myplex auth bridge, /library/sections|I.plex-api,V2,V7
T12|.|plex-api: /library/metadata, hubs, search|I.plex-api,V2,V7
T13|.|plex-api: streaming + transcode endpoints|I.plex-api,V2,V6,V9
T14|.|plex-api: timeline + session reporting|I.plex-api,V2
T15|x|group-sync protocol design doc + invariants for V3|I.group-sync,V3,V8,V18
T16|x|group-sync impl phase 1: WS at /sync/v1/ws, ClientMsg/ServerMsg, Group actor + registry, clock-offset estimator with median over N=9 samples. Jellyfin /socket bridge deferred to T16 phase 2.|I.group-sync,V3,V18,V19,V20
T17|x|group-sync client integration via Jellyfin /socket SyncPlay bridge. Phase 1: /socket WS endpoint, translates SyncPlayCreateGroup/JoinGroup/Play/Pause to GroupRegistry, emits SyncPlayGroupUpdate / SyncPlayCommand outbound. Plex SyncPlay (timeline) is Phase 2.|I.group-sync,V3,V1,V20
T18|x|feature-parity audit vs Jellyfin: gap list, prioritize. See `docs/jellyfin-parity-audit.md`.|V1,V7
T19|x|fill parity gaps from T18. Phase 1: image stubs + per-user item. Phase 2: real Primary-image extraction via ffmpeg `-ss N -vframes 1 -f image2 -vcodec mjpeg`, disk cache under `[server].image_cache_dir`. Audio cover-art + backdrop/thumb/banner deferred to phase 3. Played-state still phase 4.|V1,V7
T20|.|extensions past parity (TBD — defer scope to post-T19)|
T21|x|Jellyfin architecture audit: extract patterns, map to idiomatic Rust traits, doc in `docs/jellyfin-mapping.md`. Lands before T5.|V12
T22|x|health-api: `/healthz`, `/readyz`, `/info`, `/metrics`. Lands with T1.|I.health-api,V14,V18
T23|x|observability deepening: span attrs for media ops, RED metrics per route, log redaction|I.obs,V8,V13,V15
T24|x|dioxus-ui phase 2: wasm32-unknown-unknown rust target via rust-toolchain.toml, dioxus-cli in nix devShell, dioxus-web optional dep + `web` bin entrypoint, actix-files serves config-driven ui dir with SPA fallback. Phase 1 components from T25 now mountable. Phase 3 (login → library wiring + fetch client) tracked under T25 follow-up.|I.dioxus-ui,V16
T25|x|dioxus-ui: login + library browse views, talks Jellyfin-compat API. Phase 1: pure components. Phase 2 (done): gloo-net HTTP client (WASM-only) + serde-parsed Jellyfin response types (parse helpers are host-testable) + RootApp signals-driven router wiring LoginForm → LibraryView → PlayerView.|I.dioxus-ui,V16,V1
T26|x|dioxus-ui: player view (HLS + direct), session reporting. T26 phase 1: PlayerView component renders <video>/<audio> pointing at /Videos/{id}/stream?api_key=…; emits PlaybackEvent callbacks (Started/Progress/Stopped) consumers can route to /Sessions/Playing/*. HLS variant + real fetch wiring deferred.|I.dioxus-ui,V16,V1
T27|x|dioxus-ui: group session UI (join room, sync indicator, chat). T27 phase 1: GroupSessionPanel component (member list + leader badge + buffering indicator + Join/Leave actions); WS subscription wiring lands with T25 fetch-client work. Chat deferred to T27 phase 2.|I.dioxus-ui,V3,V16
T28|x|architecture doc + diagrams (mermaid) in `docs/architecture.md`: components, crate graph, request flow, concurrency model, data flow|V12,V18
T29|x|client-compat validation suite. Phase 1A: pharos-jellyfin-test-client crate + integration test driving a real-client-style flow (auth headers, strict serde DTOs, full login→browse→stream-head roundtrip) against pharos spawned via actix-test. Phase 1B: justfile `compat-openapi` recipe documents schemathesis run. Phase 3: Playwright suite in `compat-playwright/` drives unmodified jellyfin-web 10.11.8 pointed at a running pharos instance — 14/14 tests pass across login, library, item details, real Play button → <video>.currentTime > 0, search, settings, logout. Phase 2 (docker reference byte diff) still deferred.|V1,V7,V11
T30|x|BaseItemDto array-field enrichment — Artists, AlbumArtists, Genres, Tags, People, Studios, ProductionLocations + serde_json::Map for ProviderIds/Trickplay/ImageTags. Fixes the audio-detail Symbol.iterator throw. Residual TypeError on a nullable Name field tracked + tolerated; Play button renders, route loads.|I.jellyfin-api,V1,V7
T31|.|path-case normalising middleware in pharos-server. Replaces the dual lowercase route aliases (currently each canonical route is duplicated for jellyfin-web's lowercase paths) with one Transform that rewrites the request URI before routing. Earlier middleware attempt reverted — actix's URI-mutation API needs a service-factory rewrite rather than head_mut.|V12
T32|x|Jellyfin search endpoints — `/Search/Hints` (case-insensitive substring against `MediaItem.title`; honours `searchTerm`, `limit`, `startIndex`, `includeItemTypes`; emits SearchHintsResult with `SearchHints` + `TotalRecordCount`; ItemId == Id duplicated per Jellyfin contract); `/Search/Suggestions` returns empty ItemsResult; `/Users/{u}/Suggestions` empty ItemsResult gated by bearer-matches-path check. `/Items?searchTerm=` enrichment already shipped in T6 phase 2. 9 new integration tests in `jellyfin_search.rs` + 2 unit tests in `search.rs::tests` cover auth, substring match, pagination, type filter, and lowercase-route alias. People + studios + genres entry types stay deferred until T33/T34 surface them — `SearchHint` shape allows additive enrichment.|I.jellyfin-api,V1,V7
T33|.|UserData service — played, play_count, last_played_position_ticks, favorite flags per (user, item). New `user_data` sqlx table + migration; /Users/{u}/PlayedItems/{i} POST+DELETE + /Sessions/Playing/Progress writes through. T19 phase 3.|I.store,I.jellyfin-api,V1,V7,V10
T34|.|image extraction phase 3 — Backdrop list (with index), Thumb, Logo, Art, Banner; embedded cover art for Audio; user upload POST /Items/{id}/Images/{type}. Builds on T19 phase 2 ImageCache.|I.jellyfin-api,V1,V6,V7
T35|x|persistent server identity — `system_identity` migration + `SqliteStore::load_or_create_server_id`. `AppState::load` reads it at boot; same id across restarts. Smoked locally — two restarts return identical `Id` from /System/Info/Public.|I.store,V1
T36|x|`pharos scan` CLI wires FsScanner + FfmpegProber into cli::Cmd::Scan. Walks [media].roots, upserts probed items into the store, reports imported / conflict-skipped counts.|I.cli,I.media-fs,V5
T37|x|CI workflow at `.github/workflows/ci.yml`. Four jobs all gated through `nix develop --command`: (1) `test` — cargo build --locked + nextest --profile ci + doctests + clippy -D warnings + rustfmt --check; (2) `audit` — cargo-audit + cargo-deny check; (3) `oci` — `nix build .#pharos` + `nix build .#oci`, uploads tarball artifact; (4) `compat-playwright` — `just compat-playwright-full` driving headless jellyfin-web, uploads report on failure. magic-nix-cache caches /nix/store across runs.|V11,V17
T38|.|real-ffmpeg integration tests behind `#[ignore]` — tiny generated fixtures (lavfi testsrc) drive FfmpegProber + FfmpegTranscoder + ImageCache against real ffmpeg. Opt-in nextest job in CI; manual via `cargo nextest run --run-ignored only`.|V6,V11
T39|.|PostgresStore real impl — UserStore + TokenStore + MediaStore via sqlx::Postgres feature. Migrations under crates/pharos-store-sqlx/migrations/postgres. Sqlite stays default.|I.store,V10,V12
T40|.|Jellyfin /socket message types phase 2 — KeepAlive (currently ignored), Sessions* control commands (PlayPause/Pause/Stop targeted at a remote session), library-refresh notifications (LibraryChanged, UserDataChanged broadcast on store write). T17 phase 2.|I.jellyfin-api,V1,V20
T41|.|transcode pipeline phase 2 — DeviceProfile XML parse, codec/container negotiation matrix decides direct-play vs transcode based on client capabilities, audio remux when only codec mismatches. T8 phase 2.|I.ffmpeg,V6
T42|.|HLS segment cache — disk-backed cache of transcoded segments under [server].transcode_cache_dir; LRU eviction. Subsequent segment requests hit cache, no respawn of ffmpeg.|I.jellyfin-api,V6
T43|.|V19 conformance test harness — simulated late-joiner, poor-network member, leader-handoff, network blip; tokio paused-time + mock WS sinks. Asserts each Jellyfin SyncPlay failure mode is fixed in pharos's actor. Hard release gate for the group-sync feature.|V19,V3
T44|x|V8 redaction integration test — custom tracing layer captures all events through a live AuthenticateByName → Users/Me → Items → Sessions walk. Asserts the issued token does not appear in any captured byte; defence-in-depth regex scan rules out 32-char hex outside known-public id contexts. Catches future regressions where a SecretString gets `.expose()`-printed into a tracing field.|V8,V11
T45|x|cargo-audit + cargo-deny wired via `just audit` + the `audit` job in `.github/workflows/ci.yml`. Checked-in `deny.toml` (license allowlist incl. CDLA-Permissive-2.0 for webpki-roots; RUSTSEC-2023-0071 ignored with justification — `rsa` reachable only through sqlx-mysql, which we never enable; wildcard policy allows internal `path = "..."` deps via `publish = false` on all workspace crates) + `.cargo/audit.toml` keeping cargo-audit in sync. Runs clean against current Cargo.lock.|V17
T46|.|Jellyfin admin dashboard endpoints — /System/Configuration POST (admin writes), /Users CRUD, /Library/VirtualFolders CRUD, /Library/Refresh, /ScheduledTasks, /Plugins (empty list + GET/POST stubs), /System/Logs, /Sessions (with control commands), /Configuration/MetadataOptions. Admin-only via UserPolicy.admin. Drives the jellyfin-web /#/dashboard tree. Was won't-do; user reversed to planned.|I.jellyfin-api,V1,V7,V8
T47|.|Live TV support — /LiveTv/Channels, /LiveTv/Recordings, /LiveTv/Schedules, /LiveTv/SeriesTimers, /LiveTv/Timers, EPG storage. Tuner backends abstracted via a `TunerBackend` trait in pharos-core; first impls: HDHomeRun, M3U+XMLTV. Was won't-do; user reversed to planned.|I.jellyfin-api,V1,V7,V12
T48|.|DLNA support — MediaRenderer + MediaServer profiles, SSDP discovery on the local network, UPnP/SOAP control endpoints under /Dlna/*, device profiles XML negotiation. New `pharos-dlna` crate. Was won't-do; user reversed to planned.|V1,V12
T49|.|SyncPlay Playwright multi-context test — two browser contexts join the same group via /socket, leader issues Play, both followers' <video>.currentTime stays within V3's 500ms p95 over a 30 s window. End-to-end proof of V19 + V20 in a real browser pair.|V3,V19,V20,V11
T50|.|Dioxus admin UI mirroring T46's server-side admin endpoints — user management, library config, scheduled tasks, log viewer. Lives behind a `/ui/admin/*` route, gated by UserPolicy.admin. Phase 1 reuses jellyfin-web's dashboard at /#/dashboard; this row is the pharos-native replacement once admin endpoints stabilise.|I.dioxus-ui,V16
```

## §B bugs

```
id|date|cause|fix
B1|2026-05-26|parse_device_id matched `Device=` before `DeviceId=` because the loop returned on first hit. Test caught via auth header containing both keys.|prefer DeviceId; fall back to Device only if absent. Caught by V11 test-first practice — no new invariant.
```
