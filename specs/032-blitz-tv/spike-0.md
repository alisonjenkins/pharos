# Spike 0: Dioxus Native on the G08

Measured 2026-11-08 on the G08 Android TV box (armv7 userland, Android 12,
API 31, Mali-G52, adb over the LAN). Spike code is on branch
`spike/blitz-tv-0.8` (Dioxus 0.8.0-alpha.3, Blitz 0.3.0-beta.4); the
earlier 0.7 run is on `spike/blitz-tv`.

The spike renders the home rails, a library grid of 120 posters and the
item page from `pharos-ui` with a D-pad focus model. It signs in by
QuickConnect and loads them from the live server, with real poster images.
It has no player yet; the video hole is probed with a colour fill.

## Method

- Cold start: `am start -W -S`, TotalTime.
- Memory: `dumpsys meminfo` TOTAL PSS, on home and after opening the grid.
- Latency: `atrace -t 24 input gfx view sched` while 24 D-pad presses are
  sent 0.6 s apart. Press to visible is measured from `deliverInputEvent` to
  the next SurfaceFlinger `handlePageFlip` after the app's `queueBuffer`.
- Frame cost: the `log-times` feature of dioxus-native, which splits each
  frame into command encoding, GPU wait and style resolve.
- Runtime switches are system properties read at launch
  (`setprop debug.pharos.<name>`), so one APK covers each A/B pair.

The baseline is the WebView v12 app: 208 MB PSS plus a 106 MB renderer
process, 314 MB in all.

## Go/no-go

| Check | Result | |
|---|---|---|
| Renders home, grid, item page from the real components | Pass | Live data and posters; cosmetic gaps below |
| D-pad moves focus, OK activates | Pass | OK on a poster opens its item page |
| Back | Partial | Back closes the item page; leaving the app from home not checked |
| Press to frame, state (b) | **Fail** | p50 103 ms, p95 154 ms; target p95 under 50 ms |
| Frame time scrolling state (b) | **Fail** | about 73 ms; target p95 under 33 ms |
| Memory, states (a) and (b) | Pass | 164 / 182 MB against v12's 314 MB |
| Cold start to first drawn home | Not compared | 0.8 to 1.2 s; v12 not re-measured, as that would restart Alison's session |
| Video hole probe | Pass, with a caveat | A layer under the window shows through; `ASurfaceControl` itself is refused (see below) |

Latency and frame time are with CPU/GPU overlap and real posters at 1080p
with the shipped CSS. The renderer also needs Dioxus 0.8, which is
Alison's to approve (ADR-0077 keeps the workspace on 0.7).

## Results by renderer

| Build | Frame | Press to visible (p50 / p95) | PSS home / grid |
|---|---|---|---|
| 0.7, Vello GPU | ~373 ms | 396 / 578 ms | 96 / 184 MB |
| 0.7, Vello CPU | ~1.1 s | n/a | n/a |
| 0.8, vello-hybrid | ~94 ms | 139 / 153 ms | 79 / 91 MB |
| 0.8, Vello GPU | 273 ms | 294 ms p50 | 111 MB |
| 0.8, Vello CPU | ~730 ms | n/a | 77 / 90 MB |
| 0.8, Skia | no build | n/a | n/a |

Skia does not build for armv7-linux-androideabi because skia-bindings has
no prebuilt binary for that target; a source build was not tried.

These rows use placeholder posters and no network. The 0.8 hybrid build
cold-starts in about 1.0 s and draws 3 to 6 frames per press (the focus
move, then the rail or grid settling). wgpu runs its GLES (EGL) backend on
this TV, not Vulkan.

## Where the hybrid frame goes

At 1080p a frame is about 84 ms: about 23 ms of CPU command encoding, about
57 ms waiting on the GPU, and about 5 ms of style resolve.

Two probes split the GPU cost. The first removes CSS (key `s` in the
spike); the second renders below the window size and lets the compositor
scale up (`debug.pharos.render_scale`). The figures are total frame time:

| Render size | Shipped CSS | No poster shadows or overlays | Also no backdrop |
|---|---|---|---|
| 1080p | 84 ms | 66 ms | 58 ms |
| 720p (0.667) | 55 ms | 44 ms | 41 ms |
| 540p (0.5) | 44 ms | 35 ms | 32 ms |

At 720p, text is slightly soft but readable on the TV.

## CPU and GPU overlap

`anyrender_vello_hybrid` 0.11 waits for the GPU after every frame
(`device.poll(wait_indefinitely)`), so the CPU half of frame N+1 never runs
while the GPU draws frame N. Polling without waiting, and letting surface
acquisition apply backpressure, overlaps the two. The spike carries this as
a patch; upstream PR DioxusLabs/anyrender#92 makes the same change, and the
TV numbers below are posted on it. `debug.pharos.gpu_wait 1` restores the
wait for comparison.

| 1080p, placeholder posters | Wait | Overlap |
|---|---|---|
| Frame interval | 92 ms | 57 ms |
| Press to visible p50 | 118 ms | 106 ms |
| Press to visible p95 | 153 ms | 124 ms |

At 720p the frame interval drops from 62 to 34 ms. Settled screens are
pixel-identical with and without the wait.

## Live server and images

- Sign-in by QuickConnect the first time; later launches resume with the
  stored token. Home and a 120-item library page then load in 0.14 to
  0.36 s. The item page loads in about 59 ms.
- The capnp clients are `!Send`, so they run on their own thread with a
  current-thread runtime and a `LocalSet`, and talk to the UI over channels.
- Blitz fetches images through a `NetProvider` of the spike's own (reqwest
  with rustls, bearer token in a header, same origin only, 4 in flight).
  blitz-net is not used because it pulls in native-tls.
- The server sends posters as JPEG; Blitz decodes JPEG, PNG, WebP and GIF.
- Blitz ignores `srcset` and `sizes`, so it fetches the `Art` component's
  480 px fallback. Asking for 360 px instead (the tile is 288 device px)
  cuts image bytes by 26%. `debug.pharos.art_width` sets the width.
- Blitz ignores `loading="lazy"` and fetches every `<img>` in the tree. The
  spike draws only the grid rows near the focus (one behind, three ahead).

Real posters cost frame time: about 73 ms at 1080p with overlap, against
57 ms with placeholders. Press to visible is p50 103 ms and p95 154 ms.

Memory with real posters, before and after the two image fixes:

| PSS | Home | Grid |
|---|---|---|
| All rows, 480 px | 213 MB | 368 MB |
| Rows near focus, 360 px | 164 MB | 182 MB |

## Video hole

The player needs a video layer under the UI that shows through wherever
Blitz draws nothing. `debug.pharos.hole` selects how the layer is made.
Every mode first sets the window format to TRANSLUCENT over JNI:
NativeActivity asks for an opaque RGB_565 window, and SurfaceFlinger then
ignores the buffer's alpha. The renderer clears to transparent; the
GLES backend reports only an opaque composite alpha mode, but the alpha
reaches SurfaceFlinger and the window composites as non-opaque RGBA_8888.

| Mode | Layer | Result |
|---|---|---|
| 2 | NDK `ASurfaceControl_createFromWindow`, z -1 | Refused every time: `createWithSurfaceParent error Invalid argument`. The window is a BLAST layer, which API 31 cannot parent a child to through its native window. |
| 3 | `android.view.SurfaceControl` built over JNI as a child of the window's SurfaceControl, layer -1, wrapped in a `Surface` | Works. The parent comes from `ViewRootImpl.getSurfaceControl()`, hidden on API 31 and 32 (API 33 has a public route through `AttachedSurfaceControl`). No hidden-API warning is logged. Ready about 200 ms after launch. |
| 1 | A `SurfaceView` added by a 40-line Kotlin class, bundled by dx through `#[manganis::ffi]` | Works, but it is hand-written Kotlin, which ADR-0061 and ADR-0077 rule out. |

In modes 1 and 3 the fill shows through the transparent region, a 50%
black bar drawn by Blitz darkens it, and an opaque box covers it.

SurfaceFlinger composites the colour-filled layer and the UI window on the
GPU (CLIENT composition), not on a hardware overlay. A canvas-filled RGBA
buffer is not a decoder buffer, so this says nothing yet about video; the
first player step checks it (below).

## Fixes and gaps found

Blitz 0.2 (Dioxus 0.7) needed three local patches: a taffy aspect-ratio
fix, keys with no physical code (DPAD_CENTER, BACK) being dropped, and a
redraw on every key event that tripled frames per press. Blitz 0.3 has all
three upstream.

Gaps still open in Blitz:

- Focusability is computed only when an element is created, so the spike
  routes keys through one hidden autofocused element.
- Nested transforms are not applied to children, so the spike scrolls
  rails and the grid with margins instead of transforms.
- No `srcset`/`sizes` and no `loading="lazy"` (see images).
- No hook to pass a `NetProvider` or base URL through `dioxus_native::launch`,
  so the spike copies a trimmed `launch`.

The item page needs the `MarkPlayed` context from its host; the spike
provides one that only logs.

## Video plan

The plan keeps ADR-0077: `pharos-hls` feeds `AMediaCodec`, no hand-written
Java. Framework classes with no NDK API are called over JNI from Rust,
which ADR-0061 allows. In order:

1. **Decode one stream into the mode 3 surface.** `ANativeWindow_fromSurface`
   on the JNI-built `Surface`, `AMediaCodec` configured with it, H.264 from
   one fMP4 segment. Checks: frames shown, and whether SurfaceFlinger puts
   the decoder's buffers on a hardware overlay or composites them on the
   GPU. GPU composition of every video frame would compete with Blitz for
   the Mali-G52 and is the main risk to the plan.
2. **Codec capabilities.** The NDK has no codec list on API 31, so
   `MediaCodecList` is read over JNI into `Capabilities` (spec 013): H.264,
   HEVC, VP9, AV1, profiles and levels, and tunneled playback support.
3. **HLS feeder.** `pharos-hls` plans fetches; the feeder parses `moof`/`mdat`
   (`trun` sample sizes, durations, composition offsets) and queues samples
   with `AMediaCodec_queueInputBuffer`. If that proves slow to get right,
   `AMediaExtractor_setDataSourceCustom` (API 28) over a data source that
   presents the init segment and fragments as one stream is a stopgap.
4. **Audio and sync.** AAC and Opus decode to PCM and play through AAudio;
   the audio clock is the master, and video frames are released with
   `AMediaCodec_releaseOutputBufferAtTime`. AC-3 and E-AC-3 passthrough use
   an encoded `android.media.AudioTrack` created over JNI (no NDK API), which
   also gives the audio session id that tunneled playback needs.
5. **Subtitles.** `pharos-subs` masks go to a second SurfaceControl layer
   between the video and the UI, written with `ANativeWindow_lock`. That
   needs no Blitz frame per cue change; at 57 to 73 ms a frame, Blitz frames
   during playback would compete with the video. Drawing the masks as an
   image in the Blitz tree stays the fallback.
6. **HDR.** Read the dataspace the decoder sets on its buffers and whether
   the panel switches mode; open question 4 in the spec.

If the hidden `getSurfaceControl` is blocked on a later Android version
below API 33, the fallback is the Kotlin SurfaceView (mode 1), which needs
Alison to relax the no-Java rule for one class.

## Open choices

Which of these to take is Alison's call:

1. Render at 720p and let the compositor scale up (frame about 34 ms with
   overlap, placeholders).
2. Lighter TV CSS: no poster shadows or overlays, optionally no backdrop.
3. Keep the overlap patch until anyrender#92 is released.
4. The two go/no-go fails stand at 1080p with the full CSS. Options 1 and 2
   are the measured levers on frame time; press latency with them is not
   yet measured.
