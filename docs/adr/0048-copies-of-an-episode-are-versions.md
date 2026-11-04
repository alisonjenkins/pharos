# ADR-0048: Copies of one episode are versions of one item

- **Status:** Accepted
- **Date:** 2026-11-04T01:00:00Z
- **Deciders:** Alison Jenkins

## Context

A show folder can hold the same episode more than once: a `Season 01/` and a
`Season 1/` side by side, or a second rip (`.avi` beside `.mkv`, `HDTV`
beside `WEBDL`). On the live library 590 episodes were held by 1,187 rows,
and each copy was listed as its own episode. Alison asked for one item per
episode, with a choice of version to play and each version's facts and (for
admins) its path. Editions (`Title - Director's Cut`) were already folded
into their primary's `alternate_sources` at scan time (P41).

## Decision

We will group copies at scan time, in `group_editions`: files under one show
folder that parse to the same season and episode span become one item. The
best-ranked copy is the primary (container rank, then path) and each other
copy an `AlternateMediaSource`. A copy's old row is swept like any file that
stopped being an item, after `MediaStore::merge_user_data` moves its viewers'
state onto the primary, keeping the furthest state of the two. Films are not
grouped by this rule.

## Consequences

- One row per episode: watched state, Next Up, resume and search see one
  item, and Jellyfin clients get the copies as PlaybackInfo MediaSources.
- The first scan after deploy deletes the copies' rows. Played, favourite,
  play count and resume carry over; anything else keyed on the deleted id
  (playlist entries, a stored audio or subtitle choice) does not.
- A copy that arrives through the watcher stays its own item until the next
  full scan, as editions already did.
- Undoing this means a rescan with the rule removed; the folded rows come
  back with fresh ids and without the state that moved.

## Alternatives considered

- Group at read time (collapse rows in listings): keeps every row, but each
  listing, count, page and the letter index would need the same collapse,
  and watched state would stay split across the copies.
- Leave copies to the admin Duplicates tab: finds them but still lists each
  episode twice to viewers.

## References

B395, V335; `crates/pharos-scanner/src/fs.rs` `group_episode_copies`.
