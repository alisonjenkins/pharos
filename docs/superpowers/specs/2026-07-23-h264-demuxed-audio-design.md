# h264 Demuxed-Audio HLS (seamless audio-track switching) — Design

## Problem

Switching the audio track mid-playback in a SyncPlay group **wedges the group
in a 15–20 s freeze/restart/rewind thrash**. Live evidence (group
`4186f5db`, 2026-07-23): a member swapped audio `1→2`, the new rendition's
first segment took ~14 s to encode, jellyfin-web restarted the player 4×
(4 PlaySessionIds, `499` cancels), and each restart buffering-froze the
whole group (V19), with a stray `Seek(0)` rewind. It recovers (no 30 s
anti-wedge fires) but the thrash reads as "stuck."

### Root cause

The **h264 HLS path muxes audio into the video `.ts` segments**
(`hls.rs:534`: *"the h264 rungs mux their audio into the mpegts segments"*).
The segment cache key includes `audio_index`
(`hls_cache.rs` `SegmentKey{… audio_index …}`), so selecting a new audio
track is a **cache miss on every video segment → a full cold video
re-transcode from the playhead**. jellyfin-web reloads the master on an
audio switch (`DELETE /Videos/ActiveEncodings` + new
`master.m3u8?AudioStreamIndex=…`), and with a muxed cache key that reload
cannot reuse the video it already produced.

The **VP9 path does not have this problem**: its video segments are
audio-free (`vp9_segment_opts`: `audio: None`) and audio is a **separate
continuous rendition** (`vp9_audio_playlist` → `ensure_audio_hls_covering`,
keyed on `audio_index` in `_audiohls/{media_id}-a{a}-b{br}`). Switching
audio there re-spins only the cheap audio session; the video cache is
untouched. The h264 family never got this treatment.

## Goal

Give browser h264 clients the same demuxed-audio model VP9 already has, so an
audio-track switch reuses the cached video and re-spins only the cheap
continuous audio rendition — eliminating the cold re-transcode, the client
restart storm, and the SyncPlay group thrash.

## Approach

**Mirror the VP9 CMAF model for h264.** Add an h264-in-fMP4 (CMAF)
**video-only** rung that references a **demuxed audio group**, and route
browser h264 clients to it. Native / muxed clients are untouched.

Why fMP4 (not audio-free mpegts + a demuxed group): an all-fMP4 master
(video-only h264 fMP4 + fMP4 audio group) is the exact, in-prod-proven
structure the VP9 rung already uses with hls.js. Mixing mpegts video with an
fMP4 audio group, or muxed and demuxed models in one master, is the failure
class that caused the mixed-codec buffering outage (`hls.rs:537`). Staying
all-fMP4 avoids it.

### Components

1. **New h264-CMAF video surface** — `master`/`variant` media playlists +
   init + segment routes producing **video-only h264 fMP4** segments
   (`SegmentContainer::Fmp4`, `SegmentVideo::H264`, `audio: None`). This is
   the VP9 surface with the video codec swapped from `Vp9` to `H264`; the
   segment machinery, fMP4 `tfdt` correction, frame-aligned EXTINF grid,
   subtitle-burn gating, and 3-tier cache all carry over unchanged. Because
   `audio: None` sets `audio_index → None` in the cache key, **all audio
   tracks share one video encode** — the whole point.

2. **Shared demuxed audio rendition** — reuse the existing
   `ensure_audio_hls_covering` / `vp9_audio_playlist` machinery (Opus-in-fMP4,
   already keyed on `audio_index`, plays in Chrome + Firefox, proven by the
   VP9 path). The h264-CMAF master points its `EXT-X-MEDIA:TYPE=AUDIO` group
   at this same audio rendition. No new audio code.

3. **Master routing** — `select_master_video`/`master_playlist` gains a third
   shape: a **browser** client whose negotiated video codec is h264 gets a
   **video-only h264-fMP4 rung + the demuxed audio group** (all-fMP4). The
   discriminator is the existing `is_web_client` signal (already computed in
   `items.rs` when building `renditions`) surfaced to the master as a new
   `renditions` token (e.g. `h264cmaf`) OR a query flag, so the server knows
   to emit demuxed-h264 vs muxed-h264. **Native / no-`renditions` clients keep
   the muxed mpegts path byte-for-byte unchanged** — this contains the compat
   blast radius to browsers, which all run hls.js.

4. **Audio-switch behaviour** — jellyfin-web still reloads the master with a
   new `AudioStreamIndex`, but now: the video rung URL's segments are
   **cache hits** (audio-independent key), and only the audio group for the
   new index is cold — served by the cheap continuous audio session. No video
   re-transcode; no restart storm; the SyncPlay group's brief buffering
   freeze lifts as soon as the audio rendition warms.

### Data flow (audio switch, after)

```
jellyfin-web: DELETE ActiveEncodings; GET master.m3u8?...&AudioStreamIndex=2
  → master: video-only h264-fMP4 rung + audio group (URI=/…/audio.m3u8?…AudioStreamIndex=2)
  → video seg fetches  → CACHE HIT (h264 fMP4, audio-independent key)   [fast]
  → audio seg fetches  → ensure_audio_hls_covering(a=2) cold-spins only audio [cheap]
  → member posts /syncplay/ready quickly → group freeze lifts
```

## Client compatibility

- **hls.js (Chrome/Firefox/all browsers)**: h264-in-fMP4 (CMAF) + fMP4 Opus
  audio group is standard and identical in structure to the VP9 rung that
  already ships. This is the only path changed.
- **Native clients (Android TV, etc.)**: unchanged — they receive no
  `renditions` (or a single muxed token) and keep the muxed mpegts single-rung
  path. No native-player risk.
- **Safari / native HLS**: unaffected (still muxed unless it presents as a web
  hls.js client). The demuxed rung is browser-gated.

## Error handling

- Unknown/absent `renditions` → muxed h264 (current default) — no behaviour
  change for anything that doesn't explicitly negotiate the new rung.
- A demuxed h264 segment/playlist request for an audio-only item is N/A (audio
  items keep their own audio-only master path).
- Cache-key correctness: the h264-CMAF video segment MUST set `audio: None`
  and `audio_source_stream_index: None` so its key never varies by audio track
  (verified by a cache-key test), else the whole benefit is lost.

## Testing

- **Unit (hls.rs)**: a browser h264 master emits exactly one video-only
  h264-fMP4 rung + one `EXT-X-MEDIA:TYPE=AUDIO` group and NO muxed mpegts
  rung; a native (no-`renditions`) master is byte-identical to today's muxed
  output; the h264-CMAF segment opts carry `video: Some(H264)`,
  `container: Fmp4`, `audio: None`.
- **Cache-key**: two requests for the same h264-CMAF video segment with
  DIFFERENT `AudioStreamIndex` resolve to the SAME segment cache key (the
  audio-independence invariant).
- **client_compat / hls tests**: extend `jellyfin_hls_multivariant.rs` +
  `client_compat.rs` to drive the demuxed h264 master end-to-end (playlist
  parses, video segment + audio segment both fetch, an audio-index change
  re-fetches the audio rendition but not the video).
- **Regression**: existing VP9 (`vp9_fmp4_hls.rs`) and muxed-h264 tests stay
  green — the muxed path is untouched.

## Non-goals (YAGNI)

- hls.js seamless in-player audio switching (jellyfin-web reloads the master;
  cache-hit video makes the reload cheap, which is sufficient).
- An AAC demuxed audio group (the existing Opus-fMP4 rendition already plays
  in every target browser; no second audio codec needed).
- Changing native / muxed behaviour.
- A SyncPlay-engine change — the group thrash is a downstream symptom of the
  cold re-transcode; removing the re-transcode removes the thrash.
