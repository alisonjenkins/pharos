# ADR-0041: A subtitle burned into packed 3D is drawn into each eye

- **Status:** Accepted
- **Date:** 2026-11-02T00:00:00Z
- **Deciders:** Alison Jenkins

## Context

Burning a subtitle into a frame-packed 3D source (half or full side by side,
half or full top and bottom) used to force `StereoOutput::Mono2D`: V183 held
that a subtitle drawn once across the packed frame is split between the eyes.
The cost fell on the viewer, who lost 3D for the whole session by turning on
subtitles, and the rule lived in six places (negotiation, the transcode
backstop, the segment-opts defaults, the progressive stream and the segment
cache identity).

ffmpeg 8.1 can do better with stock filters: `split`, `crop`, `subtitles` or
`overlay`, `hstack`/`vstack`. A half layout squeezes each eye 2x on one axis,
so a subtitle drawn on the packed eye would show stretched.

## Decision

We will keep the packed layout when a subtitle burns and draw the subtitle into
each eye (V223).

- The graph is split, crop per eye, scale the eye to full aspect on a half
  layout, burn, scale back, `hstack` (side by side) or `vstack` (top and
  bottom). A text burn wraps the existing `setpts` seek sandwich around the
  whole graph; an image burn splits the bitmap track once per eye. The
  VAAPI/hardware upload tail applies once after the stack.
- `StereoOutput::Mono2D` is chosen only by `StereoMode=mono`/`mono2d` or a
  profile that rejects 3D, and keeps its left-eye crop then burn. MVC is not
  packed and is unchanged.
- The decision is reported as `reason="burn_per_eye"` in
  `pharos_playback_3d_decision_total` and the `pharos::playback::3d` log.
- The segment cache identity follows the requested output alone. A per-eye
  segment is named by its subtitle index, so it never shares a path or ETag
  with the unburned segment or with a `_mono2d` one; segments cached by older
  builds under `_mono2d` are not served to a `preserve` request.

## Consequences

- A viewer keeps 3D with subtitles on. A burn costs one split, two crops and a
  stack per frame on the CPU, plus two scales on half layouts, on top of the
  existing burn; NVENC falls back to software decode for it, as it already did
  for the Mono2D crop.
- The text graph is now a `-filter_complex` that owns the video and audio maps,
  not a `-vf` chain.
- The subtitle is placed relative to each eye, not the packed frame. A bitmap
  subtitle authored for the whole packed canvas (rather than for one eye) is
  misplaced; the real-ffmpeg test executes the text graph only, so the image
  graph's shape is asserted but not run.
- Reverting is expensive: clients that rely on 3D with subtitles, and cached
  per-eye segments, depend on it, so a return to Mono2D needs a new ADR.

## Alternatives considered

- Keep forcing Mono2D (V183): simplest, loses 3D for subtitle users.
- Burn one copy in the left eye only: leaves the right eye without text.
- Draw once across the packed frame with a stereo offset: needs per-title depth
  data pharos does not have.

## References

- `specs/001-pharos-baseline/invariants.md` V223 (supersedes V183's burn rule)
- `specs/001-pharos-baseline/bugs.md` B284
- `crates/pharos-transcode/src/lib.rs` `push_video_filters`,
  `crates/pharos-transcode/src/options.rs` `per_eye_layout`
