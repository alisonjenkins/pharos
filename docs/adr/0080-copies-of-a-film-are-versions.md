# ADR-0080: Copies of one film are versions of one item

- **Status:** Accepted; supersedes ADR-0079 in part (its "Films are not grouped")
- **Date:** 2026-11-09T08:00:00Z
- **Deciders:** Alison Jenkins

## Context

ADR-0079 made copies of one episode one item and left films out. A film can
be held twice as well: `Film BB (1995)/Film BB (1995) Bluray-1080p.mkv` beside
`Film BB (1995) [720p]/Film BB (1995) HDTV-720p.mkv`, each listed as its own film
with its own watched state. Alison asked that the pattern carry to all media
pharos manages, with the best quality copy played by default.

## Decision

A video file (`mkv`, `mp4`, `mov`, `avi`, `webm`, `m4v`) that is not an
episode and whose own file name states a year is keyed by every word of that
name, lower-cased, less the words that say how the copy was made: the
resolution, source, codec, audio format and channel layout, and the 3D
packing (`FILM_RELEASE_WORDS`). Files with one key are copies of one film.
`Film BB (1995) Bluray-1080p` and `Film BB.1995.720p.HDTV` are both `film bb 1995`;
`Film BB (1995)-trailer`, `Film BB (1995) Director's Cut`, `Film BB (1995) - part2`
and `Show (2006) - 01 - Title` each keep the word that makes them another
title. A 3D rip of a cut is a copy of that cut. The copies rank as an
episode's do under ADR-0079: 2D before 3D, then the taller picture, then the
bigger file, then container, then path; the others are `alternate_sources`,
and a copy's row folds its user data into the item as for episodes
(`FoldResume::Carry`, one timeline).

A name without a year never groups, since a title alone would merge remakes.
The film's folder name is not used, because that folder also holds the
trailer and the sample.

## Consequences

- One row per film: watched state, resume and search see one item, and the
  Version picker lists the copies.
- The first scan after deploy deletes the copies' rows, with the same
  carry-over and losses as ADR-0079.
- A copy named without a year (`Film BB.mkv`) stays its own film.
- Two different films with one title and one year (rare) would merge; the
  primary is still the better file and the other stays playable as a
  version.
- A release group or tag outside the list (`-SPARKS`, `[PROPER2]`) keeps
  two copies apart: a missed group costs a duplicate listing, a false one a
  deleted row, so the list errs towards missing.
- `pharos_scan_copy_primaries{decided_by}` counts film primaries with the
  episode ones.

## Alternatives considered

- Group by matched metadata id (TMDB): only known after the metadata pass,
  which runs after grouping, and unmatched films would never group.
- Group by folder name: catches trailers and samples, which live in the
  film's folder.
- Key by the parsed title and year alone (the first cut): merged
  trailers, samples, cuts named without a dash, parts and shows named by
  episode number.

## References

- `crates/pharos-scanner/src/fs.rs` (`film_copy_key`, `group_copies`)
- B897, V846 in `specs/001-pharos-baseline/`
