# ADR-0092: Transcodes downscale to the client's size and bitrate

- **Status:** Proposed
- **Date:** 2026-11-09T22:00:00Z
- **Deciders:** Alison Jenkins

## Context

Pharos has always encoded video at the source's resolution. Jellyfin clients
send `MaxWidth` / `MaxHeight` and a bitrate cap, and a 4K source going to a
1080p screen or a 2 Mbps link is wasted decode, tonemap and encode work. On
the CPU HDR chain the tonemap alone dominates: 6 s of 4K took 23.7 CPU-s
unscaled, 18.0 when scaled after the tonemap and 9.4 when scaled before it.

The size changes the pixels, so it has to be part of the segment's identity,
and it has to reach the encoder through every filter route (CPU, VAAPI hybrid
and resident, the GPU libplacebo tonemap) without moving any existing key.

## Decision

We will downscale a video transcode to the client's bounding box and to the
height its capped bitrate can carry, and never upscale.

- A typed `OutputSize` (even edges, bounded) is planned from the probe by one
  function; the server only collects the client's bounds.
- The scale runs first in the CPU chain, before the tonemap. It follows a
  bitmap-subtitle overlay (source-pixel coordinates) and is folded into
  `scale_vaapi` / libplacebo `w`,`h` on the GPU routes.
- The size is part of `SegmentIdentity` and the cache file name only when set,
  and is serialised into `TranscodeOptions` JSON only when set, so unscaled
  cache names, ETags and rendition keys are byte-identical to before.
- A bitrate far below the source height's need picks a lower rung of a fixed
  ladder, but only when the client capped the bitrate.

## Consequences

- A 4K to 1080p HDR transcode costs roughly 40% of the CPU it did.
- A named ladder rung (`720p`) now encodes 720 tall, not only at 720p's bitrate.
- Scaled segments are separate cache entries from unscaled ones: a client that
  changes its bound re-encodes.
- NVENC keeps NVDEC on a plain scaled re-encode and scales with `scale_cuda=W:H:format=nv12`
  (`-hwaccel_output_format cuda`, no `-pix_fmt`). A tonemap, a subtitle burn or a
  3D crop still needs system-memory frames and falls back to software decode and
  the CPU `scale`. The `scale_cuda` route is covered by argv tests only; it has
  not run on a GPU.
- The ladder floors are judgement, not measurement; they are constants in
  `output_size.rs` and a new ADR is not needed to tune them.
