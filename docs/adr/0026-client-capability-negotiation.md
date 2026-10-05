# ADR-0026: Playback decided by negotiating client against server capabilities

- **Status:** Accepted
- **Date:** 2026-10-05T00:00:00Z (documented; code landed 2026-07-21, backfill)
- **Deciders:** Alison

## Context

Every Jellyfin client declares what it can play in a `DeviceProfile` posted to
`/Items/{id}/PlaybackInfo` (ADR-0001). Pharos has to turn that, plus the source
file, into one verdict: direct play, remux or transcode.

Two things made the early answer wrong:

1. **The target codec ignored the server.** `negotiate` picked the transcode
   codec from the client profile alone and in practice hardcoded h264, blind to
   what the box could encode (`crates/pharos-transcode/src/capability.rs`,
   module doc).
2. **A user-agent sniff covered the gap.** A `force_webm` path matched
   "Firefox" plus "Linux" in the User-Agent and forced VP9 (ADR-0005, B43,
   B59). It matched too widely before it was narrowed (B43), and it split
   SyncPlay groups across separate encodes (B59, V33).

Rationale for the original UA sniff is recorded in ADR-0005 and B43 only as
"desktop-Linux Firefox can lack an H.264 decoder while `canPlayType` still
says probably". No deeper design note exists.

## Decision

The verdict is the **intersection of what the client's profile declares and
what the server can deliver**, and every verdict carries its reason.

- `negotiate` / `negotiate_explained` in
  `crates/pharos-jellyfin-api/src/device_profile.rs` walk the profile against the
  probed source: container, video codec, audio codec, bitrate ceiling, then
  `CodecProfiles` conditions (level, bit depth, channels). The result is a
  `Decision` (`DirectPlay`, `AudioRemux`, `VideoRemux`, `Transcode`).
- The transcode video codec is the best of *(client-decodable ∩
  server-encodable)*: hardware first, then cheaper, then an h264 tiebreak, then
  client order (`rank_transcode_video_codecs`). Server encode capability is
  built once at boot from trial-confirmed devices and `ffmpeg -encoders`
  (`capability.rs`). A client offering nothing the server can encode gets a safe
  fallback target.
- A remux or transcode is routed to one unified HLS master whose rendition set is
  that ranked intersection. A web client gets several codecs and picks with its
  own `isTypeSupported`. A native client gets the single negotiated codec.
- Every decision returns a `DirectPlayBlock` naming why direct play was ruled
  out, with the offending value (`no_profile`, `container`, `video_codec`,
  `audio_codec`, `bitrate`, `codec_condition`, `remote_source`). The labels are
  stable strings for metrics and logs.
- The Firefox UA force was deleted outright (commit `70a3aedf`, 2026-07-21),
  together with the `[server].linux_firefox_h264` flag. Its deployed value was
  already off, so behaviour did not change.

## Consequences

- A client that declares a codec and is wrong about it is not corrected by the
  server. Server-side corrections are narrow and encoded as rules, for example
  never advertise DirectStream for a Matroska a browser cannot demux
  (B77/V37, B80, V57).
- The User-Agent is not gone. It still separates a browser from a native app
  (`"Mozilla"` substring) for resume-offset handling and DirectPlay delivery
  (`items.rs`, `stream.rs`). It no longer picks a codec.
- Adding an encoder (for example hardware VP9 or AV1) changes verdicts without
  touching client handling, but only after a boot-time trial encode succeeds.
- Amends ADR-0005: delivery to Firefox-class clients is no longer chosen by
  User-Agent. The VP9-in-fMP4 segment path remains one rung of the master.
- The h264 tiebreak is a deliberate bias toward one shared cached encode
  (V33). Why h264 and not another codec is explained only by that cache-key
  argument.
- Observability: `pharos_playback_decision_total{decision,direct_play_block,downgrade}`
  answers "why is this transcoding" by query.

## References

- ADR-0001 (client contract), ADR-0005 (per-segment HLS; UA force amended here)
- `crates/pharos-jellyfin-api/src/device_profile.rs` (`negotiate_explained`,
  `DirectPlayBlock`, `rank_transcode_video_codecs`),
  `crates/pharos-transcode/src/capability.rs`
- `crates/pharos-server/src/api/jellyfin/items.rs` (PlaybackInfo, decision metric)
- Commits `37224f3f` (capability-aware target), `70a3aedf` (delete UA hack)
- `specs/001-pharos-baseline/invariants.md` V33, V37, V56, V57;
  `bugs.md` B43, B59, B77, B80
