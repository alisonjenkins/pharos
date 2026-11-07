# ADR-0070: HDR sources are tonemapped to SDR in software on every video transcode

- **Status:** Accepted
- **Date:** 2026-11-07T00:00:00Z
- **Deciders:** Ali

## Context

Every video transcode pharos builds outputs 8-bit 4:2:0 (`-pix_fmt yuv420p`, or `format=nv12,hwupload` on VAAPI) and sets no HDR metadata. An HDR10 (PQ) or HLG source went through that unchanged: the BT.2020/PQ code values were read as BT.709, so the picture was washed out. The negotiator can now keep an HDR source off players that cannot show it (`VideoRangeType`, T065), but a player that is sent to transcode still received the broken picture.

The devShell ffmpeg, which is the `pkgs.ffmpeg-headless` the `.#oci` image carries, has `zscale` (libzimg), `tonemap`, `tonemap_vaapi` and `tonemap_opencl`, and has libplacebo disabled. Production transcodes on an AMD RX 9070 through VAAPI (Mesa radeonsi). `tonemap_vaapi` needs the Intel iHD driver in practice, and `tonemap_opencl` needs an OpenCL stack the flake does not add to the image, so neither works there.

## Decision

- One portable software chain, `zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,tonemap=tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv`, followed by `format=yuv420p` (software, NVENC, QSV, VideoToolbox) or `format=nv12` (VAAPI, ahead of `hwupload`). A tonemapped job decodes in software on every device (`DecodeAccel::of` answers `NoDecoderForAccel`), so the filters always receive system-memory frames; the encoder still runs on the GPU. No hardware tonemap is used.
- `pharos_transcode::Tonemap { Off, HdrToSdr }` is a field of `TranscodeOptions` and `SegmentOpts`. The arg builder alone turns it into filters (`Tonemap::filters`), so no caller assembles the string.
- Tonemap whenever the source is HDR (`MediaProbe::video_range_type()` is `HDR10` or `HLG`) and the video is re-encoded. Every encoder pharos drives emits 8-bit 4:2:0 with no HDR signalling, so there is no transcode whose target is HDR; "the target is SDR" is always true and no per-device-profile check is needed. Any re-encode, including the HLS video-remux decision that re-encodes to H.264, is 8-bit SDR and is therefore tonemapped. Only a path that keeps the original video bytes (direct play, direct stream, stream copy) preserves HDR for a player that can show it.
- Not tonemapped: stream copy, filler windows (synthetic frames), SDR sources. A tonemapped job never takes the GPU-resident VAAPI pipeline, which has no CPU filters.
- The chain runs before a text or image subtitle is burned, so libass colours are not pushed through the PQ curve. The per-eye 3D burn graphs run it on the whole frame ahead of the per-eye split, so neither eye's subtitle copy goes through the curve either.
- Cache: `SegmentIdentity.tonemap` adds `_tm` to the cache filename (and therefore the ETag) when on. `Off` adds nothing, so no SDR entry is renamed and `HLS_GEN_VERSION` is not bumped. An HDR segment cached before this change sits under the bare name and is never read for a tonemapped request.
- Signal: `pharos_transcode_tonemap_total{tonemap,source_range}`, one sample per segment or progressive request, created at zero at startup. SDR is counted as `off`.

## Consequences

- Measured on this machine (32 threads, libx264 `veryfast`, synthetic x265 10-bit PQ `testsrc2`): 1080p 316 fps untonemapped, 141 fps tonemapped; 2160p 68 fps untonemapped, 30 fps tonemapped (24 fps source). Tonemapping costs 2.2x at either size; a 4K title stays above real time on a CPU with these cores but takes several cores per stream, and the cost lands on the CPU even when the GPU encodes.
- Float conversion makes a 4K HDR stream the most expensive software job the scheduler runs. The device scheduler does not know that yet.
- The `tonemap` filter is a global curve; it has no per-scene adaptation. `desat=0` keeps saturated highlights. Quality is acceptable for a fallback, not a reference grade.
- Dolby Vision and HDR10+ are not told apart by the probe (`video_range_type`), so a DV profile 5 stream, whose base layer is not PQ-compatible, is not detected here and will still look wrong.
- A build of ffmpeg without libzimg fails every HDR transcode. The nixpkgs build has it.

## Alternatives considered

- `tonemap_vaapi`: Intel-only in practice; no use on the RX 9070.
- `tonemap_opencl`: needs an OpenCL runtime and a hwupload round trip, and the flake adds no OpenCL runtime to the image.
- libplacebo: disabled in the nixpkgs build, and needs Vulkan.
- Leave it to the player: the transcoded stream carries no HDR tags, so a player cannot.
- Keep HDR in the output (10-bit HEVC with PQ tags): clients that need a transcode are the ones that cannot show it.

## References

- T065 (`enforce_video_range`), `crates/pharos-transcode/src/options.rs` (`Tonemap`)
- ffmpeg filters documentation: `zscale`, `tonemap`
