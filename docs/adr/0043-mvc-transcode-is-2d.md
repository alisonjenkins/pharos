# ADR-0043: A transcode of Blu-ray 3D (H.264 MVC) is 2D

- **Status:** Accepted
- **Date:** 2026-11-02T12:00:00Z
- **Deciders:** Alison Jenkins

## Context

Blu-ray 3D stores the second eye as an H.264 MVC extension (NAL types 15, 20) next to a plain H.264 base view. pharos detects it as `Video3DFormat::Mvc` and serves it by direct play to clients that can decode it.

ffmpeg 8.1, the transcoder, has no H.264 MVC decoder. It decodes the base view and ignores the extension NALs. So every transcode of an MVC source outputs 2D, whatever `StereoOutput` asks for: `stereo_mono_filter` returns `None` for `Mvc` and `Preserve` adds nothing. A packed source under `StereoOutput::Mono2D` is also 2D, by design (a left-eye crop scaled back up). B290 was PlaybackInfo advertising `MVC` for a delivery that yields 2D.

## Decision

We will treat "MVC transcode is 2D" as a fixed constraint and make pharos say so:

- `pharos_transcode::Output3D::of(format, effective_stereo_output)` is the one answer to what a transcode does to a 3D source (`preserved`, `flattened_mvc`, `flattened_mono`). PlaybackInfo, the 3D decision metric and log, and the `stereo_mode` metadata clear in `build_args` all use it.
- A MediaSource whose delivery is a flattened transcode advertises no `Video3DFormat`. Direct play and direct stream keep the source format.
- No pharos component tries to rebuild the second view.

## Consequences

- A Blu-ray 3D rip is 3D only for a client that direct-plays it. A client that cannot, or a browser, gets the 2D base view and is told it is 2D.
- `pharos_playback_3d_decision_total{output="flattened_mvc"}` measures how often that happens; a high rate is the case for revisiting this ADR.
- The answer composes with `effective_stereo_output`, so a change to when `Mono2D` is forced (such as subtitle burn) needs no change to the advertised format.

## Alternatives considered

- **An external MVC decoder such as edge264.** It decodes the extension view, but it is not integrated: pharos would need a new decode path, a packaging story and a way to feed two views into the ffmpeg encode. Not built.
- **MV-HEVC view packing.** ffmpeg can decode MV-HEVC views, so a re-encode could pack them. pharos does not detect MV-HEVC and has no sample file, so there is nothing to build or test against. Not recorded as tried.

## References

- `crates/pharos-transcode/src/options.rs` (`Output3D`, `stereo_mono_filter`)
- `crates/pharos-server/src/api/jellyfin/items.rs` (PlaybackInfo)
- `specs/001-pharos-baseline/bugs.md` B290, `invariants.md` V233
