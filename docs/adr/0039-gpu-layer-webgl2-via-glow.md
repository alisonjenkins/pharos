# ADR-0039: The native UI's GPU layer is WebGL2 via `glow`, behind a `Surface` port, with CSS as the fallback

- **Status:** Accepted
- **Date:** 2026-11-02T19:20:00Z
- **Deciders:** Alison Jenkins

## Context

Spec 009 asks for a "WebGL or similar" first-class UI and plans a hybrid
renderer: DOM for content, a GPU layer for effects (009 FR-005, research R2,
Clarifications Q1). Two features now need that layer at once: the library's
animated theme backdrop (010 `effects.shader`) and the music visualiser
(013 US7). Without a shared layer each would carry its own WebGL setup.

Constraints: the UI targets Quest Browser and Steam Frame, which are
fill-rate bound; users may ask for reduced motion; WebGL2 can be missing or
lose its context; themes are data only and may not carry shader source (010).

## Decision

We will draw GPU effects with raw WebGL2 through the `glow` crate (0.17),
behind a `Surface` port in `crates/pharos-ui/src/render/`:

- `render::open(canvas_id, program, reduced_motion)` returns a
  `Box<dyn Surface>` or a `Fallback` reason (`reduced_motion`, `no_webgl2`,
  `context_lost`, `shader`, `no_canvas`). Host builds always fall back, so
  every screen renders and tests without a GPU.
- Programs come from a compiled-in catalogue (`render::Program`): one
  full-viewport fragment shader each, GLSL ES 3.00, sharing one prelude of
  bounded uniforms (time, resolution, theme `bg`/`accent`/`fg`, a 0–1 level)
  and a 256×2 R8 data texture (spectrum and waveform rows). Nothing outside
  the bundle supplies shader source.
- The drawing buffer is capped at 1.25 device pixels per CSS pixel and the
  context asks for `low-power`, no alpha, depth, stencil or antialiasing.
- A lost context is not restored: the surface reports `context_lost` and the
  screen keeps its CSS look.

## Consequences

- Screens add an effect by adding a catalogue entry and a `<canvas>`; the
  library backdrop and the visualiser share all of the GL code.
- The wasm bundle grows by `glow`'s bindings (an upper bound of 200 KB gzip
  was measured standalone in 009 research R2; most of it is glue the UI
  already carries).
- Every effect has a CSS-only look by construction, which must stay usable
  on its own.
- Sprites synced to DOM rects (009 R2) are not built yet; they extend this
  port rather than replace it.
- WebGPU can replace the backend later behind the same `Surface` port.

## Alternatives considered

- **`web_sys::WebGl2RenderingContext` directly**: no extra crate, but every
  call is stringly typed JS glue and a later native or WebGPU backend would
  share nothing. 009 Clarifications Q1 picked `glow`.
- **WebGPU / `wgpu` first**: not available in every target browser yet
  (Quest Browser support lags) and a much larger bundle.
- **Canvas 2D**: no shaders, and too slow for full-screen per-pixel effects
  on a headset.

## References

- specs/009-ui-foundation/spec.md FR-005, research.md R2
- specs/010-ui-theming/spec.md (themes carry no shader source)
- specs/013-ui-playback/spec.md US7 (GPU visualiser)
