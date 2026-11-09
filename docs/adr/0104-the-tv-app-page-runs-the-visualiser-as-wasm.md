# ADR-0104: The Android TV app's page runs the music visualiser as wasm

- **Status:** Accepted
- **Date:** 2026-11-09T23:00:00Z
- **Deciders:** Alison Jenkins

## Context

The Android TV app (spec 025, ADR-0057) runs pharos-ui's Rust natively and
shows its screens in the system WebView. The music visualiser needs two
things only the page has: a WebGL2 canvas, and the sound, through a Web Audio
analyser on the `<audio>` element. Its loop (`native::listen::draw`) asks for
an analyser read and a draw on every animation frame. On the TV the
platform's `audio_tap` and `animation_frame` were stubs, so the music screen
drew nothing. Sending 60 frames a second of spectrum data and draw calls
across the native/WebView bridge as `eval` round trips would cost more than
the TV's CPU can spare.

## Decision

We will build pharos-ui's own visualiser loop to wasm and run it inside the
TV app's page:

- A `pharos-tv-visualiser` bin (`--features web`, wasm32) exports
  `start(style, on_fallback)` and `PageVisualiser::stop`. It calls
  `native::listen::run_page_visualiser`, which is the browser build's
  `render::open` + `draw` with its Web Audio tap.
- `scripts/tv-visualiser.sh` builds it with `wasm-bindgen --target
  no-modules` into `crates/pharos-ui/assets/generated/` (gitignored), and
  `scripts/android-apk.sh` runs it first. The TV build `include_str!`s the
  glue and the base64 wasm (~270 KB), and the page loads it once.
- On Android the `Visualiser` component starts a page run per style and stops
  it on unmount. A fallback comes back as a `visualiser` bridge message.
- A song's `<audio>` loads with `crossOrigin = "anonymous"` (the server's CORS
  allows any origin), and `audio_tap` accepts a CORS-fetched cross-origin
  source.

## Consequences

- Every style matches the browser, with one codebase for both, all in Rust.
- The TV build now needs a wasm build step before `dx build`. Without its
  output, the Android compile fails on the missing `include_str!` file.
- The APK grows by about 400 KB (glue plus base64).
- A song loads with CORS on the TV. If the server ever stops sending CORS
  headers on `/audio/`, songs stop playing there, not just the visualiser.

## Alternatives considered

- A second copy of the visualiser written as page JavaScript: smaller, but it
  duplicates the analysis and must be kept in step by hand.
- Visualisers off on the TV.
- Per-frame bridge calls from native Rust: too slow for the TV.

## References

B1032, V992; spec 025; ADR-0039 (the GPU layer), ADR-0057 (the TV app),
ADR-0058 (media tokens).
