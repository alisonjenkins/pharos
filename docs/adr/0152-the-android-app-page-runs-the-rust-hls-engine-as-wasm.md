# ADR-0152: The Android app's page runs the Rust HLS engine as wasm

- **Status:** Accepted
- **Date:** 2026-11-20T23:53:00Z
- **Deciders:** Alison Jenkins

## Context

The goal is one HLS engine, written in Rust, on every client, with hls.js
removed (spec 031, ADR-0071). The engine's adapter (`native/hls_web.rs`) drives
Media Source Extensions through `web-sys`. The Android TV and phone apps
(spec 025, ADR-0057; spec 044) run pharos-ui's Rust natively and show their
screens in the system WebView. `MediaSource`, the `SourceBuffer`s and the
`<video>` element exist only in the page, so the native Rust cannot run the
adapter. Android therefore played every HLS stream with hls.js, whichever
engine was asked for (B1366, V1336), and hls.js could not be removed.

The planner in `pharos-hls` is pure and could run natively, but every segment
would then cross the native/WebView bridge as an `eval` (base64 text), and
every buffer read would be a round trip. The TV's CPU and RAM cannot spare
that.

## Decision

We will build the engine to its own wasm module and run it inside the app's
page, as ADR-0104 does for the visualiser:

- A `pharos-tv-hls` bin (`--features web`, wasm32) exports
  `start(video_id, url, start_ms, device, token, send)`. It calls
  `native::hls_page::start`, which attaches the browser build's own engine.
  Freeing the returned object ends the play.
- `scripts/tv-hls.sh` builds it with `wasm-bindgen --target no-modules` into
  `crates/pharos-ui/assets/generated/` (gitignored). Both APK scripts run it.
  The Android build `include_str!`s the glue and the base64 wasm, and the page
  loads the module once, on the first play that uses it.
- The glue runs inside a function: it declares a top-level `wasm_bindgen`, as
  the visualiser's glue does, and two such scripts cannot share a page's
  global scope.
- The app outside the page keeps the log, the telemetry and the player's
  state. The module sends every log line, client event, segment load and
  fatal failure through the existing bridge as an `engine` message
  (`tv_page::EngineEvent`); the app writes the line to its log and submits the
  event under its own route and opt-in.
- The app passes the device class (buffer sizes) and the token. The module
  sends the token as `X-Emby-Token` on requests whose address names no play
  session, the rule hls.js had there, and loads straight from the server.
- `hls_engine_used` no longer replaces the Rust engine on Android. hls.js
  stays the default there until the default switch (ADR-0151).

## Consequences

- One engine and one codebase for the browser, the TV and the phone. hls.js
  can be removed once the default is switched.
- The APK carries a second wasm module, and the Android build needs a second
  wasm build step; without its output the Android compile fails on the
  missing `include_str!` file. `just tv-check` and `just phone-check` use
  empty stand-ins.
- The module's size is paid in the APK and, once loaded, in the WebView's
  memory. It is loaded only when a play uses the Rust engine.
- The page's log lines reach the app's log one bridge message each. The
  engine logs per segment and per decision, not per frame.
- The token is read when a play starts, as with hls.js: a play that outlives
  its token fails its next request without a play session in the address.
- Not yet measured on a TV or a phone. The device traces of T031-18 decide
  the default there.

## Alternatives considered

- Keep hls.js on Android only: two engines to maintain, and the JavaScript
  stays in the app.
- Run the planner natively and only the MSE calls in the page: every segment
  and buffer read crosses the bridge.
- One wasm module for the visualiser and the engine: smaller in total, but a
  music-only session would load the engine and a video-only session the
  visualiser, and the two change for unrelated reasons.

## References

Spec 031 (T031-46); V1503; ADR-0071 (the Rust engine), ADR-0104 (the
visualiser as wasm), ADR-0057 (the TV app), ADR-0058 (media tokens); B1366,
V1336.
