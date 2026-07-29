# Feature Specification: native book support

**Feature directory**: `specs/004-books/`
**Created**: 2026-07-29
**Last revised**: 2026-07-29 (post-`/speckit-analyze` refinement — see §Revision log)
**Status**: planned (Phase 1 design complete; tasks.md needs the amendments in §Revision log)

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

**Independent test**: seed one `.pdf`, open it, confirm page one renders.

### US5 — resume where I left off (P3)

Reopening a part-read book returns to the last position.

**Independent test**: read a few pages of any readable format, navigate away,
reopen, and land at the same place — with no broken progress indicator (see
§Edge Cases).

## Edge Cases

Each already has a home in `tasks.md`; collected here so the set is reviewable
in one place rather than inferred from test names.

- **A book file that is not a valid archive** (truncated epub, no
  `META-INF/container.xml`). The item still imports with an empty metadata set;
  it is never silently dropped. A file that cannot be opened at all is skipped
  and logged with the path and the underlying error.
- **A readable format with no cover inside it.** No `Primary` image tag is
  advertised. Advertising one that 404s on every grid render is the failure this
  project has already paid for once.
- **A `.cbr`.** Lists, downloads and reads (the client unpacks rar itself), but
  pharos extracts no cover from it. Cover-less-but-readable, stated rather than
  papered over.
- **A `.mobi` / `.azw3`.** Indexed and downloadable, never presented as
  readable — no client ships a reader for either.
- **A book with the same title in two series**, or a series index absent. Sorts
  by title within the series; a missing index sorts last rather than as zero.
- **A part-read book's progress indicator.** See FR-008: runtime is not a
  meaningful quantity for a book, so no progress bar is expected. Acceptance
  must confirm the client renders *no* bar rather than a broken or NaN one —
  observed, not assumed.

## Requirements

### Functional Requirements

**FR-001** A book file is imported as an item of kind Book, with no ffmpeg
probe attempted. Books are not media files; probing them would fail and V6
would skip them, so the item would never exist.

**FR-002** `BaseItemDto.Type` is `Book` and `MediaType` is `Book`. Both players
gate on `canPlayMediaType(mediaType) === "book"` (lowercased compare).

**FR-003** `BaseItemDto.Path` is emitted when the client's `Fields` requests it.
All three readers gate on `canPlayItem`, which tests `item.Path` against an
extension. Without `Path` the item is silently unplayable — the failure mode
this project has hit repeatedly (see B159/B107 class: jellyfin-web refuses with
no error path). The field is recognised in every spelling a client dialect may
send it (V69), not only the canonical one.

**FR-004** `GET /Items/{id}/Download` serves the file bytes, honouring `Range`,
and authenticates via `?api_key=` as well as the `Authorization` header — the
client builds this URL with `api_key` in the query and no header.

**FR-005** A book library is a library of collection type `books`.

**FR-006** Covers come from the file itself:

- **epub** — the OPF `<meta name="cover">` / manifest cover item, else the first
  image in the spine.
- **comic archive** — the first image entry in page order.
- **PDF** — the embedded image of page one **when page one is a single
  full-page image** (the shape a scanned book or a comic-as-PDF has). A
  text-first PDF yields no cover, and none is advertised. See §Assumptions for
  why pharos does not rasterise.

**FR-007** Books carry title, author, series and series index, publisher,
**release date and description** where the file provides them. A file that
carries no title falls back to its filename, so no book is ever listed
untitled.

**FR-008** A book item is inert to the playback pipeline. Specifically:

- its `MediaSources` and `MediaStreams` are **empty**,
- its `RunTimeTicks` is **0**,
- and no `MediaSources` entry ever exists for a client to request a stream or
  transcode of.

Empty rather than absent or null, deliberately: array-typed fields are
default-empty across pharos because jellyfin-web iterates them without null
guards, and making them absent re-opens that class of client crash. What matters
is that there is nothing for a client to act on, not the JSON spelling of
nothing.

**FR-009** Read progress is recorded per user and returned as
`UserData.PlaybackPositionTicks` / `Played`.

**FR-010** A `PlaybackInfo` request naming a book yields no playable source. The
endpoint exists and is reachable for any item id; a book must leave it without
having entered codec negotiation.

### Key Entities

- **Book item** — a library item whose kind is Book. Carries the general item
  fields (title, release date, description, artwork, per-user progress) plus the
  book-specific set below. Never carries stream/codec facts.
- **Book metadata** — format, page count where the format has a stable one,
  author, publisher, series name, series index, ISBN. Distinct from the
  media-probe facts of a video or audio item; "author" and "pixel format" do not
  belong in one structure.
- **Book format** — a settled classification per file: ebook, PDF, comic
  archive, or *indexed-but-unreadable*. The last is a first-class case, not an
  omission from a list of readable extensions.
- **Book library** — a library whose collection type is books, so clients render
  a book grid rather than a video grid.

## Success Criteria

**SC-001** An `.epub`, a `.cbz` and a `.pdf` each open and turn a page in
unmodified jellyfin-web.

**SC-002** Scanning a book library invokes ffmpeg **zero** times for a book
path, at any library size. Counted, not sampled — the assertion is the count,
which does not depend on how many files are present.

**SC-003** Of the files whose format can carry a cover and do carry one, at
least 95% present one. The rate is **observable without inspecting the library
by hand** — a reader can ask the running server what fraction of book files
yielded a cover and what the others' reason was.

**SC-004** No book item offers anything to play: `MediaSources` and
`MediaStreams` are empty and `RunTimeTicks` is 0 on every response that carries
the item, including `PlaybackInfo`.

**SC-005** For every book file imported, the server can state which format it
was classified as and, when metadata is present, which source supplied each
field. A scan that silently produced the wrong classification is diagnosable
after the fact, not only while reproducing it.

## Assumptions

- **Audiobooks are OUT of scope.** Jellyfin models them as `MediaType: Audio`
  with `Type: AudioBook`; they are an audio-pipeline feature (chapters, resume,
  bitrate) that shares nothing with the reader path. Recorded here so the
  boundary is deliberate rather than forgotten.
- A single library root holds books of mixed formats.
- `.mobi` / `.azw3` are recognised for BROWSING but have no client reader, so
  they are listed and downloadable, not readable. Emitting them as readable
  would be a lie the UI would expose as a broken open.
- **pharos does not rasterise a PDF page.** Rendering a page to an image needs a
  C rendering library (pdfium, mupdf, poppler); pharos is a single binary with no
  runtime dependency beyond ffmpeg, and ffmpeg cannot read PDF. Extracting page
  one's *embedded* image needs only a parser, and it is exactly the case that
  matters — scanned books and comics-as-PDF are image-per-page. A text-first PDF
  gets no cover, which is the honest outcome; the alternative considered and
  rejected was advertising a cover the image route could never serve.
- Release date and description are ordinary item fields, populated from the book
  file by the same mechanism that populates them for any other item — books do
  not get a parallel metadata path.

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

## Revision log

`/speckit-analyze` on 2026-07-29 found one CRITICAL and five HIGH issues, of
which these were spec-level. Requirement ids are stable and were not renumbered;
FR-010 and SC-005 are appended.

| Finding | Was | Now |
|---|---|---|
| **I1** (CRITICAL) | SC-004/FR-008 demanded "no `MediaSources` array" and "non-null `RunTimeTicks`" | Restated as **empty** and **0**. The old wording was unsatisfiable — `RunTimeTicks` is a non-optional integer and `MediaSources` a non-optional array — and the obvious fix (omit them) re-opens the unguarded-iteration client crash those defaults exist to prevent. R9's goal is that nothing is offered to play; empty achieves it |
| **I3** (HIGH) | FR-006 required "rendered page 1" for a PDF | Narrowed to page one's **embedded** image. The planned crate parses but cannot rasterise, and a rasteriser is a C library that breaks single-binary deploy. Decided in the spec instead of left as an either/or inside a task |
| **G2** (HIGH) | FR-007 named release date and description; no entity or task carried them | Kept in FR-007 and pinned in §Assumptions as ordinary item fields on the shared path |
| **G1** (HIGH) | The no-negotiation rule lived only in the HTTP contract | Promoted to **FR-010**; SC-004 now names `PlaybackInfo` |
| **G4** (MEDIUM) | No title source for a file whose metadata has none | FR-007 gains a filename fallback |
| **A1** (MEDIUM) | SC-002 specified "a library of 500 files", never exercised | Restated size-independently; the assertion is a count of zero |
| **C1/G3** (HIGH/MEDIUM) | SC-003's 95% had no way to be measured | SC-003 now requires the rate be observable from the running server, and **SC-005** requires the classification decision be answerable after the fact. Together these make the scan's decisions queryable, which the constitution's observability principle requires of any branch choosing between behaviours |
| **U2** (LOW) | No Edge Cases section | Added; the cases already existed, scattered across test tasks |

**Downstream impact — `tasks.md` is now partly stale.** These amendments need
T027/T028 reworded to the empty/0 shape, a new task for FR-010's `PlaybackInfo`
assertion, T063 rewritten to embedded-image extraction, release date and
description added to T056, a filename-title fallback task, and a
classification-instrumentation task before T022 (which also serves SC-003 and
SC-005). The remaining analysis findings — I2 (the compiler does *not* enumerate
`matches!` sites), I4, G4's call site, I5, U1, D1 — are plan/tasks-level and are
untouched here.
