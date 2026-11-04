# ADR-0056: The UI lowers playback quality by renegotiating with a bitrate cap

- **Status:** Accepted
- **Date:** 2026-11-04T00:00:00Z
- **Deciders:** Alison Jenkins

## Context

A viewer on a 1-3 Mbit/s link stalls constantly at source quality: the server
remuxes the source at 1-4.7 Mbit/s and the link cannot carry it. The only
remedy was the manual "Maximum quality" select (`Capabilities.max_bitrate`, a
source above it becomes a capped transcode), which the viewer has to find and
set per device. "Auto" should pick and adapt the quality from the connection.

What the master playlist offers hls.js:

- A browser (`Mozilla` user agent) h264 client gets the demuxed all-fMP4
  master: **one** `EXT-X-STREAM-INF` (`/videos/{id}/h264cmaf/main.m3u8`) plus
  the shared audio group. The multi-rung resolution ladder (`Variant::ladder_for`)
  is only emitted to native, non-browser h264 clients, as muxed mpegts. VP9 is
  also one rung.
- hls.js is created with `startPosition` only. Its ABR has no second level to
  switch to, so it cannot help however it is configured.
- hls.js reports each fragment download (`hlsFragLoaded` carries `stats.loaded`,
  `stats.loading.first/end` and `frag.duration`), so throughput and the stream's
  own bitrate are readable from our glue.

Giving browsers a ladder would mean new demuxed-fMP4 rungs, each a separate
transcode and cache keyspace on the GPU, with the single-viewer-one-transcode
and mixed-audio-model constraints the master already documents (`MasterVideo`).
That is a server project of its own.

## Decision

The client renegotiates with a lower cap; nothing changes server-side.

- **Rungs.** The manual picker's values, 8, 4, 2 and 1 Mbps. Auto sends the
  lower of the viewer's manual maximum and its own cap as `max_bitrate`; a
  manual value is a hard ceiling Auto never steps above. "Auto" with no manual
  value remains the default.
- **Measurement.** The hls.js engine reports each main-track fragment
  (bytes, first-byte-to-last transfer time, media duration). A throughput
  average (30 % weight to the newest download, believed after 3) and the
  stream's bitrate come from those. Stalls are `waiting`/`stalled` events.
- **Step down** (`quality::decide`), outside the 15 s after a player starts or
  changes: two stalls in 30 s (`reason=stalls`), or throughput under 1.3x the
  stream's bitrate (`reason=low_estimate`). The new cap is the highest rung
  below the stream that the link carries at 1.3x, else the lowest rung.
  Two events within 2 s are one stall; a stall reported within 3 s before a
  seek completes is the seek's own and is dropped.
- **Step up**, one rung at a time: only after the throughput has stayed at 2x
  the next cap for 60 s with no stall in between. Reaching the source's own
  bitrate lifts the cap. A step up can land back in a step down; the 15 s
  settling time and the 60 s hold are the hysteresis.
- **Reload.** A decision is a local renegotiation at the player's last position
  through the path an audio-track change already uses (new play session,
  remounted player). It sends no group seek or pause; the group session sees a
  player detach and attach, and the only group traffic is the buffering/ready
  pair any real stall or reload already produces.
- **Memory.** The last cap is stored per device (`pharos.player.auto_rung`)
  and is where the next video starts.
- **Scope.** Only video played by hls.js is judged. Direct play and Safari's
  native HLS give no download timings.
- **Observable.** The browser logs one line per decision:
  `pharos: quality auto from=<bps> to=<bps> reason=<stalls|low_estimate|headroom> estimate_bps=<n>`.
  The server side is the existing negotiate log and
  `pharos_playback_decision_total{direct_play_block="bitrate"}`, which now also
  counts automatic caps; the client's negotiate line gains `auto=<bps>`.

## Consequences

- A slow-link viewer converges on a playable rung without finding a setting,
  and returns to higher quality when the link recovers.
- Each step costs one reload (a second or two of buffering) and, for a source
  above the cap, a server transcode at that rung, per viewer, only while
  capped. A viewer starts at most one transcode: there is one stream at a time.
- Quality is chosen per reload, not per segment as ABR would. Convergence takes
  15-30 s per step down and 60 s per step up.
- Throughput is measured from fragments as hls.js fetches them; a server that
  is slow to start a transcode is not read as a slow link (first byte to last
  only), but a browser-cached fragment can overstate the link.
- The cap is sent in every video negotiation, but only judged (changed) on the
  hls.js engine. Safari native HLS and direct play keep whatever cap was
  stored, so a slow-link viewer there is not stepped down; a ladder would be
  needed there.

## Alternatives considered

- **hls.js ABR across a ladder.** The better player behaviour (per-segment
  switching, no reload), but the browser master has one rung and adding
  demuxed-fMP4 rungs needs new server transcode variants. Revisit if a
  server ladder is built; this ADR's decision rule would then only choose the
  starting level.
- **A fixed lower default.** Penalises viewers on fast links.

## References

- `crates/pharos-ui/src/native/quality.rs` (decision rule, thresholds)
- ADR-0035 (vendored hls.js), ADR-0055 (group tracks reload through the same path)
