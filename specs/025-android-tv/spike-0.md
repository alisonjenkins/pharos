# 025 slice 0 spike — results (2026-11-04)

TV: TCL 55" C6K (2024 EU), Google TV, Android 12 (API 31), MediaTek MT9653.
`ro.product.cpu.abilist` = `armeabi-v7a,armeabi` (32-bit userland only).
System WebView 156.0.8078.47. Branch `spike/android-tv`, bin `pharos-tv`.

**Verdict: go for slice 1, qualified.** Every slice 0 check passed on the
TV. The spike is a stripped bin, though: it does not prove the real `App`
runs on Android. About 20 of `web.rs`'s 71 functions read the DOM
synchronously (`position_ms`, `media_state`, `element_size`, `viewport`,
`current_path`, `tile_in_view`, `prefers`, `capabilities`, `key_on_itself`,
`on_a_control`, ...). On the webview renderer the only DOM access is an
async `document::eval` round trip, and events arrive serialised with no
element behind them. So `android.rs` cannot copy that surface one to one:
slice 1 must pick an async surface or a JS-side state mirror pushed into
Rust. That choice is slice 1's main cost and bears on ADR-0061's premise.
Eval round-trip latency was measured on 2026-11-08 (below): reads are cheap
enough for an async surface, and a pushed mirror keeps up with every frame.

## Eval latency (2026-11-08)

Release build of the spike bin (`89d85aac0`), on the TV, nobody signed in,
three seconds after launch (`~/pharos-tv-bench/run.sh`, logcat `bench`):

| Probe | Result |
|---|---|
| `document::eval` round trip, one DOM read, 200 runs | min 2.1 ms, p50 2.5 ms, p95 3.3 ms, max 116 ms (the first, warm-up) |
| One eval reading ten values (viewport, scroll, focus, tile count, `<video>` time/paused/width, a rect, dpr), 50 runs | p50 2.6 ms, p95 3.8 ms |
| JS pushing a frame into Rust (`dioxus.send`) per `requestAnimationFrame`, 300 frames | gap p50 16.7 ms, p95 16.9 ms, max 17.8 ms; 5.0 s total |

So a round trip costs about 2.5 ms whatever it reads, and batching is free.
A pushed stream keeps the 60 Hz frame rate with no dropped frame. Neither
side is a bottleneck: a D-pad press can afford one batched read (under 4 ms
at p95 against a 16.7 ms frame), and continuous state can be pushed.

### During playback (2026-11-08T15:36Z)

Same bin with a second phase: after the idle reads it loops a muted 10 s
1080p30 H.264 clip (a blob URL, hardware decode, 1-3 frames dropped at
start, none after) and repeats the reads, then pushes 600 frames (~10 s, so
it crosses the clip's loop point). 500 single reads and 100 batched reads
per phase; five launches, ranges across them:

| Probe | Idle p50 / p95 / p99 | Playing p50 / p95 / p99 |
|---|---|---|
| One DOM read | 2.1-2.2 / 2.7-2.9 / 3.1-3.7 ms | 2.5-2.9 / 7.1-8.9 / 9.2-12.2 ms |
| One batched read (ten values) | 2.7-2.8 / 3.3-3.9 / 3.5-6.8 ms | 3.4-4.2 / 8.6-11.1 / 9.8-12.6 ms |
| JS push per frame (two launches) | gap 16.7 / 16.8-16.9 ms | gap 16.7 / 17.4-20.1 / 18.6-21.4 ms, max 24.7 ms |

Worst single read while playing was 53 ms (once in 2500); the 64-68 ms
idle max is the first call after launch. Playback leaves p50 alone and
triples the read tail: a read costs up to about 12 ms at p99. The push
keeps 60 Hz (600 frames in 10.0 s); 3-32 of 600 gaps went over 20 ms and
none reached two frames. One batched read per D-pad press still fits a
16.7 ms frame. Polling `<video>` state by eval on every tick would not
(several reads a tick at up to 12 ms each), so the JS mirror for state
that changes on its own holds up under playback too. The clip is not HLS
through hls.js/MSE, so a real stream adds demux work on the same thread;
treat these as a floor for playback.


- `nix develop .#android -c dx build --platform android --package pharos-ui
  --bin pharos-tv --features mobile --target armv7-linux-androideabi`.
- dx 0.7.10 generates and runs the Gradle project (AGP 8.7, compileSdk /
  targetSdk 34, minSdk 24); no Kotlin/Java written by hand.
- The whole `pharos-ui` lib compiles for Android unchanged (host.rs platform).
- APK: debug 44.4 MB (libmain.so 157 MB), release 18.5 MB (libmain.so
  33 MB), armeabi-v7a only. Signed with a dedicated `pharos-dev` key kept in
  `~/.config/pharos-android/` (outside repo and store).

## What worked

| Check | Result |
|---|---|
| Native socket | tokio-tungstenite + rustls/ring into `connect_stream`; no Origin header, server accepts |
| QuickConnect | initiate/poll over the native API, token in `/data/data/<pkg>/files`, `resume(token)` on relaunch |
| Home row | `home()` 376-562 ms; posters load as `<img>` with `?api_key=<token>` |
| Playback | 8 Mile, `audio_remux`, hls.js + MSE, hardware `c2.mtk.avc.decoder`, 1920x1080, 1 of 2419 frames dropped, muted; pauses when the app is hidden |
| D-pad / OK | WebView's own spatial navigation moves focus between `<button>`s; Dioxus `onkeydown` gets ArrowUp/Down/Left/Right, Enter |
| Media keys | `MediaPlayPause`, `MediaPlay`, `MediaFastForward` arrive as keydown |
| Manifest | `[application] android_manifest = "android/AndroidManifest.xml"` replaces dx's manifest; `aapt2 dump badging` shows `leanback-launchable-activity`, `banner=`, touchscreen and leanback `uses-feature-not-required`; app launches via `LEANBACK_LAUNCHER` |

## What broke, and the fix used

1. Server closes a socket not signed in after 30 s
   (`native api connection closed ... cause=auth_deadline`), so a
   QuickConnect poll on one socket dies. Fix: reconnect when the socket has
   closed. Reconnecting for every poll got `HTTP error: 429 Too Many
   Requests` on the `/pharos/rpc` upgrade.
2. A code nobody approves expires after about 10 minutes
   (`notFound: quick connect request not found`). Fix: mint a new one.
3. After the server closes, a call on that bootstrap hangs instead of
   failing; the driver now sets a closed flag and polls have a 10 s timeout.
4. The WebView answers `canPlayType('application/vnd.apple.mpegurl')` =
   `maybe`, so the web UI's `choose_engine` would pick native HLS. The spike
   forces hls.js.
5. Dioxus `onkeydown` fires only when focus is inside the Dioxus root; with
   `<body>` focused, nothing arrives. `code` is always empty (`Unidentified`),
   only `key` is usable.
6. Back is not a key event. The WebView runs `history.back()` (a `popstate`
   arrives); with no history left the activity goes to the background.
   This matches the web UI's History API routing.

## Manifest (dx 0.7.10 `Dioxus.toml`)

- `[android]` has `permissions`, `features` (always `required="true"`),
  `raw_manifest` (inserted **outside** `<application>`), `url_schemes`,
  `app_link_hosts`, `min_sdk`/`target_sdk`/`compile_sdk`, `app_theme`.
- No key for `LEANBACK_LAUNCHER` or `android:banner`: those need the
  `android_manifest` override. The spike's banner points at
  `@mipmap/ic_launcher`. Shipping a real 320x180 drawable through dx is not
  tested yet.

## Codecs (WebView 156, `MediaSource.isTypeSupported`)

| Codec | MSE | Notes |
|---|---|---|
| h264 (L4.0, L5.1) | yes | |
| hevc Main, Main10 L5.1 (`hvc1`, `hev1`) | yes | decodingInfo 4K PQ: supported, smooth |
| vp9 (profile 0, profile 2 10-bit) | yes | 4K PQ: supported, smooth |
| av1 8-bit / 10-bit | yes | decodingInfo 10-bit 4K PQ: **not** supported |
| Dolby Vision `dvh1.05` / `dvhe.05` | no | |
| aac, opus, flac, mp3 | yes | |
| ac3, eac3 | **no** | surround passthrough needs native playback (slice 3) |

HDR display: `(dynamic-range: high)`, `(video-dynamic-range: high)` and
`(color-gamut: rec2020)` are all false. The decoder accepts HEVC10/VP9 PQ,
but the WebView does not report an HDR output, so HDR through the WebView
is likely tone-mapped. That needs slice 3 (native surface) to verify.
CSS viewport is 960x540 at devicePixelRatio 2.

## Carry into slice 1

- Media auth (superseded: ADR-0058 gives native clients a short-lived
  `TokenScope::Media` token for `?api_key=`, so the full device token stays
  out of URLs, and ADR-0059 revokes it with its session; no token-free route
  is needed). As spiked: the device token went in `?api_key=` on every image
  and stream URL (the WebView origin `https://dioxus.index.html` has no
  cookie). Slice 1 puts the Media-scoped token in `?api_key=` instead, as
  ADR-0058 decides, so the full device token is not in those URLs.
  Checked 2026-11-04: pharos and jellyfin-web logs show `api_key=REDACTED`,
  and the token's prefix matches nothing in Loki across every namespace.
  Logs outside Loki (e.g. a host-level reverse proxy) were not checked.
- QuickConnect needs the reconnect + re-mint loop above, or a server change
  that lets a pending QuickConnect socket outlive `auth_deadline`.
- Keep focus inside the Dioxus root at start (`root.focus()`); 011 spatial
  focus replaces the WebView's default navigation.
- Prefer hls.js over native HLS on Android (`plays_hls()` false).
- Report a playback grant's `stop` when the app goes to the background.
