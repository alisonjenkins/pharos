# ADR-0111: A slow visualiser draws at a lower resolution

- **Status:** Accepted
- **Date:** 2026-11-10T09:05:00Z
- **Deciders:** Alison Jenkins

## Context

The music visualiser's WebGL2 shaders are filled once per pixel per frame. On
the Android TV (MediaTek mt5896, Mali-G52, page 960x540 CSS at device pixel
ratio 2, canvas 1200x675 under the 1.25 ratio cap) Starfield ran at 21-25 fps,
and Alison found it choppy (2026-11-10). The CPU was not the limit: the wasm
draw, GL calls and audio analysis took about 3% of a core. Shrinking the canvas
by hand raised Starfield to 45 fps at 853x479 and 60 fps at 600x338. Spectrum
and Plasma already ran at 60 fps at full size. Weak devices are a target
(desktop, Quest, Steam Frame, Android TV), and their GPUs differ, so no fixed
size per style or per device would fit all of them.

## Decision

Each visualiser run measures its own frames. Over each window of 60 drawn
frames it takes the median interval, leaving out gaps over 250 ms (a pause, a
hidden page, a style load). A median over 20 ms multiplies the run's share of
the pixel ratio by 0.8, with a floor of 0.5. The browser scales the canvas up
to its box. The share only goes down within a run, and the next style or
screen starts again at full size. `render::resolution::Resolution` is the pure
rule, and `render::webgl::WebglSurface::fit` applies it. Each step down logs
`visualiser resolution lowered program=… scale_from=… scale_to=…
median_frame_ms=…`.

## Consequences

- A style the GPU can fill keeps full sharpness everywhere. One it cannot fill
  trades sharpness for frame rate, which suits the soft shaders.
- A run takes about a second per step to settle (three steps from 1.0 to 0.51
  at 20 fps), and those first seconds stay choppy.
- The rule never raises the share again in a run. A run slowed by something
  passing, such as another tab, stays smaller until the style or screen
  changes. The median over a window keeps one-off stalls from counting.

## Alternatives considered

- A fixed lower pixel ratio on the TV: the styles that fill at full size would
  lose sharpness for nothing, and other weak devices would need their own
  numbers.
- Raising the share again once frames are fast: the share would swing between
  two sizes at the edge, which shows as a pulsing blur. A hysteresis rule was
  judged not worth it while the floor is mild.
- Simpler shaders for the TV: possible later, but each style would need a
  second version to keep in step.

## References

- B1218, V1178 (`specs/001-pharos-baseline/`), ADR-0104 (the TV page runs the
  visualiser as wasm).
