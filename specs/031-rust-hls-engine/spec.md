# Feature specification: a Rust HLS engine for the web UI

**Feature Branch**: `feat/rust-hls-engine`
**Created**: 2026-11-07T23:10:00Z
**Status**: Slices 1-4 and telemetry built on main (opt-in per-device engine, hls.js stays default; #951, #960, #966, #969, #973, #975, #977, #988); open: slice 0 baseline (T031-01..03), T031-12 wasm size CI, T031-13 e2e, T031-16 MediaSource rebuild for the Rust engine, T031-23 disarm, T031-43..45, slice 5 flip (ADR-0071 still Proposed). Audited 2026-11-09
**Scope**: The native web UI (`/ui/`, 013/017) and the Android TV WebView (025) play HLS through a Pharos-owned engine: a pure-Rust crate, `pharos-hls`, driven by a Media Source Extensions adapter in `pharos-ui`. It sits behind the existing `PlaybackEngine` trait and a per-device setting; the vendored hls.js (ADR-0035) stays the default and the fallback until this spec's parity table and budgets are met. Safari's native HLS, direct play and jellyfin-web are unchanged.
**Decision record**: ADR-0071. **Bugs reserved**: B680-B699, B751-B760 (slice 3 recovery). **Verification**: V630-V649, V701-V710 (slice 3 recovery), V1060-V1069 (slice 3 MediaSource rebuild).

---

## Problem statement

The web UI plays HLS through hls.js, a 190 KB gzip script we vendor and cannot tune where it matters.

- Its buffering, retry and recovery behaviour are hls.js defaults. Nobody chose them for a Quest, a Steam Frame or the TCL G08 TV, and the UI cannot see them: the only signals are `hlsError` and `hlsFragLoaded` (`native/web.rs` `HlsJs`).
- Most of hls.js does no work here. Browsers get all-fMP4 masters with one video rung and one demuxed Opus audio group (`hls.rs` `master_playlist`, `push_h264_cmaf_rungs`, `push_vp9_rungs`), so its MPEG-TS transmuxer and multi-rung ABR run only for audio-only items. Quality changes are instead whole reloads made by the UI (`app.rs` auto quality), which restart the stream at a lower cap.
- None of it is reusable by the Android shell or a later native-video client.

Assumption, stated because the request named a solution: the goal is control and observability of playback on weak devices, and a smaller download, with no regression in start time, seeking or recovery. If hls.js turns out cheaper on those devices than this engine (slice 0 baseline against slice 3), the engine stays opt-in and this spec says so.

## How the UI uses hls.js today

| Feature | Where it lives today | Owner | In engine scope |
|---|---|---|---|
| Load master + media playlists | hls.js | engine | yes, slice 2 |
| Start at a position (`startPosition`) | `web.rs` `HlsJs::attach` | engine | yes, slice 2 |
| Append init + media segments, A/V in step | hls.js | engine | yes, slice 2 |
| Seek into unbuffered time | hls.js | engine | yes, slice 2 |
| Forward buffer target and back-buffer eviction | hls.js defaults | engine | yes, slice 2, tuned per device class |
| Small-gap jumping and stall nudges | hls.js | engine | yes, slice 3 |
| Fragment and playlist retries with backoff | hls.js | engine | yes, slice 3 |
| Fatal error recovery (`startLoad`, `recoverMediaError`) | `playback.rs` `RecoveryBudget` | UI + engine | yes, slice 3 (same budget, engine error vocabulary) |
| Fragment download stats for auto quality | `web.rs` `fragment_load`, `app.rs` auto quality | UI, fed by engine | yes, slice 2 (`OnFragment`) |
| Quality change | `app.rs` auto quality reloads with a cap | engine | yes, slice 4: switching between rungs inside the player, no reload (user story 4) |
| Audio track change | reload with `AudioStreamIndex` | UI | unchanged (reload) |
| Subtitles | `pharos-subs` / JASSUB; server sends no `TYPE=SUBTITLES` (`hls.rs` master comment) | UI | out of scope |
| Trickplay | image tiles (`trail.rs`, `watch_view.rs`) | UI | out of scope |
| SyncPlay rate control | `video.playbackRate` (`web.rs`, `together.rs`) | UI | unchanged; engine must not reset the rate |
| Multi-rung H.264 fMP4 ladder for browsers | not sent; browsers get one rung | server | yes, slice 4, only when the engine asks for it |
| MPEG-TS demux (audio-only AAC ladder) | hls.js | engine | open question 1 |

## Hard requirement: runs well on weak devices

The reference devices are a Quest headset (Quest Browser), a Steam Frame (its browser, assumed Chromium-based, to be confirmed) and the TCL G08 TV (Android WebView, armv7). Slice 0 measures hls.js on each; the engine must match or beat that baseline before the default flips.

| Budget | Value | How measured |
|---|---|---|
| Added wasm | <= 60 KB gzip over today's UI wasm | CI size check |
| Main-thread time per segment (fetch callback + append scheduling) | p95 <= 2 ms on the G08 | performance trace, engine counters |
| Long tasks caused by the engine | 0 | performance trace during a 10 min play |
| Time to first frame | <= the hls.js baseline + 10% | e2e timing, Chromium and Firefox; device traces |
| Seek to unbuffered time, to playing | <= the hls.js baseline + 10% | e2e timing |
| Buffered media held | forward 30 s / back 15 s by default, smaller on the "low" device class; never more than the MSE quota | buffered ranges sampled in e2e |
| Requests in flight | at most one per track | engine counters |

Design rules:

1. **No transmux.** Segments are appended as the server sent them. Anything that is not fMP4 is refused with a named reason and the player falls back to hls.js.
2. **Pure core, thin shell.** Every decision (what to fetch, when to retry, what to evict, which error is fatal) is a pure function in `pharos-hls` with native unit and property tests. The adapter only performs I/O and reports results back.
3. **One request per track.** Audio and video each have at most one segment in flight; the planner keeps them within one segment of each other.
4. **Bounded buffer.** Evict behind the play head beyond the back-buffer target; on `QuotaExceededError`, evict and lower the forward target for the session instead of failing.
5. **Abort on seek.** A seek outside the buffer aborts in-flight requests and restarts the planner at the segment holding the target time.

## User story 1 - Choose the engine per device (Priority: P1)

A tester opens Settings, Playback, and picks "Pharos engine (experimental)" on one device. Videos on that device play through the Rust engine; other devices keep hls.js.

**Acceptance**: the choice is stored per device (`localStorage`), `?engine=rust` and `?engine=hlsjs` override it for one page load, and the player logs `pharos: playback engine=<rust|hlsjs> reason=<setting|override|default|fallback>` and reports it as a client event (V630).

## User story 2 - A video plays, seeks and resumes on the Rust engine (Priority: P1)

A viewer with the Rust engine selected opens a film at a resume point, watches, seeks forward past the buffer and back, changes audio track and lets auto quality step down.

**Acceptance**: playback starts at the resume point, both seeks land within one segment of the target and play, the audio change and quality change reload at the position as they do on hls.js, and SyncPlay keeps its rate (V631-V636).

## User story 3 - Failures recover or fall back with a reason (Priority: P1)

A segment request fails, a segment is corrupt, or the browser cannot play the stream's codecs.

**Acceptance**: a failed segment is retried with backoff and playback continues; a media error rebuilds the MediaSource at the position under the same `RecoveryBudget` limits; an unsupported stream (TS, encrypted, live, unknown codec) falls back to hls.js before the first append with `reason=<the named cause>` logged and reported. Every failure path logs and reports what its success path does (V637-V641).

## User story 4 - Quality follows the connection without a reload (Priority: P1)

A viewer on the Rust engine watches over a link that drops from fast to slow and back. The picture steps down a rung before the buffer runs dry and steps back up when the link recovers, with no reload, no black frame and no jump in position. A maximum quality picked in Settings is a ceiling the engine never goes above.

**Acceptance**: on a throttled link (e2e, Chromium and Firefox) the engine switches rung at a segment boundary, playback never stalls for a down-switch that the buffer could have covered, and each switch is logged and reported with its inputs (V644-V649).

## Requirements

- **FR-001** `pharos-hls` has no browser, wasm-bindgen or async-runtime dependency, and builds for the host and `wasm32-unknown-unknown`.
- **FR-002** The parser accepts the tags Pharos emits (`EXTM3U`, `EXT-X-VERSION`, `EXT-X-INDEPENDENT-SEGMENTS`, `EXT-X-STREAM-INF`, `EXT-X-MEDIA`, `EXT-X-TARGETDURATION`, `EXT-X-PLAYLIST-TYPE`, `EXT-X-MEDIA-SEQUENCE`, `EXT-X-MAP`, `EXTINF`, `EXT-X-ENDLIST`), ignores unknown tags, and reports `EXT-X-KEY`, a missing `EXT-X-ENDLIST`, byte ranges and discontinuities as unsupported features by name.
- **FR-003** Relative URIs resolve against the playlist URL, and the query string carrying `PlaySessionId` and `api_key` is kept on every request, as the server's playlists expect.
- **FR-004** The adapter derives each `SourceBuffer` MIME type from the master's `CODECS` split by track, checked with `MediaSource.isTypeSupported` before any fetch; a codec the browser refuses is a fallback, not a fatal error.
- **FR-005** Segment requests time out after a deadline sized for on-demand transcodes (the server may take seconds to produce a segment), configurable per device class; the defaults come from slice 0 measurements, not hls.js.
- **FR-006** Errors are reported in the vocabulary `RecoveryBudget` already uses (`networkError`, `mediaError`, plus engine `details` strings), with `engine=rust` in the client event, so existing dashboards and the recovery budget work unchanged.
- **FR-007** The engine emits `OnFragment` for each video segment with the same byte and timing semantics as `fragment_load` (first byte to last byte), so auto quality judges both engines alike.
- **FR-009** Quality switching happens in the engine. For the Rust engine the server's browser master lists an H.264 fMP4 ladder (the source rung plus `Variant::ladder_for`, capped by the session bitrate as the mpegts ladder is), each rung with its own media playlist, init segment and segments on the shared `SegmentGrid`, so every rung's segment boundaries line up. The ladder is sent only when the request asks for it (`abr=1`); hls.js and other clients keep today's single-rung master. All rungs share the one demuxed audio group, so a switch never touches audio.
- **FR-010** A switch appends the new rung's init segment and then its next segment on the same video `SourceBuffer`, calling `changeType` first when the codec string differs. No reload, no position change.
- **FR-011** The ABR rule lives in `pharos-hls` as a pure function of: a bandwidth estimate (fast and slow EWMAs of first-to-last-byte throughput, the lower of the two), the forward buffer, the rung ladder, the user's maximum quality, and the time the server takes to produce a cold segment. Rungs are encoded on demand, so the first segment after a switch can be slow: an up-switch needs a buffer margin that covers a cold encode, and a down-switch is taken early enough to arrive before the buffer empties. Switches are rate-limited (no more than one up-switch per 10 s by default) so the picture does not oscillate.
- **FR-012** While the engine adapts, the UI's reload-based auto quality is off for that play; a maximum quality change from Settings is passed to the engine as a new ceiling without a reload.
- **FR-013** Every switch logs `pharos: abr switch from=<rung> to=<rung> reason=<bandwidth|buffer|ceiling|start> est_kbps=… buffer_ms=… cold_ms=…` and reports it as a client event; the server's existing segment metrics carry the rung, so switches can be seen from both ends.
- **FR-008** Dropping the engine aborts every request, removes every listener and ends the MediaSource; nothing keeps running after the player closes (V642).

## Slices

0. **Baseline**: measure hls.js on Chromium, Firefox, Quest, Steam Frame and the G08: first frame, seek, main-thread time, long tasks, memory, buffered ranges. A Playwright harness that runs both engines on the same streams with audio muted, and a corpus of real server playlists and segments.
1. **Seam**: the engine setting and override, `Engine::RustHls` routing to hls.js until the engine exists, engine-neutral telemetry labels, and auto quality gated on "engine reports fragments" instead of `Engine::HlsJs`.
2. **Play**: `pharos-hls` parser, timeline and planner; the MSE adapter; start position, seek, eviction, `OnFragment`, fallback for unsupported streams.
3. **Recover**: retries, gap jumping, stall nudges, MediaSource rebuild, `QuotaExceededError` handling, recovery budget.
4. **Adapt**: the server's opt-in H.264 fMP4 ladder for browsers (FR-009), rung switching in the adapter (FR-010), the ABR rule and its estimator (FR-011), the ceiling and auto-quality hand-over (FR-012), switch telemetry (FR-013).
5. **Measure and flip**: the comparison harness in CI, device traces against slice 0, then a separate decision to make the Rust engine the default. The default does not flip without slice 4.
6. **Later, if needed**: TS audio (open question 1), a VP9 ladder, Web Worker placement.

## Verification

| Id | Statement | Guard |
|---|---|---|
| V630 | The engine is chosen by override, then device setting, then default, and the choice and its reason are logged and reported | unit test of the choice function; playback log test |
| V631 | Pharos's own master and media playlists parse to the same timeline the server built (segment count, durations, init URI, query kept) | `pharos-hls` tests fed by playlists the server's handlers generate |
| V632 | A play head time maps to the segment that holds it, for every time in the playlist | property test over generated timelines |
| V633 | The planner never has more than one request in flight per track, and keeps audio and video within one segment of each other | property test of the planner state machine |
| V634 | Playback starts at `start_ms` within one segment duration | e2e, Chromium and Firefox |
| V635 | A seek outside the buffer aborts in-flight requests and the next request is the segment holding the target | planner unit test; e2e |
| V636 | Buffered media stays within the forward and back targets, and a `QuotaExceededError` lowers the forward target instead of failing | planner unit test; e2e buffered-range sampling |
| V637 | A failed segment request is retried with backoff up to a limit, then reported as a fatal `networkError` | planner unit test with a fake clock |
| V638 | A media error rebuilds the MediaSource at the current position, within `RecoveryBudget` | adapter e2e with a corrupt segment |
| V639 | A stream the engine cannot play falls back to hls.js before the first append, with the named reason logged and reported | unit tests per unsupported feature; e2e |
| V640 | Every engine failure path logs and reports the fields its success path records, with `engine=rust` | log/event tests with a disarm check |
| V641 | `OnFragment` reports the same bytes and first-to-last-byte time as `fragment_load` does for hls.js on the same segment | adapter test against recorded timings |
| V642 | Dropping the engine leaves no request, listener or timer running | e2e: close the player, assert no further segment requests |
| V643 | The added wasm stays within 60 KB gzip | CI size check |
| V644 | A browser master asked for with `abr=1` lists the H.264 fMP4 ladder, every rung sharing the audio group and the same segment boundaries; without it the master is unchanged | server handler tests |
| V645 | The ABR rule never picks a rung above the user's maximum quality or the session cap | property test of the rule |
| V646 | An up-switch is taken only with a forward buffer that covers a cold segment encode, and at most once per up-switch interval | property test of the rule with a fake clock |
| V647 | A down-switch is taken while the forward buffer still covers the next segment's download at the estimated rate | property test of the rule |
| V648 | A rung switch appends the new init then the next segment, with no reload and no change in `currentTime` | adapter e2e on a throttled link |
| V649 | Every switch is logged and reported with from, to, reason, estimate, buffer and cold-encode time | log/event test with a disarm check |
| V690 | Every engine telemetry report (start, seek, stall, summary) logs a `pharos: playback <what> engine=rust` line carrying exactly the fields of its client event | `hls_trace` unit test |
| V691 | Time from attach to the first `playing` is reported once per engine as `playbackStart`, with the first video segment's index, time to first byte, load time and append time; a player closed first reports `end=abandoned` | `hls_trace` unit tests; e2e `hls-telemetry.spec.ts` |
| V692 | A `playbackSummary` (segments, bytes, kbps, ttfb p50/max, load max, forward buffer min/mean while playing, stalls, seeks, switches, quota hits) goes out every 5 minutes of segments and when the engine is dropped, falls back or fails | `hls_trace` unit tests; e2e |
| V693 | A stall is `waiting` while playing, after the start and outside a seek, until `playing` or the head moves; it is reported with its duration, buffered ranges, network state, forward buffer and the last video download | `hls_trace` unit tests with a disarm check |
| V694 | A seek runs from `seeking` to playing (or to `seeked` when paused or able to play on), names where it came from, whether the target was buffered, the scrub it replaced, and the first video segment after it (download vs append split); a seek still open when the engine goes reports `end=abandoned` | `hls_trace` unit tests with a disarm check; e2e |
| V701 | A play head stuck at a buffer hole of at most 1 s is moved past it; one stuck with data ahead is nudged up to three times, then reported as a `mediaError` `bufferStalledError`, which rebuilds the MediaSource (V638); a hole wider than 1 s is left to the planner | watchdog unit tests on a fake clock; e2e with a cut audio range |
| V702 | Every stall verdict (jump, nudge, starved, give up, recovered) is logged and reported as `playbackStall` with the play head, target, gap, attempt, stall time, each track's buffer ahead and the requests in flight | log/event test with a disarm check |
| V1062 | A media error rebuilds the MediaSource at most three times; the budget refills, and the recovery is reported, only once the play head is 10 s past the last rebuild, an error from a source already replaced is ignored, and a codec the browser refuses fails without a rebuild | `MediaRecovery` unit tests; e2e with a segment that never parses |

## Open questions

1. **Audio-only TS.** Audio-only items still get an MPEG-TS AAC ladder. Either the server sends web clients fMP4 audio (one server change, no client demuxer), or `pharos-hls` gains an ADTS-to-fMP4 remux. Leaning to the server change; decide before slice 6.
2. **Steam Frame browser.** Which engine its browser uses and whether it has MSE; confirm on the device in slice 0.
3. **Device classes.** Which signal picks the "low" buffer targets (`navigator.deviceMemory`, the 025 TV build, a user setting). Decide in slice 2 from slice 0 numbers.
4. **Worker placement.** Only if slice 0 or slice 3 traces show the engine causing long tasks.
5. **VP9 ladder.** VP9 is CPU-only on the current host; a VP9 ladder multiplies that cost. Browsers that take H.264 get the ladder; VP9-only clients keep one rung until measured.
6. **Cold-encode estimate.** Whether the engine learns cold-segment time from its own first fetches or the server advertises it. Start with the engine's own measurement.
