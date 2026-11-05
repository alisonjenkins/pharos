# Feature specification: one Rust subtitle renderer for every Pharos client

**Feature Branch**: `feat/ui-subtitle-renderer`
**Created**: 2026-11-05T21:30:00Z
**Status**: Ready
**Scope**: The native web UI (`/ui/`, 013/017), the Android TV app (025, WebView first) and later phone and native-video clients draw every text subtitle (SRT, WebVTT, ASS/SSA, embedded or sidecar) with one Rust crate, `pharos-subs`, instead of the browser's `<track>`/`::cue` and the vendored JASSUB. Image subtitles (019) and jellyfin-web are unchanged.
**Decision record**: ADR-0064. **Backlog**: B572-B577. **Verification**: V485-V490.

---

## Problem statement

Subtitles look different on every client, and the look Alison wants cannot be built on the native track.

- Plain subtitles are a WebVTT `<track>` styled with `::cue`. `::cue` takes no `border-radius` or `padding` (`pharos.css`, "WebVTT cues" and "Subtitles (browser-native <track>)"), so the rounded, translucent, text-hugging backdrop only exists through Chromium's `::-webkit-media-text-track-display` pseudo-element. Firefox gets a halo and no box; the Android WebView's cue layout is the platform's; the Android shell has no subtitle styling at all (`native/host.rs` `apply_subtitle_size` is a no-op). Where the cue sits is the user agent's choice, so it cannot be raised off the bottom edge reliably.
- ASS/SSA tracks go through JASSUB (libass compiled to WASM, ADR-0057). It draws typesetting well, but it is a second renderer with its own JS glue and worker, a ~2.1 MB wasm download per session, and it cannot run outside a browser, so a future native-video client would need libass again as C.
- Fonts embedded in the file (MKV attachments) are never used: the native API does not list them, so every style draws in Liberation Sans (ADR-0057 consequences).
- Sidecar `.ass` files are labelled `webvtt` in the stream list and play as plain text.
- The animated-subtitles work ("Design AI-generated animated anime subtitles") will produce ASS with heavy animation; it needs a renderer we control, that runs well on the weakest TV.

## Hard requirement: runs well on weak devices

Performance is a release gate, not a goal. The reference weak device is the bedroom TV (TCL G08 Google TV, armeabi-v7a, Android WebView). A slice does not merge if its benchmark (V485) misses a budget on that device.

| Budget | Value on the TCL G08 | How measured |
|---|---|---|
| Idle cost when no subtitle changes | 0 work per frame: no raster, no upload, no draw call | Trace via WebView CDP; render counter stays flat |
| Plain-text cue change (layout + raster + upload) | p95 <= 4 ms, off the main thread | `pharos-subs` bench on armv7 + in-WebView trace |
| Static ASS event first draw | p95 <= 8 ms per event, worker | same |
| Animated ASS frame (corpus "heavy sign" scene) | p95 <= 12 ms per rendered frame, worker; frame rate adapts 60/30/15 before falling back | same |
| Main-thread cost per video frame while subtitles show | <= 1 ms (texture bind + quads only) | Performance trace, long tasks = 0 |
| Wasm download | <= 800 KB gzip (JASSUB: ~2.1 MB wasm) | CI size check |
| Peak memory (renderer + font and bitmap caches) | <= 48 MB, LRU-bounded | `performance.measureUserAgentSpecificMemory` / heap snapshot |
| First subtitle after picking a track (warm cache) | <= 300 ms | e2e timing |
| Raster resolution | min(display size, 1920x1080); upscaled by the GPU on a 4K panel | code + V485 |

Design rules that make the budgets reachable:

1. **Render on change, not per frame.** Each event is rasterised once into alpha masks and cached; a frame is re-rasterised only while an animated event is on screen, and only that event.
2. **Alpha masks, coloured on the GPU.** Like libass's `ASS_Image`, output is 8-bit coverage masks plus a colour per layer (fill, border, shadow). One R8 texture atlas, tinted in the shader: a quarter of the memory and upload of RGBA.
3. **Composite on the existing GPU layer.** A `subtitles` program on `render/` (ADR-0039/0049, WebGL2) draws textured quads over the video. No `putImageData`, no per-frame canvas 2D.
4. **Off the main thread.** The renderer runs in a dedicated Web Worker (the crate built as a standalone wasm module, no Dioxus); masks cross as transferable buffers. On Android the same wasm worker runs inside the WebView: pixels never cross the Rust-to-WebView `eval` bridge.
5. **Caches with bounds.** Shaped runs keyed by (font, size, text), glyph outlines by (font, glyph, size bucket), blurred masks by (outline hash, blur). All LRU within the memory budget.
6. **Cheap blur.** `\blur`/`\be` as three separable box passes over integer coverage (Gaussian within 3%), wasm SIMD where the device has it, scalar otherwise.
7. **Small fonts.** The server subsets each font (attachments and fallbacks) to the glyphs the script uses before sending it, so a CJK fallback costs kilobytes, not 16 MB.
8. **Degrade, never stutter.** If animated frames miss budget for 2 s, the renderer halves its animation rate; at 15 fps still missing, the player asks the server for pre-rendered frames of that track (slice 5) and keeps showing the words meanwhile.

## User story 1 - Plain subtitles look the same everywhere (Priority: P1)

A viewer turns on an SRT, WebVTT or plain ASS track on the web UI in Chrome or Firefox, or on the TV. The words sit on a rounded, translucent backdrop that hugs each line, at the size picked in Appearance, raised off the bottom edge of the picture, and look identical on every client.

**Independent test**: Dioxus e2e on a fixture with an SRT track in Chromium and Firefox: the subtitle canvas shows the cue, no `::cue` is rendered, and screenshots of both browsers match within the visual tolerance (V487).

**Acceptance scenarios**:

1. **Given** a text track is picked, **When** a cue is active, **Then** it is drawn by `pharos-subs` on the subtitle layer, with backdrop radius, opacity, padding and bottom margin from the theme tokens, and the `<track>` is present but hidden (kept for fallback and accessibility).
2. **Given** the viewer picks Small, Medium, Large or Extra large, **Then** text height scales 75/100/125/150% of the base size (relative to picture height, not window), and the change shows on the next frame without reloading the track.
3. **Given** the controls are showing, **Then** subtitles move up above the control bar and return when it hides.
4. **Given** the renderer fails to start or misses the first-subtitle budget, **Then** the native `<track>` comes back (as JASSUB's fallback does today): the viewer loses styling, never words.

## User story 2 - Anime typesetting with the file's own fonts (Priority: P1)

A viewer plays an MKV whose ASS track carries signs, karaoke and fonts as attachments. Signs appear at their authored positions, in their own typefaces, with borders, shadows, rotation, clips and fades, as libass draws them.

**Independent test**: the conformance corpus (V486): each sample frame rendered by `pharos-subs` matches the libass golden image from the server's burn path at SSIM >= 0.97 (>= 0.99 for dialogue-only frames).

**Acceptance scenarios**:

1. **Given** a file with font attachments, **When** its ASS track is picked, **Then** the player fetches the subsetted attachments and every style whose `Fontname` matches an attachment (family or full name, case-insensitive, as libass matches) draws in it (V488).
2. **Given** a sidecar `.ass` or `.ssa` next to the file, **Then** the stream list labels it `ass`/`ssa` and it plays styled, like an embedded track (V489).
3. **Given** a tag outside the supported set, **Then** the line still draws with that tag ignored, and `pharos_subtitle_render_unsupported_total{tag}` counts it.

## User story 3 - Animated subtitles on the weakest TV (Priority: P2)

A viewer on the TCL G08 watches an episode whose ASS track animates signs and karaoke (fansub-style, or generated by the animated-subtitles work). Animation is smooth, video playback does not drop frames, and if the TV cannot keep up the effects degrade to pre-rendered frames rather than stutter.

**Acceptance scenarios**:

1. **Given** the heavy-sign corpus scene on the G08, **Then** rendered frames meet the animated-frame budget and the video's dropped-frame count does not rise against subtitles off (V485).
2. **Given** a scene beyond the budget even at 15 fps, **Then** the player switches that track to server pre-rendered frames (slice 5, `SubtitleDelivery::image`) without restarting playback (V490).

## Requirements

### Renderer crate

- **FR-001** New workspace crate `crates/pharos-subs`, pure Rust, no C dependencies, building for `wasm32-unknown-unknown`, `armv7-linux-androideabi`, `aarch64-*` and the host. No `web-sys` in the core; the worker entry is a thin `wasm-bindgen` module beside it.
- **FR-002** Inputs: ASS v4+ and SSA v4 scripts (UTF-8 and the legacy codepages via `encoding_rs`), SRT and WebVTT (converted to an internal ASS-equivalent model with a Pharos default style). Output per `frame(t_ms, viewport)`: `Frame { changed: bool, layers: Vec<Mask> }`, each mask a coverage bitmap, its placement and an RGBA tint.
- **FR-003** Text stack: `harfrust` (HarfBuzz-compatible shaping), `skrifa` (font parsing and outlines), `unicode-bidi`, `unicode-linebreak`, an own anti-aliased scanline rasteriser (non-zero winding, 8-bit coverage) and stroker for `\bord`. Line wrapping implements `WrapStyle` 0-3 as libass does.
- **FR-004** Tag tiers, delivered by slice:
  - *Plain* (slice 1): styles, `\N \n \h`, `\an`, `\b \i \u \s`, `\fs`, `\c \1c-\4c`, `\alpha`, `\bord`, `\shad`, `\fn`.
  - *Static typesetting* (slice 2): `\pos`, `\org`, `\fscx \fscy \fsp`, `\frx \fry \frz`, `\fax \fay`, `\clip`/`\iclip` (rectangle), `\q`, `\r`, collision handling per `Layer` and margins, `PlayResX/Y` and `ScaledBorderAndShadow`.
  - *Animation* (slice 3): `\move`, `\fad \fade`, `\t` (all animatable tags, with accel), `\k \K \kf \ko`, `\blur \be`, vector `\clip`/`\iclip`, `\p` drawings, `\pbo`.
- **FR-005** Determinism: the same script, time and viewport give the same masks on every target (integer or fixed-point raster; float only in layout), so a golden test on the host holds on the TV.

### Platform adapters (spec 009 §Boundaries)

- **FR-006** A `SubtitleRenderer` port in `pharos-ui` with `load(track)`, `frame(t, viewport) -> Frame`, `set_style(SubtitleStyle)`, `stats()`. Adapters: `RustSubs` (worker), `Jassub` (today's `platform::StyledSubtitles`, kept until FR-016), `NativeCue` (`<track>`, fallback only) and a fake with a contract test (009 FR-014).
- **FR-007** A `SubtitleCompositor` port: `render/` WebGL2 on web and the Android WebView; a later native-video client implements it on its own surface. The view layer imports neither adapter.
- **FR-008** The viewport maps the script's `PlayRes` onto the video's displayed content box (letterbox and anamorphic, as 019 does); the controls' height is passed in so subtitles clear the control bar (US1 #3).

### Pharos style for plain subtitles

- **FR-009** `SubtitleStyle` from theme tokens: text colour, backdrop colour and opacity (default black at 55%), backdrop corner radius (0.45 em), horizontal and vertical padding, bottom margin (default 6% of picture height), size scale from `SubtitleSize`. Plain tracks use it; ASS tracks keep their own styles unless the viewer turns on "Override ASS styles" (later, out of scope here).

### Server

- **FR-010** The native API lists a media source's font attachments (index, filename, mime; family names are matched by the renderer from the font itself) and serves each, subset to the glyphs the chosen subtitle track uses, at `/videos/{id}/subtitles/{index}/fonts/{n}` (`skera`, the fontations subsetter, pure Rust; `subsetter` was rejected because its output is PDF-only and drops `cmap`), cached on disk like 019's extractions, behind item access.
- **FR-011** Fallback fonts (Pharos default UI face, a CJK face, a symbol face) are served the same way, subset per track, so the client never downloads a full CJK font.
- **FR-012** Sidecar `.ass`/`.ssa` are labelled with their real codec in the stream list; their original script is served at the existing `stream.ass` route.
- **FR-013** Slice 5: the server can pre-render an ASS track with libass (the existing burn path's filter graph, output as alpha-mask PNGs plus a 019-style manifest) when a client asks for it, on demand, background-I/O gated and cached; delivery `image`.

### Signals and performance gate

- **FR-014** `pharos_subtitle_render_ms{tier,phase}` histogram (client-reported through the observability channel, 016), `pharos_subtitle_render_fallback_total{reason}`, and the unsupported-tag counter (US2 #3).
- **FR-015** `pharos-subs` has criterion benches over the corpus (plain, typeset, heavy-sign); CI runs them on the host for regressions, and V485 runs the armv7 build on the TCL G08 through `adb` plus an in-WebView trace via CDP. No V485 run between 23:00 and 08:00 local (the TV is in a bedroom).
- **FR-016** JASSUB is removed (ADR-0057 superseded) only when the static and animation tiers pass the conformance corpus (V486) and V485 on the G08.

## Delivery slices

| Slice | Backlog | What ships | Gate |
|---|---|---|---|
| 0 | B572 | FR-010/011/012: font attachments (subset) in the native API, sidecar ASS labelled. JASSUB gets the fonts at once. | V488, V489 |
| 1 | B573 | `pharos-subs` plain tier, worker, `subtitles` render program, Pharos style, size picker, raised and above-controls placement. Replaces `::cue` on web and TV. | V485 (plain), V487 |
| 2 | B574 | Static typesetting tier; Rust renderer becomes the default for ASS behind a pref, JASSUB the fallback. | V486 (static) |
| 3 | B575 | Animation tier, adaptive frame rate. | V485 (animated), V486 (all) |
| 4 | B576 | Retire JASSUB and the vendor script; ADR-0057 superseded. | FR-016 |
| 5 | B577 | Server pre-render fallback for scenes a device cannot animate. | V490 |

Verification ids: V485 G08 performance budgets, V486 libass conformance corpus, V487 plain style identical in Chromium, Firefox and TV, V488 embedded fonts used, V489 sidecar ASS styled, V490 pre-render fallback without a playback restart.

## Conformance corpus

A directory of short ASS scripts and fonts under `crates/pharos-subs/tests/corpus/` (dialogue, signs, karaoke, heavy animation, CJK, RTL, broken input). Goldens are rendered once by libass through `pharos-transcode`'s burn path on a transparent canvas and checked in as PNGs with a script to regenerate them. Licensing: only scripts we write or that their authors license for redistribution.

## Out of scope

Image subtitles (019, already drawn by the player); jellyfin-web; editing or authoring subtitles; per-user overrides of ASS styles; full VSFilter quirk parity (we match libass, the reference, on the corpus).

## Open questions

1. Is 800 KB gzip achievable with `harfrust` + `skrifa`? Slice 1 measures it first; if not, the budget is revisited with Alison before more code.
2. Does the G08 WebView expose wasm SIMD and `OffscreenCanvas`? Only affects speed paths, not correctness; checked in slice 1's first V485 run.
3. Whether to take parts of `rassa` (MIT, a young pure-Rust libass rewrite) rather than write them: its parser may be usable; its renderer is not (it links FreeType and fontconfig, so it cannot build for `wasm32-unknown-unknown`).
