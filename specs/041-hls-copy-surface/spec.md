# Feature specification: serve H.264 sources as copied fMP4 segments, cut on their own keyframes (041)

**Created**: 2026-11-13T06:30:00Z
**Status**: Draft. Slice 1 in progress, behind a gate that is off by default.
**Scope**: A new segmented HLS video surface, `h264copy`. It serves an H.264 source's video packets unchanged, in fMP4, with segments cut on the source's own keyframes. Audio, subtitles, the downgrade ladder and every other surface are unchanged.
**Decision record**: ADR-0123. **Bugs reserved**: B1460-B1469. **Verification**: V1430-V1439.

---

## Problem statement

- **Browsers re-encode video that is already H.264.** A browser-class client (the TV app's WebView with hls.js, desktop Chrome) cannot open a Matroska file. The server therefore decides `video_remux` (block=container), and the segment surface re-encodes every 6 s of video into H.264 again on the GPU.
- **The re-encode is the main buffering cost.** It runs at about 1.8-1.9x realtime per segment. Telemetry from 2026-11-05 to 2026-11-12 put the coalesced-wait p95 at up to ~35 s on a busy day. Both encode devices sat saturated at peak, with up to 127 jobs pending.
- **H.264 in Matroska is the largest single group** of video files in the library. H.264 in MP4 adds more.
- **The re-encode buys nothing for these files.** The client can decode the source's video as it is; only the container is wrong.

## Why the existing surfaces cannot copy

V30 (B45, 2026-07-14) forbids stream copy on the segmented surface, and that stays true for every surface that cuts with the ffmpeg CLI. A check on 2026-11-13 confirmed it still holds on ffmpeg 8.1:

- A synthetic H.264 Matroska fixture, with irregular keyframes and B-frames, was cut with `ffmpeg -ss K_n -i src -t (K_n+1 - K_n) -c:v copy -copyts`. The cut points were exactly on the source's keyframes.
- The segments did not tile the source's packets.
  - Segments 0 and 1 carried 2 extra packets each.
  - Segment 2 carried 90 packets where its keyframe range holds ~148.

Input seek plus `-t` under copy does not give packet-exact segments. A copy surface therefore has to choose its own packets.

## The approach

1. **Keyframe index.**
   - The transcode worker opens the source and reads its seek index (Matroska Cues). Slice 1 is Matroska only: an MP4 index holds decode times, which differ from keyframe presentation times when the file has B-frames. It lists the video keyframes' presentation times.
   - It validates the index against the packets. The first packet read at each chosen keyframe must be a keyframe at that exact time.
   - The source's first video packet must be a keyframe at the first indexed keyframe. Segment 0 starts there, so a packet before it would land in no segment; such a source is refused (`leading_packets`).
   - It reads the last segment's packets. When they run past the container's duration, the plan's end moves to one frame after the last packet, so the advertised durations cover every packet. A last segment found at serve time to run past the plan's end fails rather than being stretched.
   - No index, or an index that fails validation, means the item is not eligible.
2. **Segment plan.**
   - A pure function groups the keyframes into segments of about 6 s. A segment never ends before 4 s unless it is the last one.
   - Rule: from each segment's start, end on the keyframe between 4 s and 12 s later that is closest to 6 s (the earlier on a tie).
   - The rest becomes the last segment once it fits in 12 s and cannot be split into two parts of at least 4 s each.
   - The plan is not eligible if any segment would be longer than 12 s.
   - The playlist advertises each segment's exact duration. TARGETDURATION is the ceiling of the longest segment.
3. **Packet rule.**
   - Segment n holds the video packets in decode order, from the keyframe at K_n up to but not including the keyframe at K_n+1.
   - Every video packet of the source lands in exactly one segment, byte for byte.
4. **Timing.**
   - A segment's decode times are its packets' presentation times sorted ascending. So its first decode time, and its `tfdt`, equals K_n.
   - Each sample's duration is the gap to the next decode time. The last sample's gap is to K_n+1.
   - Composition offsets are `pts - dts`, written as signed values (trun version 1). So segment n ends exactly where n+1 begins.
5. **Open GOPs are not eligible.**
   - A segment whose smallest presentation time is below K_n starts with frames that reference the previous segment.
   - The index check reads the first GOPs and rejects such sources.
   - A segment start must be an IDR picture. A non-IDR intra picture (a recovery point, which Matroska also marks as a keyframe) is not an independent access point, even when no presentation time precedes K_n.
   - At serve time, a segment found to break the rule fails with 500 and is logged and counted (`pharos_copy_segment_total{outcome="failed"}`).
   - That failure marks the source refused, so the next master load for it offers the encode surface. The two surfaces cut on different grids, so a failed copy segment cannot be swapped for an encoded one mid-playlist.
   - Pharos does no recovery inside the playback. After the player's own bounded retries, playback stops at that segment, unless the player's ABR moves to an encoded rung. Reloading the item gets the encode surface.
6. **fMP4 writing.**
   - Pharos writes the boxes itself: the init (`ftyp` + `moov` with `avc1`/`avcC` copied from the source's codec private data) and per-segment `moof` + `mdat`.
   - It uses neither the libav mp4 muxer nor the ffmpeg CLI, so every timestamp is under Pharos's control.
7. **Audio is unchanged.** The demuxed continuous-audio rendition keeps its uniform 6 s grid. HLS media playlists need not share a segment grid.
8. **The ladder is unchanged.** The 360p/720p downgrade rungs stay encoded. Their timestamps are source-anchored, so switching between a copy variant and an encoded rung keeps one timeline.

## Eligibility

A play session gets the copy surface only when all of these hold:

- the gate is on (`[server].hls_copy_surface = true`), and the client opted in with `copy=1` on the master playlist URL (slice 1: the TV app's hls.js only);
- the decision is `video_remux` or a browser H.264 transcode where nothing changes the picture:
  - the source video is H.264, 8-bit 4:2:0, progressive;
  - the profile is Baseline, Main or High;
  - no burned-in subtitles;
  - no bitrate or height cap below the source;
  - no tone mapping;
- the keyframe index exists and passes validation (closed GOPs, longest segment at most 12 s);
- CODECS is built from the source's `avcC` bytes (`avc1.PPCCLL`), not from the database's profile name.

Everything else keeps today's encode surface, unchanged.

## Requirements

- **FR-001**: The transcode worker exposes a `keyframe_index` operation. It returns the video keyframe times, the time base, the `avcC` bytes and the validation verdict. The libav demux runs in the crash-isolated worker (V6), never in the server process.
- **FR-002**: The transcode worker exposes a `copy_segment` operation. It returns the decode-order packets (bytes, pts, dts, keyframe flag) for one keyframe range.
- **FR-003**: A pure segment planner turns keyframe times into segment ranges, following the approach above. It has property tests: ranges tile the timeline, there are no gaps, and the bounds hold.
- **FR-004**: A pure fMP4 writer builds the init and media segments from the `avcC` bytes and packets.
- **FR-005**: Routes:
  - `GET /videos/{id}/h264copy/main.m3u8`
  - `GET /videos/{id}/h264copy/init.mp4`
  - `GET /videos/{id}/h264copy/{seg}.m4s`
  - Each checks the play session like the other segment routes (ADR-0100, a new `GoneRoute`).
  - The playlist and segment URLs carry a plan token, so a changed source cannot be mixed with cached segments.
- **FR-006**: The master playlist lists `h264copy` in place of the h264cmaf main variant when the session is eligible. The demuxed audio rendition and the downgrade rungs stay as they are.
- **FR-007**: The cold-start prewarm and background prefetch start no GPU encodes for a copy session.
- **FR-008**: Each copy segment served logs one line: item, seg, packets, bytes, read_ms, plan token. Each refusal of eligibility logs one line with its reason.
- **FR-009**: V30 is amended. Stream copy stays forbidden on every CLI-cut surface. It is allowed only on `h264copy`, whose packets are chosen by Pharos.

## Verification (behavioural, per the V30 corollary)

- **V1430**: Packet identity.
  - For synthetic fixtures (irregular keyframes, B-frames, a sub-second GOP, Matroska sources), concatenate init plus all segments.
  - The video packets' `framemd5` must equal the source's (`ffmpeg -c copy -f framemd5`), packet for packet.
- **V1431**: `tfdt` of segment n+1 equals `tfdt` + the sum of sample durations of segment n, for every n.
- **V1432**: The planner's property tests: ranges tile the timeline, there are no gaps, and segments are between 4 s and 12 s except the last.
- **V1433**: Chromium plays the surface through hls.js under Playwright. That includes seeking into a segment that is not the first.
- **V1434**: An open-GOP fixture is refused, and its session gets the encode surface.
- **V1435**: With the gate off, or without `copy=1`, the master playlist is byte-identical to today's.

## Slices

1. **Slice 1** (this branch):
   - worker operations, planner, writer and routes;
   - the master gate (off by default) and the prewarm/prefetch skip;
   - the V1430-V1435 checks;
   - no client changes except the TV app adding `copy=1`, which needs a TV build.
2. **Slice 2**:
   - turn the gate on after a live check on the TV;
   - add desktop Chrome through the Rust HLS engine (spec 031), after checking it handles variable EXTINF.
3. **Later**:
   - HEVC copy for clients that decode HEVC in MSE;
   - an MP4-source fast path;
   - persisting the index.

## Out of scope

- AVI and other sources without a seek index (see B1162+).
- Firefox on Linux, which does not decode H.264 in MSE and keeps VP9.
- Changing audio delivery.
