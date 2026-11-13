# ADR-0123: H.264 sources are copied into segments cut on their own keyframes

- **Status:** Proposed
- **Date:** 2026-11-13T06:40:00Z
- **Deciders:** Alison Jenkins

## Context

- Browser-class clients cannot open Matroska. For an H.264 source they get `video_remux` (block=container), and the segmented surface re-encodes every 6 s segment on the GPU (V30: `SegmentVideo` has no copy variant).
- That re-encode is the main buffering cost on the TV. Segments come out at about 1.8-1.9x realtime. At peak both encode devices were saturated with up to 127 jobs pending, and the coalesced-wait p95 reached ~35 s (telemetry 2026-11-05 to 2026-11-12).
- V30 forbids copy for a reason that still holds. On 2026-11-13, a synthetic fixture was cut with `ffmpeg -ss K -t D -c:v copy` on its exact keyframe times, using ffmpeg 8.1. The segments did not tile the source's packets: two segments carried 2 extra packets each, and one carried 90 of its ~148.
- The uniform 6 s grid (`segment_grid.rs`) cannot be copied at all: a copied segment can only start on a source keyframe.

## Decision

We will add a separate surface, `h264copy`, that serves an eligible H.264 source's video packets unchanged.

1. **Pharos picks the packets.** The transcode worker reads the source with libav, in the crash-isolated worker (V6). Segment n is every video packet in decode order from keyframe K_n up to, but not including, K_n+1. No ffmpeg CLI cutting.
2. **Segments follow the source's keyframes,** grouped to about 6 s and at most 12 s. The media playlist lists each segment's exact duration. This surface does not use the uniform grid; every other surface keeps it.
3. **Pharos writes the fMP4 boxes itself:** `avcC` copied from the source; `tfdt` = K_n; decode times = the segment's presentation times sorted; signed composition offsets (trun v1). The libav mp4 muxer is not used, because its edit lists and timestamp shifts are what broke copy before.
4. **Only closed-GOP, indexed, plain H.264 in Matroska is eligible:** 8-bit 4:2:0, progressive, Baseline/Main/High, with no burn-in, cap or tone map. Anything else stays on the encode surface.
5. **Audio and the ABR ladder are unchanged.** Audio stays the demuxed continuous rendition on the 6 s grid. The 360p/720p rungs stay encoded.
6. **It is off by default.** `[server].hls_copy_surface` must be on, and the client must opt in with `copy=1`. Slice 1 is the TV app's hls.js only.
7. **V30 is narrowed, not dropped.** Copy stays forbidden on every surface cut by the CLI. `SegmentVideo` still has no copy variant; copy is its own type and route.

## Consequences

- Eligible sessions use no GPU time for video and no encode queue slot. A segment costs one file read: milliseconds instead of seconds.
- The encode devices are freed for sources that need them.
- Picture quality equals the source, so the copy variant's BANDWIDTH is the source bitrate. On a weak link the player's ABR can move to the encoded rungs; that switch is not yet tested (below).
- Segment durations vary (4-12 s). Players must accept variable EXTINF. hls.js does; the Rust HLS engine (spec 031) must be checked before it opts in.
- Variant switches between copy and encoded rungs land on different segment boundaries. Both timelines are source-anchored, so the player should see one timeline. No test covers an ABR switch yet; it must be checked before the gate turns on.
- There is no in-playback fallback. A copy segment that fails at serve time marks the source refused, and only the next master load offers the encode surface. Within a playback, a failed copy segment is not replaced by an encoded one.
- New code: a small fMP4 box writer and a keyframe-index operation. Both are pure or isolated, and testable on synthetic fixtures.
- Seeking lands on a segment start, up to 12 s before the target, the same as a keyframe seek in a progressive file.

## Alternatives considered

- **ffmpeg CLI `-c copy` per segment.** Measured above: not packet-exact.
- **libav mp4 muxer per segment.** Fewer lines, but it applies edit lists and timestamp shifts per output. Pharos would have to undo them for every segment, the class of fault V30 records.
- **One continuous remux (`-f hls -hls_time 6 -c copy`).** Packet-exact, but it is a live process per session from the start, it cannot start mid-file without breaking cache reuse, and a seek restarts it. It reintroduces the live-session model ADR-0100 moved away from.
- **Progressive MP4 remux on `/stream`.** Already used where allowed. It cannot carry the ladder or the demuxed audio, and the TV's hls.js path needs HLS.

## References

- Spec 041 (`specs/041-hls-copy-surface/`).
- V30 and B45 (`specs/001-pharos-baseline/`), ADR-0100 (session-scoped segment routes), spec 031 (Rust HLS engine).
