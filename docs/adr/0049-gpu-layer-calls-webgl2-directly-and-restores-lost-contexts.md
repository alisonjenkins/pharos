# ADR-0049: The GPU layer calls WebGL2 through `web_sys` directly and restores a lost context

- **Status:** Accepted
- **Date:** 2026-11-04T14:30:00Z
- **Deciders:** Alison Jenkins
- **Supersedes:** ADR-0039 in part (the `glow` binding, and "a lost context is not restored")

## Context

ADR-0039 put the native UI's GPU layer on `glow` behind a `Surface` port and
let a lost context fall back to CSS for good. On Firefox for macOS
(2026-11-04) each visualiser open logged three warnings:

- `WEBGL_debug_renderer_info is deprecated in Firefox and will be removed`.
  `glow::Context::from_webgl2_context` calls `getExtension` for every
  extension it knows, this one included, and offers no way to skip it
  (0.18, and upstream `main` as of this date). The UI never reads the
  renderer string.
- `drawArraysInstanced: Drawing without vertex attrib 0 array enabled`: the
  fullscreen triangle came from `gl_VertexID` with no array enabled.
- `WebGL context was lost`: the old surface releasing its context when the
  player remounts for the next track. A real loss (a GPU reset, or a
  dual-GPU Mac switching GPUs) is handled the same way by the browser, and
  ADR-0039 gave it up for good.

## Decision

- The surface (`render/webgl.rs`) calls `web_sys::WebGl2RenderingContext`
  directly. It makes about twenty GL calls; `glow` is dropped.
- The vertex shader reads `a_corner` from a static buffer at location 0,
  enabled in the vertex array.
- On `webglcontextlost` the surface calls `preventDefault`, so the browser
  may restore the context, and skips frames. On `webglcontextrestored` it
  rebuilds its program, buffer and texture and draws on. A context that has
  not come back within 5 s still falls back with `context_lost`.
- Release on drop (`WEBGL_lose_context`) stays, so a remount does not hold
  dead contexts against the browser's cap. Firefox logs that release as
  "WebGL context was lost"; that line is expected and harmless.

## Consequences

- The wasm bundle drops `glow`'s bindings, and nothing asks for an
  extension the UI does not use.
- A later native or WebGPU backend shares no binding code with this one.
  That was ADR-0039's reason for `glow`, and it still sits behind the same
  `Surface` port.
- A brief GPU loss leaves the visualiser or backdrop blank for a moment
  rather than switching it to the CSS look until the next mount.
- Loss and restore each log a `pharos: webgl context` line.

## Alternatives considered

- **Keep `glow` and patch or fork it** so it requests extensions lazily: a
  carried patch on a crate the layer barely uses.
- **Keep `glow` and accept the warning**: Firefox says it will remove the
  extension, and the console line hides real problems.
- **Stop releasing the context on drop** to hide the "context was lost"
  line: each track would then leave a context alive until garbage
  collection, and Chromium drops the oldest one past 16 with its own
  warning.

## References

- ADR-0039
- specs/001-pharos-baseline/bugs.md B451, invariants.md V391
