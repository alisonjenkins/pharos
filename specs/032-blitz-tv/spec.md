# Feature specification: the Android TV app on Dioxus Native (Blitz)

**Feature Branch**: `feat/blitz-tv`
**Created**: 2026-11-08T13:30:00Z
**Status**: Draft. Slice 0 (spike) run; results in `spike-0.md`.
**Asked by**: Alison, 2026-11-08: "We should do the Dioxus route and we can potentially help improve the experimental nature of it" (choosing Dioxus Native over a WebView wrapper for the TV app).
**Scope**: A second Android TV build of `pharos-ui` that draws with Dioxus Native (Blitz: Stylo CSS, Taffy layout, a GPU or CPU 2D renderer) in place of the system WebView, plays video through the NDK `AMediaCodec` fed by `pharos-hls` (spec 031), and draws subtitles with `pharos-subs`. The current WebView app (spec 025, ADR-0061) keeps shipping until this one passes its parity gate.
**Decision record**: ADR-0077 (supersedes ADR-0061 in part). **Bugs reserved**: B870-B889. **Verification**: V820-V839 (V820, V821 used).
**Depends on**: 011 (input, spatial focus), 013 (playback), 017 (native API), 025 (the TV app's network layer, sign-in and manifest), 026 (`pharos-subs`), 031 (`pharos-hls`).

---

## Problem statement

The TV app today is `pharos-ui` running as native Rust with the system WebView drawing its DOM (spec 025). Two costs come from that split:

- **Every DOM read crosses a process boundary.** On the webview renderer the only DOM access is an async `document::eval`, about 2.5 ms per round trip on the TV (`specs/025-android-tv/spike-0.md`, "Eval latency"). `android_page.rs` works around it with a JS state mirror and batched evals, and the stabilisation work in PR #1002 keeps finding new places where a press or a clock tick waits on it.
- **The WebView is heavy for this TV.** The G08 has 2.4 GB of RAM and swaps under the current app. Chromium's renderer, GPU process and compositor are all resident, and its media stack decides codecs (no AC-3/E-AC-3 through MSE, HDR output not reported).

Blitz renders the same rsx in the app's own process. The DOM is a Rust tree, so a read is a function call, and there is no browser engine to keep resident. The cost is that Blitz is experimental, has no `<video>` element, and supports a subset of CSS.

Assumption, stated because the request named a solution: the goal is a TV app that responds faster and uses less memory than the WebView app, without a second UI codebase. If the spike shows Blitz cannot meet that on the G08, this spec says so and the WebView app stays.

## Reference device

All budgets are measured on Alison's TV, read over adb on 2026-11-08:

| Property | Value |
|---|---|
| Model | "Smart TV Pro", device `G08`, board `mt5879` / `merak` |
| OS | Android 12 (API 31), 32-bit userland (`armeabi-v7a`) |
| GPU | ARM Mali-G52, OpenGL ES 3.2, Vulkan 1.1 (`vulkan.level=1`, `vulkan.compute`) |
| Memory | 2.4 GB total, about 1 GB available with the current app running |
| Display | 3840x2160 panel, 1920x1080 window size override, density 320 |

## How it runs

```
pharos-ui components (rsx, shared with web)
        |
        v
dioxus-native  ->  blitz-dom (Stylo + Taffy)  ->  anyrender backend  ->  window surface (top layer)
        |                                                                   ^ transparent hole
native/blitz.rs platform module                                             |
        |                                                         ASurfaceControl child layer (below)
        +-- pharos-client socket, sign-in, token file (from 025)            ^
        +-- player: pharos-hls planner -> fMP4 samples -> AMediaCodec ------+
        +-- subtitles: pharos-subs masks -> overlay layer
```

- **Platform module.** A fourth `native/platform` module, `native/blitz.rs`, selected by a new `native` feature on Android. It reuses `native/android.rs` (socket, QuickConnect, token storage) unchanged. Calls that `android_page.rs` makes through `document::eval` become direct reads of the Blitz document (element rects, focus, scroll), so the JS state mirror and the batched-eval rules from spike-0 do not apply to this build.
- **Rendering.** The anyrender backend is chosen by the spike (see "Renderer matrix"). It must draw the whole UI at 1920x1080.
- **Input.** D-pad, OK, Back and media keys arrive through winit on Android. Focus and spatial navigation are spec 011's, driven from Rust against Blitz's layout instead of the WebView's built-in spatial navigation.
- **Video.** No `<video>` element exists. The player view leaves a transparent region in the UI; the Rust player decodes into an `ASurfaceControl` layer placed below the UI window, so the display controller composites video without a copy through the GPU. Requires API 29+; the TV is API 31. Decoding into a texture imported into the renderer is the fallback, and it loses tunneled playback and HDR output.
- **HLS.** `pharos-hls` (ADR-0071) already plans fetches, retries and buffers as pure functions and inspects fMP4. This build adds sample extraction from `moof`/`mdat` and a feeder that queues samples into `AMediaCodec`. No transmux, as spec 031.
- **Audio.** `AMediaCodec` decodes AAC and Opus into PCM played through AAudio. AC-3 and E-AC-3 passthrough needs an encoded `AudioTrack`, which has no NDK API; how to reach it without hand-written Java is an open question.
- **Subtitles.** `pharos-subs` already parses, lays out and rasterises to alpha masks. The player draws those masks either as an image in the Blitz tree above the video hole or as a second `ASurfaceControl` layer; slice 3 picks by measured cost.

## Renderer matrix

The spike tests these in order on the G08 and records start-up, frame time and memory for each. A degraded backend is a performance trade-off and goes to Alison before it is chosen.

| Backend | Crate | dioxus-native | Risk on the G08 |
|---|---|---|---|
| Vello on wgpu Vulkan | `anyrender_vello` | 0.7.10 (default) | Compute shaders on a Vulkan 1.1 Mali; highest risk |
| Vello on wgpu GLES 3.2 | `anyrender_vello` | 0.7.10 | GLES compute support in wgpu |
| Vello CPU | `anyrender_vello_cpu` | 0.7.10 | CPU time per frame on 32-bit ARM at 1080p |
| Skia on GL | `anyrender_skia` | 0.8.0-alpha only | Needs a workspace-wide Dioxus 0.8 bump |
| Vello hybrid | `anyrender_vello_hybrid` | 0.8.0-alpha only | As above |

The workspace pins Dioxus 0.7. The spike starts on dioxus-native 0.7.10 so it shares components with the web build unchanged. Moving to 0.8 is a separate decision recorded in its own ADR, because it changes the web build too.

## CSS used by `pharos-ui` today

`crates/pharos-ui/assets/pharos.css` (4,755 lines, counted on `origin/main` 2026-11-08). The spike checks how each renders on Blitz and lists the gaps. A gap is fixed upstream, worked around with a TV-only style, or accepted with Alison's OK.

| Feature | Uses | Notes |
|---|---|---|
| `var(--…)` custom properties | 530 | Theming (ADR-0036) depends on it |
| `calc()` | 72 | |
| `transform` | 86 | Focus growth on posters |
| `transition` / `animation` / `@keyframes` | 56 / 35 / 12 | |
| `:focus-visible` / `:focus-within` | 53 / 5 | Focus ring on TV |
| `display: grid` | 36 | Poster grids |
| `box-shadow` | 30 | Already off on TV in PR #1002 |
| `linear-gradient` / `radial-gradient` | 24 / 14 | Backdrops and scrims |
| `@media` | 22 | Includes the TV device class |
| `mask` | 18 | |
| `position: fixed` / `sticky` | 17 / 1 | Overlays and headers |
| `backdrop-filter` | 15 | Already off on TV in PR #1002 |
| `aspect-ratio` / `object-fit` | 14 / 11 | Posters |
| `:has()` | 12 | |
| `text-overflow` / `line-clamp` | 12 / 3 | Titles |
| `overflow: auto/scroll` / `scroll-snap` | 11 / 3 | Rails |

## User stories

1. **Browse with the remote.** On the TV, Alison opens Pharos, moves between rails and posters with the D-pad, opens an item with OK and goes back with Back. Every screen the WebView app has is there and looks the same apart from gaps she has accepted.
2. **Play a film.** She starts a film. It plays with hardware decoding, resumes where she left off, seeks with the remote, and shows the subtitles she picked.
3. **Stay responsive.** A remote press shows its effect within one or two frames, including on a 100-poster library page, and the app does not push the TV into swap.

## Go/no-go criteria for slice 0

Fixed before the spike runs. Both apps are measured in the same states on the G08: (a) home, idle, 10 s after a cold launch, signed in; (b) a library grid of at least 100 posters, scrolled to the end with the D-pad.

| Check | Pass |
|---|---|
| Renders home, a library grid and an item page from the real components | No missing or unreadable content; cosmetic gaps listed |
| D-pad moves focus, OK activates | Every focusable item on those screens reachable |
| Back | Pops in-app history; at home it leaves the app without a crash |
| Press to frame, state (b), 50 presses | p95 no worse than the WebView app, and under 50 ms |
| Frame time scrolling state (b) | p95 under 33 ms |
| Memory, state (a) and (b) | PSS no more than the WebView app's (process plus its WebView renderer and GPU process) |
| Cold start to first drawn home | No more than the WebView app's + 20% |
| Video hole probe | An `ASurfaceControl` layer below the window shows through a transparent region (colour fill is enough) |

Any fail goes to Alison with the numbers before slice 1 starts. A pass on everything except a renderer that needs Dioxus 0.8 also goes to her, as the 0.8 bump is hers to approve.

## Slices

0. **Spike.** A `pharos-tv-native` bin builds for `armv7-linux-androideabi` with its own application id, `com.alisonjenkins.pharos.tv.native`, so it installs beside the WebView app and never over it. It renders the three screens above from fixture data and from the live server, runs the renderer matrix, and records the go/no-go table. Results in `spike-0.md`.
1. **Platform module.** `native/blitz.rs` against the shared surface; sign-in and socket reused from `android.rs`; Back and media keys; Nix build and signing as spec 025.
2. **Ten-foot UI on Blitz.** Spec 011 spatial focus driven from Rust; CSS gaps closed upstream or with TV styles.
3. **Native player.** `pharos-hls` sample feeder into `AMediaCodec`, `ASurfaceControl` video layer, AAudio, `pharos-subs` overlay, `Capabilities` from `AMediaCodecList`-equivalent probing. Playback signals as 013.
4. **Parity and switch-over.** Feature parity with the WebView app on Alison's checklist; the WebView build stays installable as a fallback for one release.

## Observability (ODD)

The app reports as `client = "pharos-android-tv-native"` through the same 016 client events, so dashboards split it from the WebView app. Signal for the switch-over decision: `{namespace="pharos"} |= "playback decision" |= "pharos-android-tv-native"`. The spike logs its own numbers to logcat under tag `pharos-bench` (press-to-frame, frame time, PSS samples), the same way spike-0 measured eval latency.

## Upstream work

Alison is open to contributing fixes to Dioxus and Blitz. Each gap the spike finds is logged in `spike-0.md` with a minimal reproduction, and fixed upstream where that is smaller than a workaround here. Pharos depends on a released crate or a pinned git revision, never on a local patch that is not also offered upstream.

## What upstream gives us today

Read from the published crates and the Dioxus/Blitz repositories on 2026-11-08, not yet run on the TV:

| Area | Finding | Consequence |
|---|---|---|
| Packaging | `dx` 0.7.10 and 0.8.0-alpha.3 always generate a `WryActivity` (WebView) for Android; `--native` only affects desktop | The manifest override from spec 025 points the launcher at the framework's `android.app.NativeActivity` with `android.app.lib_name = main`, so no Kotlin or Java is written. Upstream fix: `dx` picks `NativeActivity` for the native renderer |
| Activity | dioxus-native 0.7.10 → blitz-shell 0.2 → winit 0.30 with `android-activity` `native-activity` | Matches the manifest above |
| Keys | winit maps D-pad to arrow keys, D-pad centre to Enter, Back to `BrowserBack`, media keys to `Media*`; it marks every key but volume as handled | Back reaches the app as a key (`use_back_button`) and never finishes the activity on its own, so leaving at home is ours to do |
| Renderer | 0.7.10 hard-wires GPU Vello (wgpu, Vulkan or GLES); Vello CPU only on the iOS simulator. 0.8 alpha makes Vello, Vello CPU, Vello hybrid (its default), Skia and Skia raster features | Testing anything but GPU Vello needs a patched 0.7.10 or 0.8. Open wgpu/Vello issues on Mali GPUs (wgpu #10186, #8937; vello #1816, #1465) |
| Focus | `tabindex` and `:focus` work; `.focus()` works through `set_focus`; Tab walks DOM order. `:focus-visible` and `:focus-within` always match nothing. Enter on a focused button does not fire a click. No spatial navigation | `pharos.css` uses `:focus-visible` 53 times, so the spike either patches Blitz to match it (upstream fix) or the TV build maps it to `:focus`. Enter-to-click and spatial focus are driven from Rust (spec 011) |
| CSS | Per blitz.is status: 2D transforms, transitions, animations, `box-shadow`, `overflow: scroll`, `object-fit`, `@font-face` supported; `filter` partial; `backdrop-filter` Skia only; 3D transforms and `overflow: auto` not; `position: sticky` in progress. Custom properties come from Stylo | The spike confirms each on the TV |
| Custom paint | `CustomPaintSource` gives a wgpu texture inside the tree; no API places a native surface under an element | The video hole is ours: a transparent element plus an `ASurfaceControl` child of the window's surface |

## Decisions

Made on 2026-11-08T23:15:00Z while Alison was away, under her instruction to decide rather than ask and to write each decision down. Each can be reversed by editing this section; none has shipped to anyone.

1. **Slice 0's two speed fails are accepted for now.** Slices 1 and 3 go ahead at 1080p with the shipped CSS. The fails (press to frame p95 154 ms, frame about 73 ms) are renderer cost; no part of the platform module or the player depends on the render size or the CSS. The WebView app keeps shipping until slice 4, so nobody uses the slow build meanwhile. Both measured levers are already runtime switches (`debug.pharos.render_scale`, the TV CSS probe), and press latency at 720p is not measured yet, so choosing 720p now would be choosing on a guess. The speed checks move to slice 4's gate unchanged: press to frame p95 under 50 ms and frame p95 under 33 ms on the G08, by whatever mix of 720p, lighter TV CSS and upstream renderer work gets there. The first input is a G08 run of press latency at 720p, with and without the light CSS. That run (2026-11-09T06:53Z, live server, spike build of 2026-11-08T21:58Z with the anyrender overlap patch, 24 D-pad presses 0.6 s apart, press to SurfaceFlinger page flip as in `spike-0.md`):

   | Render size, CSS | Press to visible p50 / p95 | Frame interval p50 / p95 | Frames per press |
   |---|---|---|---|
   | 1080p, shipped | 55 / 57 ms | 72 / 74 ms | 4 |
   | 720p, shipped | 51 / 54 ms | 39 / 41 ms | 6 |
   | 720p, no poster shadows or overlays | 47 / 49 ms | 30 / 33 ms | 7 |
   | 720p, also no backdrop | 45 / 47 ms | 28 / 31 ms | 7 |

   Press latency passes the gate (p95 under 50 ms) at 720p with either lighter CSS level. Frame time (p95 under 33 ms) passes only with the backdrop removed too (31 ms); with only the shadows and overlays removed it sits on the line at 33 ms. The 1080p press figure is about half of slice 0's (p95 154 ms then); this build carries the later fixes, and the reason for the gap is not measured. Each press now settles inside the 0.6 s spacing, so the first frame after a press is drawn for that press.
2. **The spike keeps the anyrender overlap patch until anyrender#92 is released.** The patch is offered upstream as that PR, which "Upstream work" requires. The app moves to the released crate when there is one.
3. **The video layer is a `SurfaceControl` child built over JNI (spike mode 3).** It works on API 31 through the hidden `ViewRootImpl.getSurfaceControl()` and has a public route from API 33. The Kotlin `SurfaceView` (mode 1) stays the documented fallback only, as ADR-0061 and ADR-0077 rule out hand-written Java and Kotlin.
4. **The feeder parses fMP4 in `pharos-hls`**, not through `AMediaExtractor_setDataSourceCustom`. `pharos-hls` gains the sample table, the codec boxes and the Annex B conversion as pure functions, tested on the host against ffmpeg fixtures. That keeps variant switches under the planner (an extractor over one concatenated stream would need rebuilding at each switch), and needs no extra copy of each segment. The extractor route stays the stopgap if the feeder stalls.

Made on 2026-11-09 while Alison was asleep, under the same instruction:

5. **The feeder is checked against served segments on the host (V820).** Three segments of the h264cmaf video rendition and the demuxed audio rendition, from the production argv through `process_segment`, read the same as ffprobe, run on across boundaries and decode in Annex B one frame per sample. Served samples carry no in-band SPS/PPS, so the player must always pass the init's parameter sets as `csd-0`/`csd-1`. The TV feeder's first run no longer has to be that check.
6. **Capabilities come from `MediaCodecList` by fixed rules (V821).** A video codec needs a hardware decoder that takes 1080p (Android ships software VP9 and AV1 decoders on every device, and a TV SoC cannot be relied on to run them at 1080p); audio takes any decoder; containers are `NO_DIRECT_PLAY` only; image subtitles are burned in until the native player draws them; HDR is the display's answer. Because the negotiator reads an empty list as "any", no list is ever empty: a failed probe declares H.264 and AAC, which the CDD requires. The JNI walk is on `spike/blitz-tv-codecs`; the rules are in `pharos-ui` `native::decoders`. The G08's list, read passively with `dumpsys media.player` on 2026-11-09, has MediaTek hardware decoders for H.264, HEVC, VP9 and AV1 (each to 4096x2304), plus Dolby Vision, MPEG-2 and VC-1, and hardware AC-3/E-AC-3 and DTS; AAC, Opus, FLAC and MP3 are Android's software decoders. So the G08 is offered all four video codecs and six audio codecs, and AV1 needs no transcode on it.

## Open questions

1. Does the `NativeActivity` APK launch and draw on the TV? It builds: a smoke app with `android_main` handing the `AndroidApp` to `blitz_shell::set_android_app` produces a 22 MB armeabi-v7a APK whose launcher activity is `android.app.NativeActivity` (2026-11-08, dx 0.7.10).
2. Is `:focus-visible` fixed upstream (smaller, and helps every Blitz user) or mapped to `:focus` for TV?
3. AC-3/E-AC-3 passthrough: encoded `AudioTrack` has no NDK API. Options are JNI from Rust to the framework class (no hand-written Java, as ADR-0061) or PCM decode only.
4. HDR: does an `ASurfaceControl` layer with HDR10 dataspace reach the panel as HDR on this TV?
5. Image formats the server sends for posters (WebP?) and whether Blitz's image decoders cover them.
6. Music visualisers (WebGL today): port to the renderer's GPU path or leave them off on TV.
