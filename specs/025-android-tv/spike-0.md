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
Eval round-trip latency on the TV is not measured yet.

## Build

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

- Media auth: the spike puts the device token in `?api_key=` on every image
  and stream URL (the WebView origin `https://dioxus.index.html` has no
  cookie). Slice 1 needs a token-free route (wry custom-protocol proxy or a
  cookie set through JNI) so the token stays out of URLs and logs.
  Checked 2026-11-04: pharos and jellyfin-web logs show `api_key=REDACTED`,
  and the token's prefix matches nothing in Loki across every namespace.
  Logs outside Loki (e.g. a host-level reverse proxy) were not checked.
- QuickConnect needs the reconnect + re-mint loop above, or a server change
  that lets a pending QuickConnect socket outlive `auth_deadline`.
- Keep focus inside the Dioxus root at start (`root.focus()`); 011 spatial
  focus replaces the WebView's default navigation.
- Prefer hls.js over native HLS on Android (`plays_hls()` false).
- Report a playback grant's `stop` when the app goes to the background.
