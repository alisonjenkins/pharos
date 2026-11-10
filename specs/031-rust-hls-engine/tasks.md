# Tasks: a Rust HLS engine for the web UI (031)

**Input**: [spec.md](spec.md), ADR-0071. Ids are local to this spec (`T031-NN`). Every code task follows TDD: a failing test first, then the code. Test in Chromium and Firefox with audio muted.

## Slice 0 - Baseline (measurements only, no player change)

- [x] T031-01 Corpus: record master, media playlists, init and first media segments for an H.264 CMAF film, a VP9 film and an audio-only item from a dev server into `crates/pharos-hls/tests/corpus/` (small clips only, under 2 MB in all). `scripts/record-hls-corpus.py` records it (about 1.3 MB) and `tests/it_corpus` reads it. The audio-only item is muxed MPEG-TS, which the Rust engine leaves to hls.js.
- [x] T031-02 Playwright harness that plays one stream for N seconds on a named engine and writes JSON: time to first frame, seek-to-playing time, buffered ranges every second, long tasks, JS heap. Runs in Chromium and Firefox, muted. `tests/dioxus/hls-bench.spec.ts` with `playwright.hls-bench.config.ts`, off unless `PHAROS_HLS_BENCH=1`; long tasks and heap are Chromium only (Firefox has neither API).
- [ ] T031-03 hls.js baseline on Chromium, Firefox, Quest, Steam Frame and the G08 (device runs by hand via CDP/WebView debugging); results table appended to this file with ISO 8601 UTC timestamps.

## Slice 1 - Seam (V630)

- [x] T031-04 `Engine::RustHls`; `choose_engine` takes the engine preference; `engine()` routes `RustHls` to hls.js until slice 2 lands, logging `reason=fallback cause=not_built asked=rust asked_by=<reason>`.
- [x] T031-05 Per-device setting in Settings, Playback, stored in `localStorage`; `?engine=` override; `pharos: playback engine=… reason=…` log and client event.
- [x] T031-06 Engine-neutral telemetry: `hls_event` takes the engine label (moved to slice 2, when a second engine raises errors); auto quality gates on an `Engine::reports_fragments()` capability instead of `== Engine::HlsJs` (done). The choice is reported as a `rendererMode` event with `renderer=hls`, `mode=<engine>`, `reason`.

Notes from slice 1 (2026-11-07T23:00:00Z): Playwright's bundled Firefox does not advance `currentTime` on the HLS fixture here, for hls.js too (the existing "an HLS title plays through hls.js" case fails the same way), so Firefox playback checks need the system Firefox (puppeteer-core) in slice 0. The engine-choice logs pass in both browsers.

## Slice 2 - Play (V631-V636, V639, V641-V643)

- [x] T031-07 `crates/pharos-hls`: crate, m3u8 parser, URI resolution with query kept, unsupported-feature reasons. Tests against server-generated playlists and the corpus (V631).
- [x] T031-08 Timeline: time to segment, segment to time range (V632, property tests).
- [x] T031-09 Planner state machine: next request per track, A/V lockstep, seek, eviction, forward/back targets, quota shrink (V633, V635, V636).
- [x] T031-10 fMP4 inspection: track timescales from the init `moov`, `tfdt` of a media segment, for sanity checks and logs. Codec strings come from the master's `CODECS`, not `moov`.
- [x] T031-11 MSE adapter `RustHls` in `native/web.rs`: MediaSource, SourceBuffers, fetch with AbortController, append queue, `OnFragment`, drop cleanup (V641, V642).
- [ ] T031-12 Fallback before first append for unsupported streams (V639, done: `plan_hls` causes `unsupported_tag`, `not_fmp4`, `live`, `codec`, `no_variant`); wasm size check in CI (V643, open).
Notes from slice 2 (2026-11-08T00:40:00Z): with #949's `PHAROS_FIREFOX_LIBAV` on the loader path, Playwright's Firefox plays the fixture. "the Rust engine override plays and seeks without hls.js" and the hls.js case both pass in Chromium and Firefox. The dioxus config is Chromium-only, so the Firefox run used a local config that adds a Firefox project with the together suite's launch options. The Rust engine raises its own `engine=rust` events, so `hls_event` keeps `engine=hlsjs` for hls.js only (T031-06 done).

- [ ] T031-13 e2e: start position, seeks, audio change, auto quality step, SyncPlay rate kept (V634, V635).

## Slice 3 - Recover (V637, V638, V640)

- [x] T031-14 Retries with backoff and per-device deadlines (V637). The deadlines (desktop 10 s first byte, 60 s load; TV and phone 15 s, 90 s) are provisional until the slice 0 baseline (T031-03) measures them.
- [x] T031-15 Gap jumping and stall nudges, each counted and logged (V701, V702). The 1 s jump bound is provisional.
- [x] T031-16 MediaSource rebuild on media error under `RecoveryBudget` (V638, V1062); failure-path symmetry tests with a disarm check (V640). A SourceBuffer append, remove or decode error and a stall give-up rebuild at the play head; a playlist error during a rebuild is still fatal. The 10 s refill distance is provisional until the slice 0 baseline (T031-03).

## Slice 4 - Adapt (V644-V649)

- [x] T031-19 Server: `abr=1` on the browser master lists the H.264 fMP4 ladder (`h264cmaf/{rung}/main.m3u8`, per-rung init and segments, encoder caps from `Variant`), all rungs on `SegmentGrid` and bound to the one audio group; without `abr=1` the master is byte-identical to today. Segment metrics carry the rung (V644).
  - Rungs cap the bitrate only and keep the source resolution, as the mpegts ladder does (`RESOLUTION` is advertised scaled). Cold-start prewarm still warms only the source rung.
- [x] T031-20 `pharos-hls` bandwidth estimator: fast and slow EWMAs over first-to-last-byte samples, minimum-sample guard.
- [x] T031-21 `pharos-hls` ABR rule: ceiling, buffer-aware down-switch, cold-encode-aware up-switch, up-switch interval (V645-V647, property tests with a fake clock).
- [x] T031-22 Adapter: rung switch on the video `SourceBuffer` (init then segment, `changeType` when codecs differ), the engine asks for `abr=1`, UI auto quality off while the engine adapts, Settings ceiling passed in live (V648).
  - The Settings maximum caps the server's ladder (V645) and applies at the next negotiation, as under hls.js; it is not pushed into a playing engine. Codecs are the same on every rung, so no `changeType` is needed.
- [ ] T031-23 Switch log line and client event with a disarm check (V649); throttled-link e2e in Chromium and Firefox.
  - Switch log line, `qualitySwitch` event and throttled-link e2e (Chromium and Firefox) are in with T031-22; open: a disarm run of the e2e against the log line.

## Telemetry (V690-V694, 016 US4)

- [x] T031-40 `native/hls_trace.rs`: pure start, seek, stall and window trackers fed by `<video>` events and appended segments; `playbackStart` and `playbackSummary` appended to the 016 event catalogue (V690-V694).
- [x] T031-41 Adapter hooks: `<video>` `playing`/`seeked`/`pause`/`error` listeners, appended segments, ABR switches, `QuotaExceededError` reported as a `playbackError` (`type=quotaExceeded`), and the closing summary on drop.
- [x] T031-42 e2e `hls-telemetry.spec.ts`: start, seek to unbuffered time and the closing summary reach the console and `pharos_client_events_total`.
- [ ] T031-43 Count segment retries and deadline misses in the summary once T031-14 lands (its retry path owns `on_fetch_failed`).
- [ ] T031-44 The same start, seek and stall reports for hls.js, so slice 0 has a baseline in the same shape.
- [ ] T031-45 Server: `pharos_ui_playback_stall_seconds` and seek-latency histograms from these events (016 contract), and a `traceparent` on segment requests so a slow seek's fetch joins the server's segment span.

## Slice 5 - Measure and flip

- [ ] T031-17 Comparison harness in CI on the corpus streams, both engines. (2026-11-10: `.github/workflows/hls-bench.yml` runs both engines in Chromium and Firefox on the seeded `fixture-1.webm` and writes `tools/hls-bench-compare.cjs`'s table to the job summary, for batch PRs and main pushes touching the engine; the corpus streams wait on T031-01; open)
- [ ] T031-18 Device traces against the slice 0 baseline; a separate ADR or ADR-0071 acceptance to flip the default.
