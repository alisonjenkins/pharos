# Spike 0: Dioxus Native on the G08

Measured 2026-11-08 on the G08 Android TV box (armv7, Android 9, adb over
the LAN). Spike code is on branch `spike/blitz-tv-0.8` (Dioxus
0.8.0-alpha.3, Blitz 0.3.0-beta.4); the earlier 0.7 run is on
`spike/blitz-tv`.

The spike renders the home rails and the library grid from `pharos-ui`
with a D-pad focus model. It has no network, images or video.

## Method

- Cold start: `am start -W -S`, TotalTime.
- Memory: `dumpsys meminfo` TOTAL PSS, on home and after opening the grid.
- Latency: `atrace -t 24 input gfx view sched` while 24 D-pad presses are
  sent 0.6 s apart. Press to visible is measured from `deliverInputEvent` to
  the next SurfaceFlinger `handlePageFlip` after the app's `queueBuffer`.
- Frame cost: the `log-times` feature of dioxus-native, which splits each
  frame into command encoding, GPU wait and style resolve.

The baseline is the WebView v12 app: 208 MB PSS plus a 106 MB renderer
process.

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

The 0.8 hybrid build cold-starts in about 1.0 s and draws one frame per
press. It uses less than half the memory of v12 before counting v12's
renderer process.

## Where the hybrid frame goes

At 1080p a frame is about 84 ms: about 23 ms of CPU command encoding, about
57 ms waiting on the GPU, and about 5 ms of style resolve.

Two probes split the GPU cost. The first removes CSS (key `s` in the
spike); the second renders below the window size and lets the compositor
scale up (`setprop debug.pharos.render_scale`). The figures are total
frame time:

| Render size | Shipped CSS | No poster shadows or overlays | Also no backdrop |
|---|---|---|---|
| 1080p | 84 ms | 66 ms | 58 ms |
| 720p (0.667) | 55 ms | 44 ms | 41 ms |
| 540p (0.5) | 44 ms | 35 ms | 32 ms |

At 720p, text is slightly soft but readable on the TV. The CPU part stays
at about 19 to 22 ms at every scale, so it is the floor until CPU encoding
and GPU work for consecutive frames overlap.

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

## Open choices

Which of these to take is Alison's call:

1. Render at 720p and let the compositor scale up.
2. Lighter TV CSS: no poster shadows or overlays, optionally no backdrop.
3. Overlap CPU and GPU work across frames (an upstream Blitz change).

Not yet measured: images, the video hole (ASurfaceControl under the UI)
and MediaCodec playback.
