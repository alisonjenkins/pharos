# Implementation Plan: native book support

**Branch**: `004-books` | **Date**: 2026-07-29 | **Spec**: [spec.md](./spec.md)

**Input**: Feature specification from `specs/004-books/spec.md`

## Summary

Serve ebooks and comics natively — no plugin host, because pharos has none and
what Jellyfin's Bookshelf plugin adds to the *server* is, here, core scanner and
DTO work.

The finding that shapes everything: **the readers already ship in the client.**
The deployed jellyfin-web bundle contains `bookPlayer` (epub.js), `pdfPlayer`
(pdf.js) and `comicsPlayer` (libarchive.js). pharos renders no page, paginates
nothing and unpacks no archive for delivery. It must satisfy three gates those
players apply and then hand over bytes.

Two of those gates are currently **hard blockers**, both verified against the
tree rather than assumed:

1. `BaseItemDto` has **no `Path` field at all**, and all three players gate on
   `item.Path` ending in an extension. Absent → `canPlayItem` returns false and
   the client declines with no error, no toast, no request.
2. **`GET /Items/{id}/Download` does not exist** (only
   `/Items/{id}/RemoteImages/Download`). That is the URL every reader builds.

## Technical Context

**Language/Version**: Rust (workspace toolchain, pinned in `rust-toolchain.toml`)
**Primary Dependencies**: `pharos-core` (`MediaKind`, `LibraryKind`, `MediaItem`),
`pharos-scanner` (walk, extension classify, metadata resolver),
`pharos-jellyfin-api` (`BaseItemDto`), `pharos-server` (items/download handlers),
`pharos-cache` (cover art via the existing image cache)
**New crates**: `zip` for cbz + epub (epub is a zip); `sevenz-rust` for cb7;
`pdf`/`lopdf` for the PDF info dictionary + page one. **No ffmpeg involvement.**
**Storage**: seven nullable columns on `media_items`, one migration, no backfill
**Testing**: `cargo nextest`; a spy `Prober` proves SC-002 with no ffmpeg (V12)
**Target Platform**: Linux, k8s
**Project Type**: single Rust workspace
**Performance Goals**: a 500-file book library scans with zero ffmpeg invocations
(SC-002); cover extraction is one archive open per file
**Constraints**: no book item may carry `MediaSources` / `MediaStreams` /
non-null `RunTimeTicks` (SC-004) — those fields invite codec negotiation on an
epub
**Scale/Scope**: single household

**Resolved unknowns** (closed in [research.md](./research.md)):

- Who renders the book → **the client already does** (R1).
- Exact player requirements → `MediaType: "Book"` + `Path` + `/Download`,
  read off the shipped bundle (R2).
- Books through a probe-centric scanner → **classify by extension before the
  prober is reached**; a probe miss writes nothing (V6), so an epub handed to
  ffmpeg is an item that never exists (R4).
- Progress with no time axis → reuse `UserData.PlaybackPositionTicks`;
  `RunTimeTicks` stays null and no progress bar renders, deliberately (R8).

**NEEDS CLARIFICATION — non-gating**: R7, whether to add a rar reader for `.cbr`
cover extraction. `.cbr` files will list, download and *read* (libarchive.js
handles rar client-side); only server-side cover extraction is blocked, and
unrar is not in the devShell. Decide when a `.cbr` actually appears. Nothing else
in the design depends on it.

## Constitution Check

| Principle | Assessment |
|---|---|
| **I. Wire compatibility is the product** | This feature *is* wire compat — the acceptance test is unmodified jellyfin-web opening a book. Every requirement was derived by reading the deployed bundle, not the OpenAPI doc. `Book` and `books` are real Jellyfin tokens, and the DTO stays typed (V38) with enum-valued fields restricted to real members (V39). **PASS** |
| **II. Group sync** | Untouched. Books never enter SyncPlay. **N/A** |
| **III. Test-first, prove by query** | TDD per task; each gate gets a failing test first. ODD is thin here *by argument, not omission*: a book that will not open is a total, immediately visible failure, not a silent degradation — the class ODD exists for. The one non-obvious signal (which provider supplied metadata) already exists as B169's `pharos_metadata_field_source_total`, reused rather than duplicated. **PASS** |
| **IV. Never panics, never leaks, never lies** | No `unwrap`/`expect` (V17). A malformed epub is logged and skipped by the resolver, and the item still imports (V6). `/Download` resolves an id to a stored path, so there is no client-supplied path to traverse (V9). Errors carry the offending value. **PASS with one clause — see below** |
| **V. Types over conventions** | `MediaKind::Book` and `LibraryKind::Books` as variants, not booleans, so every exhaustive match fails to compile until it decides about books. `BookFormat::Unreadable` makes "indexed but no client reader" unrepresentable-as-readable rather than implied by an extension list elsewhere. `BookMeta` is separate from `MediaProbe` so "author" never sits beside "pix_fmt". **PASS** |

**Tech constraints**: sqlx behind the existing traits, both backends; `zip` /
`sevenz-rust` / a PDF crate are pure Rust with no runtime dependency, preserving
single-binary deploy; new deps mean `just hakari-regen` (CI fails on a stale
workspace-hack) and `just test-postgres` after any `sqlx::query*` edit.

### The one clause: V9 and `Path`

V9 is "media paths never reach an unauthenticated client". This feature emits a
filesystem path to a client for the first time, so it is stated explicitly rather
than left to interpretation:

- `/Download` and item fetches are **authenticated**, so V9 as written is not
  breached.
- `Path` is additionally **`Fields`-gated** — emitted only when the request asks
  (the client sends `Fields=CanDownload,Path`), reusing the existing gating that
  keeps `People` off default payloads. Least exposure.
- The alternative — a synthetic `Path` shaped to pass the extension test — was
  rejected: the field means a filesystem path, the metadata editor displays it,
  and lying resurfaces as a different bug later.

**No unjustified violations. No complexity-tracking entries required.**

## Project Structure

```
crates/pharos-core/src/lib.rs          MediaKind::Book, LibraryKind::Books,
                                       BookMeta, BookFormat, MediaItem.book
crates/pharos-scanner/src/
  fs.rs                                BOOK_EXTENSIONS, classify-before-probe
  book/mod.rs                          format dispatch
  book/epub.rs                         container.xml → OPF metadata + cover
  book/comic.rs                        zip/7z entry list → cover + page count
  book/pdf.rs                          info dictionary + page one
  metadata/book.rs                     MetadataProvider impl over BookMeta
crates/pharos-jellyfin-api/src/dto.rs  Path (Fields-gated), Book projection
crates/pharos-server/src/api/jellyfin/
  items.rs                             IncludeItemTypes=Book, Fields=Path,
                                       books collection type
  download.rs                          GET /Items/{id}/Download (new)
crates/pharos-store-sqlx/              migration + 7 columns, both backends
specs/004-books/                       this plan + artifacts
```

## Delivery order

Each step is independently revertable and leaves the tree compiling.

1. **`MediaKind::Book` + `LibraryKind::Books`.** Largest mechanical blast radius
   (every exhaustive match), zero behaviour change. Alone so the noisy diff never
   hides a behavioural one.
2. **Store columns + migration + `BookMeta` round-trip.** `just test-postgres`.
3. **`Path` on `BaseItemDto`, `Fields`-gated.** Useful on its own (the metadata
   editor), and it unblocks gate 2.
4. **`GET /Items/{id}/Download`** with `Range` + a truthful HEAD
   `Content-Length` (V113 — a sized body, since actix derives the header from
   `BodySize` and discards a hand-set one; this is B166's mistake, one endpoint
   over).
5. **Scanner: classify before probe + epub reader.** SC-002's spy-`Prober` test
   lands here. **US1 is testable end to end at this point.**
6. **Comic reader** (cbz/cb7) → US2.
7. **PDF reader** → US4.
8. **Book metadata provider** in the existing resolver → US3's authors/series.
9. **Cover extraction** through `set_artwork` (never `put` — B155: `put` does not
   maintain `has_primary_art`).
10. **Read progress** → US5.
11. `bugs.md` / `invariants.md` entries; a V-number for the `Path` clause.

MVP is steps 1–5: an epub that opens and turns a page in a stock client.

## Risks

| Risk | Handling |
|---|---|
| `MediaKind` variant blast radius | Its own commit, compiler-enumerated; no behaviour change to review alongside |
| A book reaching the transcode path | SC-004 asserts no `MediaSources`; `PlaybackInfo` returns none for a book (R9) |
| `.cbr` covers | Known gap (R7), recorded honestly — readable, cover-less. Advertising a cover that 404s is the B149 shape |
| `.EPUB` uppercase | Client-side case-sensitive compare (spec, Known client limitations). Not worked around by misreporting a path |
| Progress bar absent | By design (R8). Recorded so it is not "fixed" with a fabricated `RunTimeTicks` |

## Next command

`/speckit-tasks` — turn the delivery order into numbered tasks. This feature
carries its own `tasks.md` here; only small fixes against existing behaviour go
straight to `specs/001-pharos-baseline/tasks.md`.
