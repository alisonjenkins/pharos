# ADR-0006: Subtitles delivered out-of-band, never muxed

- **Status:** Accepted
- **Date:** 2026-07-09T00:00:00Z
- **Deciders:** Alison

## Context

Media carries subtitles as embedded streams (SubRip, ASS/SSA, PGS image subs) or
external sidecars. jellyfin-web does not consume subtitles as a muxed track in
the transcoded container — it fetches **text** subtitles as `Stream.js`, a JSON
array of `TrackEvents` (cues), and renders them itself; ASS/SSA rendering
(SubtitlesOctopus) additionally needs the embedded **fonts**, delivered as
`MediaAttachments`. Image subs (PGS) must be burned in.

## Decision

Subtitles are delivered **out-of-band**, never muxed into the transcoded audio/
video stream:

- **Text subs** → served as WebVTT and as jellyfin-web's `Stream.js` JSON
  (`subtitles.rs` `deliver_js`); routes are public (jellyfin-web fetches them
  without an auth header).
- **Embedded fonts** → advertised as `MediaAttachments` and served from
  `/Videos/{id}/{msid}/Attachments/{idx}`, extracted from the source's
  attachment streams so SubtitlesOctopus can render ASS/SSA.
- **Image subs (PGS)** → burned into the video via the ffmpeg `subtitles` filter
  (which reads the source file directly, not a mapped stream).
- The transcode argv always passes **`-sn`** so ffmpeg's default stream
  selection never grabs a source subtitle and writes it as a spurious `mov_text`
  track into the output container.

## Consequences

- The transcoded fMP4/WebM stream is video+audio only — no third `text` track to
  confuse hls.js's timeline (a real bug found 2026-07: the deployed VP9 init
  declared three tracks; `-sn` removed the stray one).
- Burn-in for image subs is unaffected by `-sn` because the `subtitles` filter
  demuxes the file independently of output stream mapping.
- pharos must implement jellyfin-web's exact `Stream.js` cue shape and font
  delivery, rather than relying on the container to carry subs — more endpoints,
  but it matches what the client actually requests.
- Backfilling `MediaAttachments` onto items scanned before the feature existed
  requires a forced re-probe (ADR-0008): `POST /Library/Refresh?force=true`.

## References

- `crates/pharos-server/src/api/jellyfin/subtitles.rs`
- `crates/pharos-transcode/src/lib.rs` (`-sn`), `libav/attachment.rs`
- memory `reference_jellyfin_subtitle_stream_js`, `project_vp9_hls_avsync_findings`
