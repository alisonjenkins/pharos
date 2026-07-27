# Implementation Plan: pharos baseline

**Branch**: `main` | **Date**: 2026-07-27 | **Spec**: [spec.md](./spec.md)
**Input**: migrated from `SPEC.md` §C (constraints) on 2026-07-27

## Summary

pharos is a single Rust binary serving a Jellyfin-compatible HTTP/WS API, its own
Dioxus WASM UI, a filesystem scanner, an ffmpeg-backed transcode pipeline and a
SyncPlay-compatible group-watch engine, over a sqlx store (sqlite default, postgres
in production). Domain logic sits behind `pharos-core` traits so every IO backend —
store, transcoder, metadata provider — swaps by wiring rather than refactor.

## Technical Context

**Language/Version**: Rust stable, backend and frontend (Dioxus → WASM). WASM target
pinned in `rust-toolchain.toml`.

**Primary Dependencies**:
- actix-web (chosen over axum for throughput), clap derive (no builder API)
- sqlx behind `MediaStore` and sibling core traits; native `async fn` in traits (no
  `async_trait`); generics preferred over `dyn Trait`
- `sonic-rs` for JSON, `xxhash-rust` xxh3 for stable ids, `blake3` for content
  hashing, `image` for decode — SIMD where available, graceful fallback elsewhere
- `tracing` + `metrics` + Prometheus exporter + OTLP
- Dioxus + the `dx` CLI for the UI; reqwest in compat tests only

**Storage**: sqlite by default, postgres feature-gated and used in production (CNPG).
Migrations via `sqlx::migrate!`. sqlx does **not** check placeholder arity or column
names at compile time, so any `sqlx::query*` change in `pharos-store-sqlx` needs
`just test-postgres` — `just test` skips those arms.

**Transcode**: ffmpeg, two interchangeable backends by Cargo feature. `ffmpeg-lib`
(default) runs the high-frequency tiny ops — probe, image extract, trickplay tiles,
srt→webvtt, waveform — in-process via `ffmpeg-the-third`, serviced by a persistent
crash-isolated `LibavWorkerPool`; a libav fault kills a worker, never the server.
Video segment and live transcode always stay on the spawn worker, where encode time
dwarfs fork/exec and the scheduler load-balances every GPU and CPU. `ffmpeg-spawn`
forks the binaries outright.

Pixel formats are encoder-specific and always set explicitly: mjpeg needs full-range
`yuvj420p`; software/NVENC/QSV/VideoToolbox H.264/HEVC force `-pix_fmt yuv420p` for
8-bit 4:2:0 client compat; VAAPI uploads via `format=nv12,hwupload` instead.

**Testing**: `cargo nextest run --workspace` (config in `.config/nextest.toml`),
doctests separately via `cargo test --doc --workspace`. Fast loops through
`just test-fast` (lib only) and `just test-changed` (cargo-guppy blast radius); full
`just test` before every commit; `just test-thorough` with `PROPTEST_CASES=512` for
pre-release. Client compat has two layers: in-tree `tests/client_compat.rs` on every
PR, and manual/nightly schemathesis plus the jellyfin-web Playwright suite. Every hot
or SIMD path carries a `criterion` bench, gating regression rather than correctness.

**Target Platform**: Linux server, single binary. Deployed to home k3s via Flux;
chart under `charts/pharos` (bump `Chart.yaml` version or Flux will not reconcile).

**Project Type**: Rust cargo workspace — `pharos-core` (domain + traits),
`pharos-store-sqlx`, `pharos-transcode` (+ `transcode-worker`), the API crates, and
`pharos-ui` (Dioxus/WASM, outside the workspace-hack).

**Performance Goals**: group sync within 500 ms p95 (V3); segment transcode comfortably
above realtime under load — software encodes capped near 4 threads at `veryfast` with
a permit budget of `cores / threads`, never `permits = cores` (V23); speculative
prefetch work is shed rather than queued when it would take capacity an interactive
request needs (V58).

**Constraints**: no `unwrap()`/`expect()` outside tests, deny-level clippy (V17); no
`println!`/`eprintln!` in non-test code (V15); mutable runtime state owned by one
task and mutated over mpsc, no `Mutex<State>` on the request path (V18); no runtime
dependency beyond ffmpeg; no `Dockerfile` — images come from `nix build .#oci` via
`dockerTools`, built with `buildRustPackage` because the pinned nixpkgs
`buildRustCrate` mishandles `ffmpeg-the-third`'s `cargo::`-syntax version cfgs.

**Scale/Scope**: ~14k library items on NFS-backed storage; 100 tasks, 82 invariants,
121 recorded bugs; `replicaCount: 1` in steady state, surging only during a rolling
update.

## Constitution Check

The constitution's teeth are `invariants.md`. Before implementing, a change states
which invariants it touches; after, its tests assert them. Standing gates:

- [ ] TDD — failing test precedes the change (V11)
- [ ] ODD — the LogQL/PromQL query that proves the behaviour is named before the fix,
      and instrumentation lands as its own commit if it does not yet exist
- [ ] No new `unwrap`/`expect`/`println!` in non-test code (V15, V17)
- [ ] Wire objects are typed DTOs, enum fields carry real enum members (V38, V39)
- [ ] `just test` green; `just test-postgres` if any `sqlx::query*` changed;
      `just hakari-regen` if any `Cargo.toml` dependency changed

## Project Structure

### Documentation (this feature)

```
specs/001-pharos-baseline/
├── spec.md          # goal, interfaces, user stories, success criteria
├── plan.md          # this file — stack + architecture + gates
├── tasks.md         # T1…T100 with status; open work listed first
├── invariants.md    # V1…V81 — normative register
└── bugs.md          # B1…B129 — cause + fix + guarding invariant
```

### Source Code (repository root)

```
crates/
├── pharos-core/         # domain types + traits (MediaStore, transcoder, providers)
├── pharos-store-sqlx/   # sqlite + postgres impls, migrations
├── pharos-transcode/    # ffmpeg backends, scheduler, libav worker pool
├── pharos-ui/           # Dioxus WASM frontend
└── …                    # api crates, server binary, workspace-hack
charts/pharos/           # Flux-reconciled Helm chart
compat-playwright/       # jellyfin-web E2E + SyncPlay harness
tests/                   # client_compat.rs, ssr_render.rs, integration suites
```

## Complexity Tracking

Deliberate deferrals, each with a standing reason (see `tasks.md` for the full text):

| Item | Why deferred |
|------|--------------|
| T11–T14 Plex API | Phase 2; no Plex work blocks Jellyfin progress |
| T85, T91 multi-replica SyncPlay | steady state is `replicaCount: 1`; these bite only during a deploy surge and touch the working ownership layer |
| T92 native `/sync/v1/ws` gate parity | native clients only; jellyfin-web unaffected |
| T80 postgres test harness | until it lands, pg-only SQL divergences ship silently (B19 class) |
| T20 extensions past parity | scope intentionally undefined until T19 lands |
