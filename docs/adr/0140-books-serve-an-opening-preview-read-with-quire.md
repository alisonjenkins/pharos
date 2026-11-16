# ADR-0140: Books serve an opening preview, read with quire

- **Status:** Accepted
- **Date:** 2026-11-16T12:10:00Z
- **Deciders:** the project owner (asked to read a book's opening before downloading it), Claude

## Context

Many books have no stored description, so a reader cannot judge one without
downloading the whole file. The Kobo app has a description pager but nothing
to put in it. `pharos-server` did not depend on `quire`, the in-workspace
EPUB reader that `pharos-ui` uses; EPUB text had only been read by
`pharos-scanner` for metadata.

## Decision

We will add `GET /Items/{id}/Opening`, returning the first ~6000 characters of
prose as `text/plain`, behind the same item-access check as `/Download` and
reading the file `/Download` would send (a converted Kindle book reads its
EPUB). `pharos-server` takes a path dependency on `quire`. EPUBs are opened
through an `Archive` over the file on disk, so only the zip directory and the
spine items needed are read; `.txt`, `.md` and `.html` read their first
256 KiB; other formats answer 404. Verdicts count on
`pharos_book_opening_total{outcome}` (V1448).

## Consequences

- A reader can see a book's start without the download; the Kobo detail page
  uses it for books with and without a description.
- The server now links `quire` (already built for `pharos-ui`, no new
  external crate) and owns a small zip `Archive` adapter.
- The HTML stripper is deliberately not a parser; odd markup may leave stray
  text in a `.html` book's preview.
- Skipping spine items under 80 characters can drop a very short real
  chapter; the preview then starts at the next one.

## Alternatives considered

- Read the whole file into `quire::ZipArchive`: simpler, but holds a large
  EPUB in memory per request.
- Use the scanner's EPUB code: it extracts metadata and covers, not
  sanitised section text.
- Do it in the Kobo app from the downloaded file: defeats the point, the
  reader wants it before downloading.

## References

- `crates/pharos-server/src/api/jellyfin/opening.rs`
- `specs/001-pharos-baseline/invariants.md` V1448
