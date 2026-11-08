# 025 — Android TV app (Dioxus, pure Rust)

- **Status:** draft. Slice 0 done (`spike-0.md`, go for slice 1, qualified); slice 1 in progress.
- **Date:** 2026-11-04
- **Asked by:** Alison, "I want to go the Rust route with Dioxus, it keeps it
  more like the Web version + means we don't have the heavy weight Java BS".
- **Ids:** B/V allocated when slices land. ADR-0061 (Proposed).
- **Depends on:** 009 (shell), 011 (D-pad focus, US1/US8), 013 (playback),
  017 (native API: `deviceToken`, `quickConnect*`, `Capabilities`).

## Goal

The pharos UI runs on Android TV / Google TV as an installable app built
from the same `pharos-ui` Dioxus components as the web UI, written only in
Rust. No Kotlin or Java is hand-written; the Android glue `dx` generates is
treated as build output.

## How it runs

`dx build --platform android --package pharos-ui` builds `pharos-ui` for `aarch64-linux-android`
(and `armv7` if a target TV needs it). The components run as native Rust;
the system WebView only draws the DOM Dioxus emits. Nothing is compiled to
WASM on this target.

`pharos-ui` already keeps every browser call in one module
(`native/web.rs`, ~2.2k lines) with a host stand-in (`native/host.rs`)
answering the same surface. Android adds a third module, `native/android.rs`,
selected by `cfg(target_os = "android")`, with the same functions:

| Area | Browser today | Android |
|---|---|---|
| RPC socket | `WebSocket` via `pharos-client::browser` | native websocket (tokio-tungstenite or similar) fed to `pharos_client::connect_stream` |
| Sign-in | `POST /pharos/session` cookie | `Bootstrap.login` / `quickConnect*` → `deviceToken`, then `resume(deviceToken)` |
| Token storage | session cookie | app-private file (Android's per-app sandbox); never logged |
| Prefs / `session_get` | `localStorage` | app-private file |
| Routing / back | History API, `popstate` | in-app history; the remote's Back key pops it, exits at home |
| Video | `<video>` + hls.js (`PlaybackEngine`) | slice 1: same, run in the WebView via `document::eval`; slice 3: native MediaCodec/ExoPlayer through JNI from Rust behind the same trait |
| Capabilities | `MediaSource.isTypeSupported` | slice 1: same probe in the WebView; slice 3: `MediaCodecList` + display HDR caps via JNI |
| Visualisers | WebGL via `web-sys` | off on TV until ported (see Open questions) |

Shared server code is unchanged: the TV is one more native-API client.

## TV specifics

- Manifest: `LEANBACK_LAUNCHER` intent, a 320×180 banner, and
  `android.hardware.touchscreen required=false`. If `dx` cannot set these
  from `Dioxus.toml`, a checked-in manifest template overrides its output.
- Input: D-pad, Enter/OK, Back, media keys. Needs 011 US1 (spatial focus)
  and US8 (media keys); that work is shared with the web, Quest and Steam
  Frame.
- Ten-foot layout: a device class (010 theming already resolves per-device
  values) with larger cards and focus glow always on.
- Sign-in on a TV: QuickConnect first (code shown on TV, approved from a
  signed-in phone or desktop); name + password stays as a fallback.

## Build and signing

- The APK builds inside the Nix devShell on ali-desktop (Android SDK, NDK
  and `dx`). Cloud containers cannot fetch flake inputs.
- Nix produces an **unsigned** APK. Signing (`zipalign` + `apksigner`) is a
  separate step outside any derivation: the keystore never enters
  `/nix/store` or the repo. Locally it is a sops/age-encrypted file
  decrypted only for the signing command; in CI a secret written to a temp
  file and removed after.
- Test installs: `adb connect <tv-ip>` + `adb install` with one dedicated
  "pharos-dev" key, so updates install over each other.

## Observability (ODD)

The TV reports through the same 016 client events and trace headers as the
web UI, with `client = "pharos-android-tv"` so dashboards split by
platform. The server already records the playback decision
(`pharos_playback_decision_total`); the client capabilities it sent are in
that decision's log line, so a wrong codec answer from the TV is visible by
query. Signal to name before slice 1 lands:
`{namespace="pharos"} |= "playback decision" |= "pharos-android-tv"`.

## Slices

0. **Spike** (days): `dx` Android build of a stripped app on Alison's TV:
   connect, QuickConnect sign-in, one home row, play one film with hls.js.
   Records whether the WebView's `<video>` + MSE and D-pad events work.
   Decides go/no-go for slice 1.
1. **Platform module**: `native/android.rs` with the table above (WebView
   playback), Back key, token storage, manifest, Nix build + signing.
2. **Ten-foot UI**: 011 spatial focus and media keys; TV device class.
3. **Native playback**: JNI `PlaybackEngine` (MediaCodec or ExoPlayer) and
   native capabilities, for HEVC, HDR and surround passthrough.

## Open questions

- Alison's TV model (Samsung Tizen / LG webOS cannot run Android apps).
- Port the WebGL visualisers to the WebView (eval'd JS) or to wgpu, or
  leave music visuals off on TV.
- **How the Android UI reads the DOM (settled 2026-11-08, `spike-0.md`
  "Eval latency").** An eval round trip on the TV is 2.5 ms p50 / 3.3 ms
  p95 whatever it reads, and a JS push into Rust holds 60 Hz. `android.rs`
  uses both, by how a value changes: state that changes on its own
  (`<video>` position and media state, viewport, page visibility, current
  path) is pushed by a JS mirror into Rust and read synchronously from the
  cache; reads made on an input (D-pad spatial focus, element sizes, tile
  in view) run as one batched eval per event, with the spatial pick done in
  JS beside the rects. Fire-and-forget calls (seek, play, focus by id,
  push path) are a spawned eval with no reply.
- Whether a real 320x180 banner drawable ships through `dx` (the manifest
  override works; the banner is the launcher icon for now).

## Quickstart: build and sign the TV APK

Landed in slice 1 (the dev shell, the build, the network layer; the UI is a
placeholder until `android.rs` lands; the DOM question above is settled).

```sh
# Dev shell (Android SDK 34, NDK, JDK 17, dx, rust android targets). It is
# exposed as legacyPackages.<system>.android, not devShells, so `nix flake
# check` does not build the unfree SDK.
nix develop .#android

# Build + zipalign + sign. The keystore is yours; the script never creates
# keys and nothing goes in the repo or the store.
export PHAROS_ANDROID_KEYSTORE=~/.config/pharos-android/pharos-dev.keystore
export PHAROS_ANDROID_KEYSTORE_PASS_FILE=~/.config/pharos-android/pass
export PHAROS_ANDROID_KEY_ALIAS=pharos-dev
nix run .#android-apk              # debug  -> target/pharos-tv-debug.apk
nix run .#android-apk -- --release # release -> target/pharos-tv-release.apk
```

`PHAROS_ANDROID_APK_OUT` overrides the output path; `CARGO_TARGET_DIR` moves
the build. The app targets `armv7-linux-androideabi` (the TCL TV is 32-bit
userland). The pure sign-in logic (`native/tv_auth.rs`) is tested on the host
with `cargo nextest run -p pharos-ui`; `native/android.rs` is the I/O around it.
