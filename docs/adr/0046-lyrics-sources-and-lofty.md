# ADR-0046: Song lyrics come from one service, local sources first, tags read with lofty

- **Status:** Accepted
- **Date:** 2026-11-04T00:30:00Z
- **Deciders:** Alison Jenkins

## Context

The native music player (013 US7) shows a song's lyrics, synced when it can.
Before this, the only source was an `.lrc` sidecar read inside the Jellyfin
`GET /Audio/{id}/Lyrics` handler. Most tagged music keeps its words in the
file: ID3v2 `USLT` (plain) or `SYLT` (synced) frames, Vorbis `LYRICS`, MP4
`©lyr`. ffprobe exposes the text tags but not `SYLT`, and the scanner does not
store lyrics. Online databases exist; LRCLIB needs no key, but a lookup sends
the song's title and artist to a third party.

## Decision

We will serve lyrics from one domain service, `services::lyrics`, called by
both `Library.lyrics` (native, schema v8) and the Jellyfin route (V172). It
tries, in order, an `.lrc` sidecar, the file's own tags, a `.txt` sidecar,
and an online provider; the first with words wins (V300). Local sources are
read per request, not stored, so an edited sidecar shows on the next play.

Tags are read with the `lofty` crate, imported only in
`services/lyrics/embedded.rs`; a millisecond `SYLT` frame wins over text tags,
whose text is parsed as LRC. The online provider sits behind the
`OnlineLyrics` trait; the one adapter is LRCLIB, off unless `[lyrics].online`
(`PHAROS_LYRICS_ONLINE`) is true. Its answers are remembered per item; a
failure skips the provider for 60 s.

## Consequences

Tagged files show lyrics with no rescan or schema change. Each lyrics request
reads the song's tags (a header parse, audio properties skipped). `lofty` is a
new pure-Rust dependency, confined to one module so it can be replaced. An
operator who turns on the online provider shares listening data with LRCLIB.

## Alternatives considered

- **ffprobe tags**: no `SYLT`, and a process or libav open per request.
- **Store lyrics at scan time**: every library would need a re-probe
  (`PROBE_SCHEMA_VERSION` bump, ~14k files on the live library) and sidecar
  edits would wait for the next scan.
- **A hand-written ID3v2 parser**: smaller, but would cover MP3 only.

## References

- `crates/pharos-server/src/services/lyrics/`, B360, V300.
- LRCLIB API: https://lrclib.net/docs
