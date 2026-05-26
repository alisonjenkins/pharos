# SPEC.md — pharos

## §G goal

Rust media server. Wire-compat with Jellyfin + Plex client ecosystems. Better perf/reliability than both. Group watch + group listen first-class, not bolt-on.

## §C constraints

- Lang: Rust stable. Backend + frontend both Rust.
- HTTP framework: actix-web (perf over axum).
- CLI: clap derive (declarative) mode. No builder API.
- Web UI: Dioxus (Rust → WASM). Single-lang stack.
- API compat: existing Jellyfin clients (Finamp, Infuse, Jellyfin web/mobile/TV) work unmodified.
- API compat: existing Plex clients (Plex web/mobile/TV, Plexamp) work unmodified.
- Phase 1: feature-parity with Jellyfin.
- Phase 2: extensions past parity.
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

## §T tasks

```
id|status|desc|cites
T1|x|cargo workspace skeleton, actix-web app, config loader, tracing init, OTel+Prom exporters, test harness, trait scaffolding, clippy lints (V17), nix flake (devShell + package + OCI)|I.config,I.cli,I.obs,I.nix-flake,V11,V12,V13,V15,V17,V18
T2|.|sqlite store layer via sqlx, migrations|I.store,V10
T3|.|media-fs scanner: walk roots, extract metadata via ffprobe|I.media-fs,I.ffmpeg,V5
T4|.|user/auth model + token issuance|V8
T5|.|jellyfin-api: /System/Info, /Users/AuthenticateByName, /Users/Me|I.jellyfin-api,V1,V7
T6|.|jellyfin-api: /Library/* + /Items/* (browse, search, details)|I.jellyfin-api,V1,V7
T7|.|jellyfin-api: /Videos/{id}/stream, /Audio/{id}/universal (direct play)|I.jellyfin-api,V1,V9
T8|.|transcode pipeline: ffmpeg wrapper, segment delivery, format negotiation|I.ffmpeg,V6
T9|.|jellyfin-api: transcoded streaming + HLS|I.jellyfin-api,V1,V6
T10|.|jellyfin-api: /Sessions, /PlayState (playback reporting)|I.jellyfin-api,V1
T11|.|plex-api: identity, /myplex auth bridge, /library/sections|I.plex-api,V2,V7
T12|.|plex-api: /library/metadata, hubs, search|I.plex-api,V2,V7
T13|.|plex-api: streaming + transcode endpoints|I.plex-api,V2,V6,V9
T14|.|plex-api: timeline + session reporting|I.plex-api,V2
T15|.|group-sync protocol design doc + invariants for V3|I.group-sync,V3
T16|.|group-sync impl: websocket hub, room model, drift correction|I.group-sync,V3
T17|.|group-sync client integration via existing Jellyfin/Plex session hooks|I.group-sync,V3,V1,V2
T18|.|feature-parity audit vs Jellyfin: gap list, prioritize|V1
T19|.|fill parity gaps from T18|V1
T20|.|extensions past parity (TBD — defer scope to post-T19)|
T21|.|Jellyfin architecture audit: extract patterns, map to idiomatic Rust traits, doc in `docs/jellyfin-mapping.md`. Lands before T5.|V12
T22|.|health-api: `/healthz`, `/readyz`, `/info`, `/metrics`. Lands with T1.|I.health-api,V14
T23|.|observability deepening: span attrs for media ops, RED metrics per route, log redaction|I.obs,V8,V13,V15
T24|.|dioxus-ui crate: workspace member, WASM build pipeline, served via axum static + fallback|I.dioxus-ui,V16
T25|.|dioxus-ui: login + library browse views, talks Jellyfin-compat API|I.dioxus-ui,V16,V1
T26|.|dioxus-ui: player view (HLS + direct), session reporting|I.dioxus-ui,V16,V1
T27|.|dioxus-ui: group session UI (join room, sync indicator, chat)|I.dioxus-ui,V3,V16
```

## §B bugs

```
id|date|cause|fix
```
