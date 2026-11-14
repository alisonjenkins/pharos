# ADR-0126: Video courses in a Books library are shows

- **Status:** Accepted
- **Date:** 2026-11-14T07:31:17Z
- **Deciders:** Alison Jenkins

## Context

- A Books library can hold video courses beside its books: a folder per
  course, a folder per module, a video per lesson. Invented examples:
  `Intro to Widgets/02 - Assembly/03_-_0201_Fitting_the_Base.mp4`,
  `Intro to Widgets/02. Assembly/02_05-Fitting.mp4`,
  `Intro to Widgets/03/007 Fitting.mp4`, or flat
  `Intro to Widgets/01 - Getting Started.mp4`.
- The scanner did not know library kinds. Each lesson was probed as a film,
  so the Books grid listed every lesson as a loose video next to the books.
- The online matcher then searched TMDB for each lesson's file name and
  stored whatever film it found: wrong titles, descriptions, people and
  posters on lessons.
- Files must not move on disk. The fix has to read the layout as it is.
- pharos already has a page for "a folder of numbered videos in numbered
  folders": the show page (`ShowView`), with seasons and episodes, played
  state, Next Up and resume.

## Decision

We will read a video under a Books library root as a course lesson and store
it as an episode of a show.

1. **Representation.** `MediaKind::Episode` with `SeriesInfo`: the series is
   the first folder under the Books root (its folder is the identity key);
   the season is the module folder's leading number, else 1; the episode is
   the last leading number of the file name. `series_year` and `series_key`
   stay `None`, so the series split never splits a course. A video directly
   in the root is a one-lesson course of its own. The title is the file name
   with its numbers and separators taken off. No migration: the row is marked
   by `match_source = 'course'` (with `match_provider = 'local'`), which is a
   free text column.
2. **Where the rule applies.** The scanner learns the Books roots
   (`FsScanner::with_course_roots`) at every place it is built: the CLI scan,
   the watcher and periodic rescan, and the admin refresh. Only video
   containers are lessons; books, audio and notes in a course folder keep
   their own handling.
3. **No online matching.** Course rows and course series are left out of
   both match queries, and the enricher skips a course row if one reaches it.
4. **Old matches are cleared on the next scan.** An unchanged lesson whose
   row is not yet an episode marked `course` is re-probed. Its stored match
   is replaced by the course mark, and its provider artwork rows and cached
   images are removed. Overview, year and the like are written over by the
   re-probe, as for any row, unless an admin locked them.
5. **Listings.** A Books library listed with no kind named lists books only.
   Its courses are listed by asking for series under the library, as a TV
   library's are. A course's seasons are named after its module folders.
6. **UI.** The native Books library gains a Courses tab when it has any
   courses. A course opens on the existing show page.

## Consequences

- Courses get resume, played marks, Next Up and season tabs for free.
- Lessons appear in Next Up like episodes. This is accepted.
- Course series are scoped by library like any show, so they never appear
  in a TV library or its home rail.
- A module numbered `00` is season 0, which the show page treats as
  specials: the page opens on the first module above 0.
- Two modules with no number both read as season 1 and share a tab.
- An audio file in a course stays an audio item. It is not a lesson and is
  not listed under the course.
- Every unchanged lesson costs one row read per scan, to check its mark.
- The Jellyfin episode DTO's `SeasonName` still says "Season N": the module
  name is only on the season items.

## Alternatives considered

- **A new `MediaKind::Lesson`.** Every kind match in the workspace, the wire
  enums, the store and the clients would need a new arm, and the show page
  would have to be taught a second kind of episode. Rejected for its size.
- **A new library kind `Courses`.** The user keeps courses and books in one
  library on disk and did not want to move files or split the root.
- **Matching lessons as films and hiding them.** The wrong metadata would
  still be fetched and stored. Rejected.

## References

- B1423, B1424; V1392, V1393 in `specs/001-pharos-baseline/`.
- `crates/pharos-scanner/src/course.rs`.
