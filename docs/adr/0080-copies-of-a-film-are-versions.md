# ADR-0080: Copies of one film are versions of one item

- **Status:** Accepted; supersedes ADR-0079 in part (its "Films are not grouped")
- **Date:** 2026-11-09T08:00:00Z
- **Deciders:** Alison Jenkins

## Context

ADR-0079 made copies of one episode one item and left films out. A film can
be held twice as well: `Heat (1995)/Heat (1995) Bluray-1080p.mkv` beside
`Heat (1995) [720p]/Heat (1995) HDTV-720p.mkv`, each listed as its own film
with its own watched state. Alison asked that the pattern carry to all media
pharos manages, with the best quality copy played by default.
## Decision

A video file (`mkv`, `mp4`, `mov`, `avi`, `webm`, `m4v`) that is not an
episode and whose own file name states a title and a year is a copy of every
other such file with the same title and year, the title folded to lower-case
letters and digits. The copies rank as an episode's do under ADR-0079: 2D
before 3D, then the taller picture, then the bigger file, then container,
then path; the others are `alternate_sources`, and a copy's row folds its
user data into the item as for episodes (`FoldResume::Carry`, one timeline).

A name without a year never groups, since a title alone would merge remakes.
The film's folder name is not used, because that folder also holds the
trailer and the sample. An edition other than 3D stays its own title.

## Consequences

- One row per film: watched state, resume and search see one item, and the
  Version picker lists the copies.
- The first scan after deploy deletes the copies' rows, with the same
  carry-over and losses as ADR-0079.
- A copy named without a year (`Heat.mkv`) stays its own film.
- Two different films with one title and one year (rare) would merge; the
  primary is still the better file and the other stays playable as a
  version.
- `pharos_scan_copy_primaries{decided_by}` counts film primaries with the
  episode ones.

## Alternatives considered

- Group by matched metadata id (TMDB): only known after the metadata pass,
  which runs after grouping, and unmatched films would never group.
- Group by folder name: catches trailers and samples, which live in the
  film's folder.

## References

- `crates/pharos-scanner/src/fs.rs` (`film_copy_key`, `group_copies`)
- B897, V846 in `specs/001-pharos-baseline/`
