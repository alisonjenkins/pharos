# ADR-0081: Copies of one song are versions of one item

- **Status:** Accepted
- **Date:** 2026-11-09T08:30:00Z
- **Deciders:** Alison Jenkins

## Context

ADR-0079 grouped copies of an episode and ADR-0080 copies of a film. Music
is held twice too: an album ripped again (`Album/` beside `Album [FLAC]/`),
or a song beside its re-encode (`01 Song.flac` and `01 Song.mp3`). Each copy
was its own song with its own play count. Alison asked that the pattern
carry to all media pharos manages, the best quality copy playing by default.

## Decision

An audio file (`flac`, `mp3`, `opus`, `m4a`, `ogg`, `wav`) that is not an
episode is a copy of every other with the same file stem under the same
three folders above it (album, artist and the folder above, or disc, album
and artist). Each name is folded to lower-case letters and digits after
dropping each bracketed tag (`[..]`, `(..)` or `{..}`) whose every word is a
format, bit depth, rate or year (`[FLAC]`, `(2001)`, `(320 kbps)`,
`[24-96]`). Any other bracket stays part of the name, so `[Live]`, `[CD2]`,
`(Deluxe)` and `(Remix v2)` keep their songs apart. A song is never ranked
as a 3D rip. The bigger file is the
primary, as for episodes and films (copies of one song run the same length,
so size stands for bitrate, and a lossless file is almost always bigger
than a lossy one; a `.wav` beside its `.flac` wins, which plays the same).
The others are `alternate_sources`, and a copy's row folds its play count,
favourite and resume into the item (`FoldResume::Carry`).

Songs are keyed by path, not tags. Tags are read when a file is probed,
which runs after grouping, so a tag key would group a new copy only on the
scan after it arrived and needs a store read per song on every scan.

## Consequences

- One row per song: play counts, favourites and the album's track list see
  one item. Jellyfin clients get the copies as MediaSources. pharos-ui's
  Version picker is on the film and episode page only, so the UI plays the
  primary.
- The first scan after deploy deletes the copies' rows, with the same
  carry-over and losses as ADR-0079 (playlist entries naming a deleted id
  are lost).
- A re-rip whose folder or file name differs by more than release tags
  (`Album (Remastered)`, `01 Song.flac` and `01 - Song.flac` are the same,
  `1 Song` and `01 Song` are not) stays apart.
- A song's copies never look up a stored picture height.

## Alternatives considered

- Key by tags (album artist, album, disc, track, title): sturdier against
  renamed folders, but only known after the probe, as above.
- Rank lossless first, then bitrate: size orders them the same way for one
  song without reading the store.
- Drop every `[..]` and `{..}` and keep two folders (the first cut): merged
  live takes, instrumentals and discs, and two artists' compilations with a
  disc folder.

## References

- `crates/pharos-scanner/src/fs.rs` (`song_copy_key`, `fold_release_name`)
- B898, V847 in `specs/001-pharos-baseline/`
