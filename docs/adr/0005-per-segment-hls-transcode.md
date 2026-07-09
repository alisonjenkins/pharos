# ADR-0005: Per-segment on-demand HLS transcode + VP9-in-fMP4 for Firefox

- **Status:** Accepted
- **Date:** 2026-07-09T00:00:00Z
- **Deciders:** Alison

## Context

Browsers on codec-less Linux (Zen/Firefox) cannot decode H.264 in MSE — there is
no bundled licensed decoder — so pharos must transcode to a royalty-free codec
(VP9 + Opus) for those clients. A progressive `<video src>` WebM stream plays but
cannot seek to an unbuffered position and does not report a reliable resume
position. Native clients (Android/Google TV) accept H.264 HLS directly.

Two structural choices exist for serving HLS: Jellyfin's model — **one
continuous ffmpeg** per playback session with the HLS muxer owning segmentation
and timestamps — or **independent per-segment transcodes** where segment N is its
own `ffmpeg -ss N*6 -t 6` run.

## Decision

pharos serves HLS as **independent, on-demand per-segment transcodes**, and
delivers VP9-in-**fMP4** HLS to Firefox-class clients (H.264 to native clients).
Each `.m4s` is generated the moment it's requested and cached
(`HlsSegmentCache`); there is no whole-file pre-transcode and no per-session
ffmpeg process to manage.

fMP4 requires all media segments to share one init segment and carry a
*continuous* `baseMediaDecodeTime` (`tfdt`), but ffmpeg resets `tfdt` to 0 for
each independent run. `crates/pharos-server/src/api/jellyfin/fmp4.rs`
(`process_segment`) repairs this after ffmpeg: it splits the self-contained
fragmented mp4 into a shared init + media, and rewrites each fragment's `tfdt` to
`seg_index * 6 * timescale` per track so segments land at their true position on
the global timeline.

## Consequences

- **Random access is trivial:** any segment (including a seek target) is a
  standalone job — no "restart the encode at the seek point" machinery, no
  session state, no orphaned ffmpeg processes. Segments cache and replay.
- The cost is the `tfdt` surgery in `fmp4.rs` and its correctness burden.
- **A/V sync was investigated in depth (2026-07)** against real deployed
  segments: the model holds up. Video+audio are cut together per segment with
  ~0ms start-skew; modern ffmpeg input-seek (`-ss` before `-i`) is *frame
  accurate under re-encode* (it only keyframe-snaps under stream-copy); measured
  `tfdt`s are aligned and continuous across segments with no drift. The
  per-segment `tfdt`-reset actively *prevents* cross-segment accumulation. See
  memory `project_vp9_hls_avsync_findings` — the model is not a drift source, so
  do not "fix" it speculatively.
- Pixel formats are encoder-specific and must be set explicitly (mjpeg needs
  full-range `yuvj420p`; H.264/HEVC force `yuv420p`; VAAPI uploads `nv12`).
- If a continuous-transcode need ever arises (e.g. a client that can't tolerate
  the per-segment boundaries), it would be a new ADR superseding this one.

## Alternatives considered

- **Continuous transcode (Jellyfin model):** correct-by-construction timestamps,
  but requires per-session process lifecycle + seek-restart logic and holds a
  running ffmpeg per viewer. Rejected for the on-demand model's statelessness;
  reconsider only if per-segment artefacts prove unfixable.

## References

- `crates/pharos-server/src/api/jellyfin/fmp4.rs`, `hls.rs`
- `crates/pharos-transcode/src/lib.rs` (`build_args_for_device`)
- memory `project_vp9_fmp4_hls`, `project_vp9_hls_avsync_findings`
