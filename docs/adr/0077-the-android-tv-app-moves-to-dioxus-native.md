# ADR-0077: The Android TV app moves to Dioxus Native (Blitz), with a Rust player on MediaCodec

- **Status:** Proposed
- **Date:** 2026-11-08T13:30:00Z
- **Deciders:** Alison
- **Supersedes:** ADR-0061 in part (its renderer and playback choices; the Rust-only rule, the shared `pharos-ui` components, device-token sign-in and signing outside Nix stay)

## Context

ADR-0061 put the TV app on Dioxus's webview renderer: `pharos-ui` runs as
native Rust and the system WebView draws its DOM and plays video through
`<video>` and hls.js. It rejected Dioxus Native (Blitz) because Blitz has no
video element, so the player would have to be built from scratch.

Since then:

- The WebView boundary is where the TV app's slowness lives. Every DOM read
  is an async `document::eval` (2.5 ms p50 on the TV, spec 025 spike-0), and
  the stabilisation work in PR #1002 keeps finding presses and clock ticks
  that wait on it.
- The G08 TV has 2.4 GB of RAM and swaps with the WebView app running.
- The WebView's media stack refuses AC-3/E-AC-3 through MSE and does not
  report an HDR output.
- Most of the player now exists in Rust: `pharos-hls` plans fetches,
  retries and buffers as pure functions and inspects fMP4 (ADR-0071), and
  `pharos-subs` parses and rasterises subtitles (spec 026).

Alison chose Dioxus Native for the long-term TV app on 2026-11-08 and is open
to contributing upstream where it is still experimental.

## Decision

We will build a second Android TV app from the same `pharos-ui` components
with the Dioxus Native renderer (Blitz) instead of the WebView:

- A `native/blitz.rs` platform module, selected by a `native` feature on
  Android, reads the Blitz document directly. It reuses `native/android.rs`
  for the socket, QuickConnect and token storage.
- Video is decoded by the NDK `AMediaCodec` into an `ASurfaceControl` layer
  under the UI window, fed with fMP4 samples planned by `pharos-hls`.
  Subtitles come from `pharos-subs`. Audio is decoded to PCM and played
  through AAudio.
- The renderer backend is picked by the spike on the G08 (spec 032, renderer
  matrix). The workspace stays on Dioxus 0.7 unless a later ADR moves it.
- It ships under its own application id beside the WebView app, which stays
  the default until spec 032's parity gate passes.
- Still no hand-written Kotlin or Java (ADR-0061).

Details: `specs/032-blitz-tv/spec.md`.

## Consequences

- DOM reads become function calls. `android_page.rs`'s JS state mirror and
  batched evals are not needed in this build.
- No Chromium in the TV process, so less memory and no browser compositor.
- We own the player: start, seek, recovery, A/V sync, track switching and
  codec capabilities move from the browser to Rust. That is the largest cost.
- HEVC, HDR output and surround passthrough become reachable, since the
  decoder and the display layer are ours. Passthrough still needs an encoded
  `AudioTrack`, which has no NDK API (spec 032, open question 3).
- Blitz supports a subset of CSS and is experimental; gaps are fixed upstream
  or styled around for TV. Upstream fixes may pull us onto Dioxus 0.8.
- Two TV builds exist until switch-over, so platform changes need checking on
  both.
- A slice 0 spike on the G08 gates all of this with go/no-go numbers fixed in
  the spec before it runs.

## Alternatives considered

- **Keep the webview renderer and keep optimising (ADR-0061).** Cheapest
  next step and it stays the fallback, but the eval boundary and the
  WebView's memory and codec limits stay.
- **A thin WebView shell that loads the WASM web build.** Removes the eval
  boundary (Rust and DOM share the page) and keeps the browser player, but
  keeps the WebView's memory and codec limits and needs a native shell.
  Not chosen by Alison.
- **Slint.** Mature on Android, but every screen rewritten outside
  `pharos-ui`, so a second UI to keep in step with the web.
- **Kotlin / Compose for TV + Media3.** Rejected again for the reason in
  ADR-0061: no Java or Kotlin stack.

## References

- `specs/032-blitz-tv/spec.md`, `specs/025-android-tv/spike-0.md`
- ADR-0061 (superseded in part), ADR-0071 (`pharos-hls`), ADR-0028 (native API)
- Blitz: <https://github.com/DioxusLabs/blitz>
