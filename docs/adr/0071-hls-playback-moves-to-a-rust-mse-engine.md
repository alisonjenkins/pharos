# ADR-0071: HLS playback in the web UI moves to a Rust MSE engine, hls.js stays the default until parity

- **Status:** Proposed
- **Date:** 2026-11-07T23:10:00Z
- **Deciders:** Alison Jenkins

## Context

The native web UI plays every HLS stream outside Safari through a vendored
hls.js (ADR-0035): 624 KB minified, 190 KB gzip, loaded as a second script
beside the Dioxus wasm. The UI drives very little of it (`native/web.rs`
`HlsJs`): `startPosition`, `loadSource`, `attachMedia`, the `hlsError` and
`hlsFragLoaded` events, and `startLoad` / `recoverMediaError` for recovery.
Everything else hls.js does is implicit: segment scheduling, forward and back
buffer limits, seeking into unbuffered time, gap and stall handling, fragment
retries, MediaSource lifecycle.

What the server sends a browser is narrow. Every video master for a
`Mozilla` user agent is all-fMP4 (`api/jellyfin/hls.rs` `master_playlist`):
one video rung (H.264 CMAF from `h264cmaf/`, or VP9 from `vp9/`) bound to one
demuxed Opus `EXT-X-MEDIA:TYPE=AUDIO` group, VOD playlists, `EXT-X-MAP` init
segments. No subtitle renditions (our own renderer draws them, ADR-0064). No
I-frame playlists (trickplay is image tiles). Quality and audio-track changes
are reloads the UI makes (`app.rs` auto quality, a new `AudioStreamIndex`),
and SyncPlay rate control is `video.playbackRate`. Only audio-only items
still get an MPEG-TS AAC ladder.

So hls.js's largest parts, the MPEG-TS transmuxer and its in-player ABR across
rungs, do no work for video in this UI. What we cannot do today is see or tune
the buffer and retry behaviour on the weak devices Pharos targets (Quest,
Steam Frame, the TCL G08 TV), where hls.js's defaults were never chosen for
us, nor reuse the player logic in the Android or later native clients.

## Decision

We will write our own HLS engine as a new pure-Rust crate, `pharos-hls`,
driven by a thin MSE adapter in `pharos-ui`, as specified in
`specs/031-rust-hls-engine/spec.md`.

- `pharos-hls` is pure logic with no browser types: m3u8 parsing, the segment
  timeline, a buffer planner (what to fetch next, what to evict), fMP4 box
  inspection (codec strings, `tfdt` timing checks), retry and recovery state,
  and later a bandwidth estimator. It builds and tests natively.
- The adapter is a new `PlaybackEngine` (`Engine::RustHls`) in
  `native/web.rs`: `fetch` with `AbortController`, one `MediaSource`, one
  `SourceBuffer` per track, an append queue. It compiles into the existing UI
  wasm; there is no second download.
- fMP4 passthrough only. The engine never transmuxes; it appends the server's
  segments as they arrive, on the main thread, as hls.js already does for its
  appends. MPEG-TS input (audio-only items) stays on hls.js until a later
  decision (spec open question 1).
- A per-device setting picks the engine; hls.js remains the default and the
  fallback (also when the Rust engine fails to start) until the spec's
  parity table and weak-device budgets are met. Flipping the default is a
  separate change with its own measurements.
- Quality switching lives in the engine, not in UI reloads. When the Rust
  engine asks (`abr=1`), the server's browser master lists an H.264 fMP4
  ladder on the shared segment grid, one rung per `Variant` up to the source
  and the session cap, all bound to the one demuxed audio group. The engine
  switches rung at a segment boundary on the same `SourceBuffer`, by a pure
  ABR rule in `pharos-hls` that knows rungs are encoded on demand (an
  up-switch costs a cold segment encode). hls.js clients keep the single-rung
  master.
- The engine reports through the same paths hls.js does (`OnFatal`,
  `OnFragment`, `playbackError` client events), with `engine=rust`.

## Consequences

- When the default flips, the 190 KB gzip hls.js download leaves the
  player's critical path; the engine is budgeted at 60 KB gzip of added wasm.
- Buffer sizes, retry timing and recovery are ours, chosen per device class
  and visible in telemetry.
- We own an HLS client. Its behaviour is narrow on purpose: it plays the
  playlists Pharos emits, and rejects anything else with a named reason
  rather than guessing (live playlists, TS and encryption fall back to
  hls.js).
- The fixes hls.js makes quietly (small buffered gaps, `tfdt` drift between
  audio and video, stalls at segment boundaries) must be rediscovered, so
  each slice is checked against hls.js on the same streams before merging.
- More code to maintain than a vendored script: a parser, a scheduler, an
  ABR rule and an MSE driver, plus their tests and a comparison harness.
- A viewer whose link drops gets a lower rung within a segment or two
  instead of a stall and a reload. The server encodes a rung only when it
  is fetched, so a switch spends one cold encode; H.264 rungs can use the
  GPU (spec 003), VP9 stays one rung.

## Alternatives considered

- **Keep hls.js.** No new code, but no control over buffering on weak
  devices, a 190 KB second download, and nothing reusable outside a browser.
  Remains the fallback.
- **Port hls.js whole, including the TS transmuxer.** The server already
  sends fMP4 to browsers, so the transmuxer would run on nothing but the
  audio-only ladder. Deferred until a measured need.
- **Keep quality changes as UI reloads.** No server change, but every change
  restarts the stream and shows a gap; rejected by Alison (2026-11-07).
- **Run the engine in a Web Worker with MSE-in-Worker.** Not available in
  every target browser, and with no transmux the main-thread cost is an
  append call per segment. Revisit if the slice 0 baseline shows main-thread
  long tasks.
- **Use shaka-player or another JS player.** Swaps one opaque dependency for
  a larger one.

## References

- `specs/031-rust-hls-engine/spec.md`
- ADR-0035 (vendored hls.js), ADR-0064 (subtitles drawn by our own renderer)
- `crates/pharos-ui/src/native/web.rs` (`HlsJs`, `engine`),
  `crates/pharos-ui/src/native/playback.rs` (`PlaybackEngine`,
  `choose_engine`, `RecoveryBudget`)
- `crates/pharos-server/src/api/jellyfin/hls.rs` (`master_playlist`,
  `push_h264_cmaf_rungs`, `is_web_client`)
