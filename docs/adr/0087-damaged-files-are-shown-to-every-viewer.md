# ADR-0087: Damaged files are shown to every viewer, from the integrity memo

- **Status:** Accepted
- **Date:** 2026-11-09T19:30:00Z
- **Deciders:** Alison Jenkins

## Context

The integrity sweep (B899) reads every movie, episode, song and book end to
end and records each verdict in the integrity memo, a JSON file beside the
transcode cache. Only admins could see the result, on Admin → Damaged
files. A viewer found a damaged episode by watching it break.

On 2026-11-09 Alison asked for damaged files to be shown in the UI: a badge,
a warning when playback starts that says what is damaged, and the damaged
time ranges on the seek bar. The memo kept only the first fault's position,
so B936 added damaged spans to each verdict.

## Decision

We will show damage to every viewer through the native API, read straight
from the in-process integrity memo:

- `ItemSummary.damaged` badges the item's primary file in every listing.
  The damaged ids are read from the memo once per call, not per item.
- `ItemDetail.damage` gives the verdict, the reader's own words, the spans
  and when it was checked, for the file the detail describes. Only primary
  files are swept, so an alternate version's detail carries none, and
  `Version.damaged` marks the damaged primary in the picker.
- The player shows a short note that fades by itself after ~8 s and marks
  the spans on the seek bar. The title page says where the file breaks.

A verdict stands until the sweep reads the file again, as on the admin
list. A replaced file is re-read on the sweep's next pass, because its
mtime and size change.

## Consequences

- No migration and no new store method: damage is a measurement of a file,
  not domain state (the memo's own reasoning), and losing the memo costs one
  slow sweep.
- A follower replica serves the memo it loaded at startup, because only the
  leader sweeps. On a multi-replica deploy a new verdict reaches viewers on a
  follower only after it restarts. The live deploy has one replica. If that
  changes, the verdicts move to a table the leader writes.
- A replaced file stays badged until its re-read, up to one sweep interval
  (an hour) plus the read itself.
- Every listing walks the memo's entries once (~14k on the live library),
  which costs microseconds next to the store query.

## Alternatives considered

- **A table of verdicts, written by the leader:** correct on every replica
  at once, but it adds a migration and six `MediaStore` implementations for
  state whose loss costs only a re-read. Deferred until there is more than
  one replica.
- **A separate `Library.damaged` method listing damaged ids:** one more
  call, and a new method to cover in the redaction list. The summary field
  costs nothing on clients that already list items.
- **Admins only:** what existed. A viewer still met the damage mid-film
  with no warning.

## References

- B936, B937, B938 and V897, V898, V899 in `specs/001-pharos-baseline/`.
- B899 (the integrity sweep and the admin tab).
