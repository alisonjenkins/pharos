# ADR-0075: Copies of one episode are versions of one item

- **Status:** Accepted
- **Date:** 2026-11-08T12:00:00Z
- **Deciders:** Alison Jenkins

## Context

A show folder can hold the same episode more than once: a `Season 01/` and a
`Season 1/` side by side, or a second rip (`.avi` beside `.mkv`, `HDTV`
beside `WEBDL`). On the live library 590 episodes were held by 1,187 rows,
and each copy was listed as its own episode with its own watched state.
Alison asked for one item per episode, with a choice of version to play and
each version's facts and (for admins) its path. Editions (`Title - Director's
Cut`, `Title - 3D.HSBS`) were already folded into their primary's
`alternate_sources` at scan time (P41). This decision was first written as
ADR 0048 in #643, which never merged and whose number has since been taken.

## Decision

We will group copies at scan time, at the end of `group_editions`: files
under one show folder that parse to the same season and episode span become
one item. The best-ranked copy is the primary (a 2D file before a 3D one,
then container rank, then path) and each other copy an
`AlternateMediaSource` named by what its file name adds. A copy's old row is
swept like any file that stopped being an item, after
`MediaStore::merge_user_data` moves its viewers' played, play count,
favourite and resume state onto the primary, keeping the furthest state of
the two. When the primary was not written in that pass (its probe failed),
the copy keeps its own row. Films are not grouped by this rule.

## Consequences

- One row per episode: watched state, Next Up, resume and search see one
  item, Jellyfin clients get the copies as PlaybackInfo MediaSources, and
  pharos-ui offers them in its Version picker (schema 26).
- The first scan after deploy deletes the copies' rows. Played, favourite,
  play count and resume carry over; anything else keyed on the deleted id
  (playlist entries, a stored audio or subtitle choice, whose stream indexes
  named the other file's streams) does not.
- A copy that arrives through the watcher stays its own item until the next
  full scan, as editions already did.
- A version's image subtitles burn and it lists no embedded fonts, because
  those routes read the primary file only.
- Undoing this means a rescan with the rule removed; the folded rows come
  back with fresh ids and without the state that moved.

## Alternatives considered

- Group at read time (collapse rows in listings): keeps every row, but each
  listing, count, page and the letter index would need the same collapse,
  and watched state would stay split across the copies.
- Delete the duplicate files: not pharos's to decide, and the copies differ
  (quality, audio languages) in ways a viewer may want.

## References

- `crates/pharos-scanner/src/fs.rs` (`group_episode_copies`, `settle_copies`)
- B771, V721 in `specs/001-pharos-baseline/`
