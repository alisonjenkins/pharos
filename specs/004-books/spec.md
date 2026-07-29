# Feature Specification: native book support

**Feature directory**: `specs/004-books/`
**Created**: 2026-07-29
**Status**: planned (Phase 1 design complete)

## Goal

Serve a book library — ebooks and comics — from pharos, so that unmodified
Jellyfin clients browse and READ them. Native to the server, not a plugin:
pharos has no plugin host and does not want one, and everything Jellyfin's
Bookshelf plugin adds to the *server* is, in pharos, core scanner + DTO work.

The decisive finding, established before this spec was written rather than
assumed: **the readers already exist on the client.** The deployed jellyfin-web
bundle ships `bookPlayer` (epub via epub.js), `pdfPlayer` (pdf.js) and
`comicsPlayer` (cbz/cbr/cbt/cb7 via libarchive.js). pharos does not render a
page, paginate, or unpack an archive. It has to satisfy three gates those
players apply, and hand over bytes.

## User Stories

### US1 — read an ebook (P1)

A user opens the Books library, picks an epub, and reads it in the browser with
working pagination and table of contents.

**Independent test**: seed one `.epub`, open it in jellyfin-web, turn a page.

### US2 — read a comic (P1)

A user opens a `.cbz` and pages through it as images.

**Independent test**: seed one `.cbz` of known page count, open, page forward.

### US3 — browse a book library (P1)

Books appear as their own library with covers, titles, authors and series
grouping — not as unplayable rows in a video library.

**Independent test**: `/UserViews` returns a `books` collection; the grid
renders covers.

### US4 — read a PDF (P2)

A `.pdf` opens in the pdf reader.

### US5 — resume where I left off (P3)

Reopening a part-read book returns to the last position.

## Functional Requirements

**FR-001** A book file is imported as an item of kind Book, with no ffmpeg
probe attempted. Books are not media files; probing them would fail and V6
would skip them, so the item would never exist.

**FR-002** `BaseItemDto.Type` is `Book` and `MediaType` is `Book`. Both players
gate on `canPlayMediaType(mediaType) === "book"` (lowercased compare).

**FR-003** `BaseItemDto.Path` is emitted when the client's `Fields` requests it.
All three readers gate on `canPlayItem`, which tests `item.Path` against an
extension. Without `Path` the item is silently unplayable — the failure mode
this project has hit repeatedly (see B159/B107 class: jellyfin-web refuses with
no error path).

**FR-004** `GET /Items/{id}/Download` serves the file bytes, honouring `Range`,
and authenticates via `?api_key=` as well as the `Authorization` header — the
client builds this URL with `api_key` in the query and no header.

**FR-005** A book library is a library of collection type `books`.

**FR-006** Covers come from the file itself: the epub OPF `<meta name="cover">`
/ manifest cover item, the first image in page order for a comic archive, page
one for a PDF.

**FR-007** Books carry title, author, series and series index, publisher,
release date and description where the file provides them (epub OPF metadata /
ComicInfo.xml in a comic archive).

**FR-008** Book items expose no video/audio/runtime technical fields — no
`RunTimeTicks`, no `MediaStreams`, no `MediaSources` playback path.

**FR-009** Read progress is recorded per user and returned as
`UserData.PlaybackPositionTicks` / `Played`.

## Success Criteria

**SC-001** An `.epub`, a `.cbz` and a `.pdf` each open and turn a page in
unmodified jellyfin-web.
**SC-002** A book library of 500 files scans without a single ffmpeg invocation
attributable to a book path.
**SC-003** Covers render for ≥95% of files that contain one.
**SC-004** No book item response contains a `MediaSources` array or a non-null
`RunTimeTicks`.

## Assumptions

- Audiobooks are OUT of scope. Jellyfin models them as `MediaType: Audio` with
  `Type: AudioBook`; they are an audio-pipeline feature (chapters, resume,
  bitrate) that shares nothing with the reader path. Recorded here so the
  boundary is deliberate rather than forgotten.
- A single library root holds books of mixed formats.
- `.mobi` / `.azw3` are recognised for BROWSING but have no client reader, so
  they are listed and downloadable, not readable. Emitting them as readable
  would be a lie the UI would expose as a broken open.

## Known client limitations (not pharos defects)

- `bookPlayer.canPlayItem` compares `Path.endsWith("epub")` **case-sensitively**
  (`pdfPlayer` lowercases first). A file named `.EPUB` will not open. pharos will
  not misreport a path to work around this; it is recorded so the symptom is
  diagnosable.
- `comicsPlayer` loads its unpacker from `/libraries/worker-bundle.js`, served by
  the jellyfin-web bundle, not by pharos.

## Out of scope

Audiobooks; writing metadata back to files; an online metadata provider for
books (Google Books / Comic Vine) — the local-file metadata in FR-007 comes
first, and a provider slots into the existing resolver afterwards; a pharos-UI
(Dioxus) reader.
