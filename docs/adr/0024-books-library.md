# ADR-0024: Books as a first-class library kind (client-side readers, no ffmpeg)

- **Status:** Accepted
- **Date:** 2026-10-05T00:00:00Z (specified 2026-07-29, shipped 2026-07-30; backfilled 2026-10-05)
- **Deciders:** Alison

_Backfill: written after the fact from `specs/004-books/`, the invariants and
the code. Rationale the spec does not record is marked "not recorded"._

## Context

pharos models every item as video or audio, and the scanner only keeps a file
if ffprobe can read it (V6: a probe miss writes nothing). An epub is not a
media container, so ebooks and comics never became items at all. Users with a
book shelf could not browse or read it through a Jellyfin client.

Jellyfin does this with the Bookshelf plugin. pharos has no plugin host and
does not want one (spec.md Goal). The deciding finding was on the client side:
the deployed jellyfin-web already ships `bookPlayer` (epub.js), `pdfPlayer`
(pdf.js) and `comicsPlayer` (libarchive.js). The server renders nothing. It
has to pass three gates those players apply and then hand over bytes
(plan.md Summary, research.md R1, R2).

## Decision

Books are their own kind: **`MediaKind::Book`** and **`LibraryKind::Books`**
(collection type `books`), not a variant of the video or audio path.

- **Why a kind, not a media variant.** Book facts (author, series, ISBN, page
  count) are not probe facts. spec.md Key Entities keeps `BookMeta` apart from
  `MediaProbe` so "author" never sits beside "pixel format", and
  `BookFormat::Unreadable` makes "indexed but no client reader" a state the
  types can express. Why a boolean on an existing kind was not chosen: rationale
  not recorded beyond that (R5 states the variant "is still the right
  modelling choice").
- **Scanning.** The scanner classifies by extension before the prober is
  reached, so a library scan runs ffmpeg zero times for a book (SC-002).
  Pure-Rust readers under `crates/pharos-scanner/src/book/`: `epub.rs`
  (container.xml, OPF, cover), `comic.rs` (cbz/cb7 entry list, first image as
  cover), `pdf.rs` (info dictionary, page-one embedded JPEG). A malformed file
  still imports with empty metadata (V6, V119).
- **Serving.** Item DTOs carry `Type` and `MediaType` of `Book`, an item
  `Path`, and `GET /Items/{id}/Download` with `Range` and `api_key` auth
  (`download.rs`). Those are the three gates the readers check (R2). A book
  offers nothing to play: `MediaSources` and `MediaStreams` empty, `RunTimeTicks`
  0, and `PlaybackInfo` yields no source (FR-008, FR-010). Empty, not absent,
  because jellyfin-web iterates those arrays unguarded (R9). Progress reuses
  `UserData.PlaybackPositionTicks` (R8).
- **Storage.** Seven nullable `book_*` columns on `media_items`, migration 0053
  on both backends (the plan names 0052; the shipped file is
  `0053_book_metadata.sql`).
- **No rasteriser.** PDF covers are page one's embedded JPEG only; a text-first
  PDF gets no cover and advertises none. A rasteriser is a C library and breaks
  the single-binary deploy (R11).
- **The kind-decision audit.** `kind-decision-audit.md` (2026-07-29, before the
  variant existed) found that the compiler does not enumerate the sites that
  decide for a new variant: 34 `matches!`/`==` sites plus the wildcard arms, 8
  needing a change. Two wildcard sites would have labelled a book `Video`
  (`dto.rs` `MediaType`, and `media_type_of` in `items.rs`), failing every
  reader with no error. The audit ran first, verdicts were recorded, and this
  became V118.

Out of scope, per the spec: audiobooks, writing metadata back, an online book
metadata provider, a Dioxus reader.

## Consequences

- Unmodified jellyfin-web opens an epub, cbz and pdf. Pagination, table of
  contents and unpacking stay on the client.
- B170 broke the first cut: `Path` was `Fields`-gated, but the details fetch
  sends no `Fields`, so every book was silently unopenable. `Path` is now
  unconditional for books (V117).
- A hostile PDF could abort the scan: `lopdf` 0.34 overflowed the stack
  (B173), and parsers run behind a panic guard (V119). `.cbr` is readable and
  downloadable but cover-less (R7); `.mobi`/`.azw3` are indexed but not
  readable. ADR-0025 later made the second group readable by conversion.
- Client limits stay client limits: `bookPlayer` compares `.epub`
  case-sensitively, and pharos does not misreport a path to dodge it.
- Every new `MediaKind` variant now owes the same audit (V118). The kind is
  extra surface on both migration trees (ADR-0003).

## References

- `specs/004-books/spec.md` (Goal, FR-001..FR-010, SC-002..SC-005, Assumptions)
- `specs/004-books/plan.md`, `research.md` (R1, R2, R4, R5, R9, R10, R11),
  `kind-decision-audit.md`, `contracts/books-http.md`
- `invariants.md` V117, V118, V119, V122; `bugs.md` B170, B171, B173, B214
- `crates/pharos-core/src/lib.rs` (`MediaKind::Book`, `BookFormat`, `BookMeta`),
  `crates/pharos-scanner/src/book/`,
  `crates/pharos-server/src/api/jellyfin/download.rs`,
  `crates/pharos-store-sqlx/migrations/*/0053_book_metadata.sql`
- ADR-0001 (wire compatibility), ADR-0003 (two migration trees), ADR-0025
  (Kindle conversion)
