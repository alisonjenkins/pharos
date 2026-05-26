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
T6|x|jellyfin-api: /Library/* + /Items/* (browse, search, details). T6 phase 1: GET /Items, GET /Items/{id}, GET /Users/{uid}/Items, GET /Library/VirtualFolders (search/filters/images deferred to T6 phase 2)|I.jellyfin-api,V1,V7,V9
T7|x|jellyfin-api: /Videos/{id}/stream, /Audio/{id}/universal (direct play) via actix-files NamedFile + Range support. Auth extractor accepts api_key query param.|I.jellyfin-api,V1,V9
T8|.|transcode pipeline: ffmpeg wrapper, segment delivery, format negotiation|I.ffmpeg,V6
T9|.|jellyfin-api: transcoded streaming + HLS|I.jellyfin-api,V1,V6
T10|x|jellyfin-api: /Sessions, /PlayState (playback reporting). Actor-owned SessionRegistry; POST Playing/Progress/Stopped + Capabilities accept body and update state; GET /Sessions returns active list.|I.jellyfin-api,V1,V18
T11|.|plex-api: identity, /myplex auth bridge, /library/sections|I.plex-api,V2,V7
T12|.|plex-api: /library/metadata, hubs, search|I.plex-api,V2,V7
T13|.|plex-api: streaming + transcode endpoints|I.plex-api,V2,V6,V9
T14|.|plex-api: timeline + session reporting|I.plex-api,V2
T15|x|group-sync protocol design doc + invariants for V3|I.group-sync,V3,V8,V18
T16|x|group-sync impl phase 1: WS at /sync/v1/ws, ClientMsg/ServerMsg, Group actor + registry, clock-offset estimator with median over N=9 samples. Jellyfin /socket bridge deferred to T16 phase 2.|I.group-sync,V3,V18,V19,V20
T17|.|group-sync client integration via existing Jellyfin/Plex session hooks|I.group-sync,V3,V1,V2
T18|x|feature-parity audit vs Jellyfin: gap list, prioritize. See `docs/jellyfin-parity-audit.md`.|V1,V7
T19|.|fill parity gaps from T18|V1
T20|.|extensions past parity (TBD — defer scope to post-T19)|
T21|x|Jellyfin architecture audit: extract patterns, map to idiomatic Rust traits, doc in `docs/jellyfin-mapping.md`. Lands before T5.|V12
T22|x|health-api: `/healthz`, `/readyz`, `/info`, `/metrics`. Lands with T1.|I.health-api,V14,V18
T23|x|observability deepening: span attrs for media ops, RED metrics per route, log redaction|I.obs,V8,V13,V15
T24|x|dioxus-ui crate skeleton phase 1: workspace member, library with first component, builds on host. WASM build pipeline + actix static serving deferred to T24 phase 2.|I.dioxus-ui,V16
T25|.|dioxus-ui: login + library browse views, talks Jellyfin-compat API|I.dioxus-ui,V16,V1
T26|.|dioxus-ui: player view (HLS + direct), session reporting|I.dioxus-ui,V16,V1
T27|.|dioxus-ui: group session UI (join room, sync indicator, chat)|I.dioxus-ui,V3,V16
T28|x|architecture doc + diagrams (mermaid) in `docs/architecture.md`: components, crate graph, request flow, concurrency model, data flow|V12,V18
```

## §B bugs

```
id|date|cause|fix
B1|2026-05-26|parse_device_id matched `Device=` before `DeviceId=` because the loop returned on first hit. Test caught via auth header containing both keys.|prefer DeviceId; fall back to Device only if absent. Caught by V11 test-first practice — no new invariant.
```
