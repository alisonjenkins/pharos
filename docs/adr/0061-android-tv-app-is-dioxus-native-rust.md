# ADR-0061: The Android TV app is the Dioxus UI built for Android, in Rust only

- **Status:** Proposed. Superseded in part by ADR-0077 (renderer and playback) once that is accepted; this app stays the default until spec 032's parity gate passes.
- **Date:** 2026-11-04T22:20:00Z
- **Deciders:** Alison

## Context

Pharos plans its own TV and phone apps on the native API (ADR-0028). The
web UI is Dioxus compiled to WASM, with every browser call in one platform
module (`pharos-ui/src/native/web.rs`) and a host stand-in beside it. The
native API already has what a TV needs: device tokens, QuickConnect and a
`Capabilities` struct for codec negotiation.

## Decision

We will build the Android TV app from the same `pharos-ui` components using
Dioxus's Android target (`dx build --platform android --package pharos-ui`),
written only in Rust. A third platform module, `native/android.rs`, will
implement the same surface as `web.rs` with a native websocket into
`pharos_client::connect_stream`, device-token sign-in and app-private
storage. Slice 1 adds its socket and QuickConnect sign-in; the `platform`
alias still points at `host.rs` until the shared `App` runs on Android, so
that wiring is pending. Playback starts on the WebView's `<video>` + hls.js behind the
existing `PlaybackEngine` trait; native MediaCodec/ExoPlayer is reached later
through JNI from Rust behind the same trait. No Kotlin or Java is
hand-written. Signing happens outside Nix; the key never enters the store.
Details: `specs/025-android-tv/spec.md`.

## Consequences

- Screens, state reducers, theming and `pharos-client` are shared with the
  web UI; TV and web stay alike.
- The browser glue has to be written again for Android (~2.2k lines today),
  and every new browser call needs an Android twin.
- Dioxus on Android is younger than its web target and unproven on TV; a
  spike on real hardware gates the work.
- `android.rs` reads the DOM both ways, by how a value changes (decided
  2026-11-08 from the TV's eval latency, `specs/025-android-tv/spike-0.md`).
  State that changes on its own (`<video>` position and media state,
  viewport, page visibility, current path) is pushed by a JS mirror into
  Rust and read synchronously from the cache, so `web.rs`'s synchronous
  callers keep their signatures; the mirror can lag by one push. Reads made
  on an input (spatial focus, element sizes, tile in view) run as one
  batched `document::eval` per event. One round trip is 2.1 ms p50 / 2.8 ms
  p95 / 3.7 ms p99 idle and 2.7 ms / 8.9 ms / 12.2 ms p99 during muted
  1080p playback, so one batched read per key press fits a 16.7 ms frame,
  while polling by eval during playback does not. A fully async surface
  was rejected: every caller changes and it still pays a round trip per
  read.
- Until native playback lands, codec support is the WebView's (weak HEVC,
  HDR and surround).
- The Nix devShell gains the Android SDK/NDK.

## Alternatives considered

- **Kotlin / Compose for TV + Media3, Rust core via UniFFI**: best playback
  and focus handling out of the box, but every screen rewritten in Kotlin
  and a second UI to keep in step. Rejected by Alison: no Java/Kotlin stack.
- **Thin Kotlin shell loading the web bundle in a WebView**: reuses the WASM
  bundle unchanged, but keeps a Kotlin project. Rejected for the same reason.
- **Fully native Rust renderer (wgpu: Dioxus Native/Blitz, Slint, egui)**:
  no WebView, but no video element, so a player built from scratch.

## References

- `specs/025-android-tv/spec.md`, spec 011 (input), ADR-0028 (native API).
