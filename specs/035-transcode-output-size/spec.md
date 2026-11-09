# Spec 035 — Transcode output size

- **Status:** Built (this branch).
- **Date:** 2026-11-09T22:00:00Z
- **ADR:** [0092](../../docs/adr/0092-transcodes-downscale-to-the-clients-size-and-bitrate.md)
- **Ids:** B637, V589–V592, T153.

## Problem

Every video transcode encodes at the source's resolution. A 4K source sent to a
1080p screen, or to a client that capped its bitrate at 2 Mbps, costs the
server a 4K decode, a 4K tonemap and a 4K encode for pixels nobody sees, and
the client more bandwidth than it asked for. Measured on 6 s of 4K HDR10 with
the CPU chain (x264 veryfast, 4 threads): 23.7 CPU-s unscaled, 18.0 with the
scale after the tonemap, 9.4 with it before (-60%).

## What clients send

| Where | What | Meaning |
|---|---|---|
| master / variant / segment / stream URL | `MaxWidth`, `MaxHeight` | bounding box for the frame |
| same | `Width`, `Height` | read as the same kind of bound; a transcode never upscales |
| same (baked by PlaybackInfo) | `VideoBitrate` (from `MaxStreamingBitrate`) | the client's bitrate cap |

`playback_qs` forwards the size keys master -> variant -> segment, as it does
`VideoBitrate`. Where a key repeats the tightest value wins. Zero, junk and
values above 8192 read as absent.

## Behaviour

1. `OutputSize` (`pharos-transcode`) is the typed target: even edges, 64..=8192.
   `plan()` never upscales, keeps the aspect ratio, rounds each edge down to a
   multiple of 2 (4 for a packed 3D source, so each eye stays even), and
   returns `None` when nothing would change.
2. **Bitrate ladder (step 2, built).** When the client capped the video bitrate
   (a negotiated cap or a ladder rung, never a bitrate that merely follows a
   low-bitrate source), the output height is bounded by the tallest rung the
   bitrate can carry: 2160 >= 6 Mbps, 1440 >= 3.5, 1080 >= 1.8, 720 >= 0.9,
   else 480. The client's box wins when tighter.
3. A named ladder rung (`720p`, ...) bounds the height by its own height.
4. Filter placement:

   | Route | Where the scale goes |
   |---|---|
   | CPU chain (software, NVENC, QSV, VideoToolbox, VAAPI hybrid) | first, before the tonemap and the 3D mono crop |
   | text subtitle burn | before the burn, so libass draws at output size |
   | bitmap subtitle burn (PGS/VOBSUB) | after the overlay: its coordinates are in source pixels |
   | per-eye 3D burn | on the whole frame before the eye split (text), after the stack (bitmap) |
   | resident VAAPI | `scale_vaapi=w=..:h=..` |
   | GPU tonemap (libplacebo) | `w=..:h=..` inside the libplacebo filter |
   | filler window | generated at the output size |

   NVENC keeps NVDEC on a plain scaled re-encode: `-hwaccel_output_format cuda`
   and `scale_cuda=W:H:format=nv12`, no `-pix_fmt`. A tonemap, subtitle burn or
   3D crop needs system-memory frames, so those jobs decode in software and use
   the CPU `scale` (same rule as the 3D filters, B240). The cuda route is
   string-tested; it has not run on a GPU.
5. `SegmentIdentity` carries the size. The cache file name gains `_{W}x{H}`
   only when scaled, so every unscaled name (and ETag) is unchanged.
   `RenditionKey` serialises the size only when set, so unscaled renditions do
   not re-place.
6. Signal: `pharos_transcode_output_scaled_total{reason=max_size|bitrate}`,
   pre-registered, counted once when a scaled transcode starts (the HLS cache's
   encode, the progressive stream), never on a cache hit, an init segment or a
   prewarm. A debug line carries source size, output size, reason and bitrate.
7. A packed 3D source downmixed to 2D is bounded on the eye the client
   receives, not on the packed frame the scale runs on (`plan_delivered`).
8. The master advertises the size the `main` rendition is actually encoded at
   and leaves out rungs taller or wider than the client's bounds.
9. PlaybackInfo reads `MaxWidth`/`MaxHeight` from its query and bakes them into
   the transcoding URL, so the cold-start prewarm plans the same size and
   `capped` flag as `serve_segment` (`output_size_bounds`).

## Out of scope / follow-ups

- The master cannot see a source's 3D layout, so a packed-3D `main` rendition's
  advertised `RESOLUTION` is planned as flat (cosmetic).
- A bitmap-subtitle burn runs the overlay at source size.
