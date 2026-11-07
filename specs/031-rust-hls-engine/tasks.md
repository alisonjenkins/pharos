# Tasks: a Rust HLS engine for the web UI (031)

**Input**: [spec.md](spec.md), ADR-0071. Ids are local to this spec (`T031-NN`). Every code task follows TDD: a failing test first, then the code. Test in Chromium and Firefox with audio muted.

## Slice 0 - Baseline (measurements only, no player change)

- [ ] T031-01 Corpus: record master, media playlists, init and first media segments for an H.264 CMAF film, a VP9 film and an audio-only item from a dev server into `crates/pharos-hls/tests/corpus/` (small clips only, under 2 MB in all).
- [ ] T031-02 Playwright harness that plays one stream for N seconds on a named engine and writes JSON: time to first frame, seek-to-playing time, buffered ranges every second, long tasks, JS heap. Runs in Chromium and Firefox, muted.
- [ ] T031-03 hls.js baseline on Chromium, Firefox, Quest, Steam Frame and the G08 (device runs by hand via CDP/WebView debugging); results table appended to this file with ISO 8601 UTC timestamps.

## Slice 1 - Seam (V630)

- [ ] T031-04 `Engine::RustHls`; `choose_engine` takes the engine preference; `engine()` routes `RustHls` to hls.js until slice 2 lands, logging `reason=fallback cause=not-built`.
- [ ] T031-05 Per-device setting in Settings, Playback, stored in `localStorage`; `?engine=` override; `pharos: playback engine=… reason=…` log and client event.
- [ ] T031-06 Engine-neutral telemetry: `hls_event` takes the engine label; auto quality gates on an `Engine::reports_fragments()` capability instead of `== Engine::HlsJs`.

## Slice 2 - Play (V631-V636, V639, V641-V643)

- [ ] T031-07 `crates/pharos-hls`: crate, m3u8 parser, URI resolution with query kept, unsupported-feature reasons. Tests against server-generated playlists and the corpus (V631).
- [ ] T031-08 Timeline: time to segment, segment to time range (V632, property tests).
- [ ] T031-09 Planner state machine: next request per track, A/V lockstep, seek, eviction, forward/back targets, quota shrink (V633, V635, V636).
- [ ] T031-10 fMP4 inspection: codec strings from the init `moov`, `tfdt` of a media segment, for sanity checks and logs.
- [ ] T031-11 MSE adapter `RustHls` in `native/web.rs`: MediaSource, SourceBuffers, fetch with AbortController, append queue, `OnFragment`, drop cleanup (V641, V642).
- [ ] T031-12 Fallback before first append for unsupported streams (V639); wasm size check in CI (V643).
- [ ] T031-13 e2e: start position, seeks, audio change, auto quality step, SyncPlay rate kept (V634, V635).

## Slice 3 - Recover (V637, V638, V640)

- [ ] T031-14 Retries with backoff and per-device deadlines (V637).
- [ ] T031-15 Gap jumping and stall nudges, each counted and logged.
- [ ] T031-16 MediaSource rebuild on media error under `RecoveryBudget` (V638); failure-path symmetry tests with a disarm check (V640).

## Slice 4 - Measure and flip

- [ ] T031-17 Comparison harness in CI on the corpus streams, both engines.
- [ ] T031-18 Device traces against the slice 0 baseline; a separate ADR or ADR-0071 acceptance to flip the default.
