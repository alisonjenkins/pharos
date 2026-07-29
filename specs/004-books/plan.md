# Implementation Plan: native book support

**Branch**: `004-books` | **Date**: 2026-07-29 | **Spec**: [spec.md](./spec.md)
**Revised**: 2026-07-29 after `/speckit-analyze` — see §Revision log

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

1. `BaseItemDto` has **no `Path` field at all** (`dto.rs:407`), and all three
   players gate on `item.Path` ending in an extension. Absent → `canPlayItem`
   returns false and the client declines with no error, no toast, no request.
2. **`GET /Items/{id}/Download` does not exist** (only
   `/Items/{id}/RemoteImages/Download`). That is the URL every reader builds.

And one blocker that the first version of this plan got wrong, now corrected —
see **R10**: the type system will *not* catch the sites that decide a book's
`MediaType`. `dto.rs:1588` is a `match` with a `_ => "Video"` arm, so a `Book`
silently becomes a video at the single most load-bearing line in the feature.

## Technical Context

**Language/Version**: Rust (workspace toolchain, pinned in `rust-toolchain.toml`)
**Primary Dependencies**: `pharos-core` (`MediaKind`, `LibraryKind`, `MediaItem`),
`pharos-scanner` (walk, extension classify, metadata resolver),
`pharos-jellyfin-api` (`BaseItemDto`), `pharos-server` (items/download handlers),
`pharos-cache` (cover art via the existing image cache)

**New crates**: `zip` (cbz + epub — epub *is* a zip), `sevenz-rust` (cb7),
`lopdf` (PDF page tree + info dictionary). None are in `Cargo.lock` today.
**Already present, reused rather than added**: `quick-xml` is already a
`pharos-scanner` dependency (`Cargo.toml:29`), so `container.xml`, the OPF and
`ComicInfo.xml` need no new parser. **No ffmpeg involvement anywhere in this
feature.**

**Storage**: seven nullable columns on `media_items`, one migration, no backfill
**Testing**: `cargo nextest`; a spy `Prober` proves SC-002 with no ffmpeg (V12)
**Target Platform**: Linux, k8s
**Project Type**: single Rust workspace
**Performance Goals**: a book library scans with zero ffmpeg invocations at any
size (SC-002); cover extraction is one archive open per file
**Constraints**: no book item may offer anything to play — `MediaSources` and
`MediaStreams` empty, `RunTimeTicks` 0, `PlaybackInfo` yielding no source
(SC-004, FR-010)
**Scale/Scope**: single household

**Resolved unknowns** (closed in [research.md](./research.md)):

- Who renders the book → **the client already does** (R1).
- Exact player requirements → `MediaType: "Book"` + `Path` + `/Download`,
  read off the shipped bundle (R2).
- Books through a probe-centric scanner → **classify by extension before the
  prober is reached**; a probe miss writes nothing (V6), so an epub handed to
  ffmpeg is an item that never exists (R4).
- Progress with no time axis → reuse `UserData.PlaybackPositionTicks`;
  `RunTimeTicks` is 0 and no progress bar is expected (R8).
- What "no MediaSources" can actually mean → **empty, not absent** (R9).
- Whether the compiler finds every site → **no. 44 sites decide on item kind
  without an exhaustive match** — 34 `matches!`/`==` plus 10 wildcard arms
  (R10). This is the correction that reshaped the delivery order.
- PDF covers without a rasteriser → **pass through page one's embedded JPEG**
  (R11).
- Proving the classification by query → one counter, `label()`-backed (R12).
- `.cbr` cover extraction → **no rar reader** (R7, closed by the R11 precedent:
  `unrar` wraps a C library, the same objection that rules out a PDF
  rasteriser). `.cbr` lists, downloads and *reads* — libarchive.js handles rar
  client-side — and is permanently cover-less, counted as `rar_unsupported`.

**No unresolved clarifications remain.** R7 was the last one; it is settled by
principle rather than deferred to the first `.cbr` sighting.

## Constitution Check

| Principle | Assessment |
|---|---|
| **I. Wire compatibility is the product** | This feature *is* wire compat — the acceptance test is unmodified jellyfin-web opening a book. Every requirement was derived by reading the deployed bundle, not the OpenAPI doc. `Book` and `books` are real Jellyfin tokens, and the DTO stays typed (V38) with enum-valued fields restricted to real members (V39). `Path` is accepted in every spelling a client dialect may send (V69) — the camelCase-ignored class has bitten this project repeatedly. **PASS, with the R9 correction below** |
| **II. Group sync** | Untouched. Books never enter SyncPlay. **N/A** |
| **III. Test-first, prove by query** | TDD per task; each gate gets a failing test first. **Amended**: the first version of this plan argued ODD was thin here because a book that will not open fails visibly. That argument holds for the *symptom* but not for the constitution's decisions clause — T022 adds a branch choosing between behaviours (probe vs. book reader), so it is instrumented, in its own commit, before the branch lands (R12). That counter is also what makes SC-003 and SC-005 answerable. **PASS after amendment** |
| **IV. Never panics, never leaks, never lies** | No `unwrap`/`expect` (V17). A malformed epub is logged and skipped by the resolver, and the item still imports (V6). `/Download` resolves an id to a stored path, so there is no client-supplied path to traverse (V9). Errors carry the offending value. A cover is never advertised unless it exists. **PASS with one clause — see below** |
| **V. Types over conventions** | `MediaKind::Book` and `LibraryKind::Books` as variants, not booleans. `BookFormat::Unreadable` makes "indexed but no client reader" unrepresentable-as-readable. `BookMeta` is separate from `MediaProbe` so "author" never sits beside "pix_fmt". **PASS, but the earlier justification was wrong** — see R10. Adding a variant does *not* make the compiler enumerate every decision site, so the delivery order now opens with an explicit audit instead of trusting it |

**Tech constraints**: sqlx behind the existing traits, both backends; `zip` /
`sevenz-rust` / `lopdf` are pure Rust with no runtime dependency, preserving
single-binary deploy — **this is load-bearing, and it is why pharos does not
rasterise a PDF page** (R11). New deps mean `just hakari-regen` (CI fails on a
stale workspace-hack) and `just test-postgres` after any `sqlx::query*` edit.

### The one clause: V9 and `Path`

V9 is "media paths never reach an unauthenticated client". This feature emits a
filesystem path to a client for the first time, so it is stated explicitly rather
than left to interpretation:

- `/Download` and item fetches are **authenticated**, so V9 as written is not
  breached.
- `Path` is additionally **`Fields`-gated** — emitted only when the request asks
  (the client sends `Fields=CanDownload,Path`), reusing the existing gating that
  keeps `People` off default payloads (`items.rs:4874`). Least exposure.
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
  book/mod.rs                          format dispatch + classify counter
  book/epub.rs                         container.xml → OPF metadata + cover
  book/comic.rs                        zip/7z entry list → cover + page count
  book/pdf.rs                          info dictionary + page-one image
  metadata/book.rs                     MetadataProvider impl over BookMeta
crates/pharos-jellyfin-api/src/dto.rs  Path (Fields-gated), Book projection,
                                       the MediaType/has_primary decision sites
crates/pharos-server/src/api/jellyfin/
  items.rs                             IncludeItemTypes=Book, Fields=Path,
                                       books collection type, playback_info
  download.rs                          GET /Items/{id}/Download (new)
crates/pharos-store-sqlx/              migration 0052 + 7 columns, both backends
specs/004-books/                       this plan + artifacts
```

## Delivery order

Each step is independently revertable and leaves the tree compiling.

0. **Audit the non-exhaustive decision sites** (R10). 34 production sites match
   on item kind with `matches!`/`==`/a wildcard arm; the compiler will flag none
   of them. Enumerate them and record the intended verdict for `Book` at each,
   *before* adding the variant — because after step 1 the tree compiles clean
   and the list becomes invisible. Read-only; no code change.
1. **`MediaKind::Book` + `LibraryKind::Books`.** Largest mechanical blast radius
   (every exhaustive match), zero behaviour change. Alone so the noisy diff never
   hides a behavioural one. Apply step 0's verdicts to the `matches!` sites in
   the same commit — they are part of "adding the variant", not a follow-up.
2. **Store columns + migration + `BookMeta` round-trip.** `just test-postgres`.
3. **`Path` on `BaseItemDto`, `Fields`-gated**, accepting every spelling. Useful
   on its own (the metadata editor), and it unblocks gate 2.
4. **`GET /Items/{id}/Download`** with `Range` + a truthful HEAD
   `Content-Length` (V113 — a sized body, since actix derives the header from
   `BodySize` and discards a hand-set one; this is B166's mistake, one endpoint
   over).
5. **Classification instrumentation** (R12) — the counter ships and can be read
   before the branch it measures exists, per Constitution III.
6. **Scanner: classify before probe + epub reader.** SC-002's spy-`Prober` test
   lands here. **US1 is testable end to end at this point.**
7. **Book inertness**: `MediaType`, empty sources/streams, `RunTimeTicks` 0, and
   `PlaybackInfo` yielding no source (FR-010). Grouped, because they are one
   property — "nothing to play" — asserted at four sites.
8. **Comic reader** (cbz/cb7) → US2.
9. **PDF reader** → US4.
10. **Book metadata provider** in the existing resolver → US3's authors/series,
    plus release date, description and the filename title fallback.
11. **Cover extraction** through `set_artwork` (never `put` — B155: `put` does not
    maintain `has_primary_art`), and the `has_primary` decision site from step 0.
12. **Read progress** → US5.
13. `bugs.md` / `invariants.md` entries; a V-number for the `Path` clause.

MVP is steps 0–7: an epub that opens and turns a page in a stock client, and
provably offers nothing to transcode.

## Risks

| Risk | Handling |
|---|---|
| **`MediaKind` variant blast radius is invisible, not enumerated** | Step 0 audits it before the variant exists. This was the plan's worst earlier assumption (R10) |
| A book reaching the transcode path | Step 7 asserts empty sources at `/Items`, `/Items/{id}` and `PlaybackInfo`; R9 fixes what "empty" means so the assertion is satisfiable |
| Cover-less books advertising a poster | `has_primary` at `dto.rs:1880` is `!matches!(kind, Audio) \|\| has_primary_art`, so a book gets a tag unconditionally today. In step 0's list; asserted in step 11 |
| `.cbr` covers | Settled, not a gap: readable, cover-less, counted (R7). Advertising a cover that 404s is the B149 shape |
| PDF covers narrower than FR-006 implies | R11: only pass-through-encodable page-one images. There is no pure-Rust image decoder in the tree and adding a rasteriser breaks single-binary deploy |
| `.EPUB` uppercase | Client-side case-sensitive compare (spec, Known client limitations). Not worked around by misreporting a path |
| Progress bar absent | By design (R8). `RunTimeTicks` 0 must be confirmed to render *no* bar rather than a broken one — observed during step 12, not assumed |

## Revision log

`/speckit-analyze` found one CRITICAL and five HIGH issues. The spec-level ones
were fixed in [spec.md](./spec.md). These were plan-level:

| Finding | Change |
|---|---|
| **I2** (HIGH) | The Constitution Check claimed the compiler enumerates every site that must decide about books. **False** — 34 production sites use `matches!`/`==`/wildcard arms, including `dto.rs:1588`'s `_ => "Video"`, which decides the very gate FR-002 depends on. New **R10**, new **step 0**, and the V-principle justification rewritten. This was the single most load-bearing wrong assumption in the plan |
| **I1** (CRITICAL) | New **R9** replaces "no `MediaSources`" with "empty" and explains why absent is worse: `dto.rs:420` documents array fields as default-empty *because* jellyfin-web iterates them without null guards |
| **I3** (HIGH) | New **R11**. `lopdf` parses but cannot rasterise, and there is no pure-Rust image decoder anywhere in the tree, so PDF covers are pass-through embedded JPEGs only |
| **C1** (HIGH) | ODD reassessed. The earlier "thin by argument" position covered the symptom, not the constitution's *decisions* clause. New **R12** and delivery step 5 — instrumentation ships before the branch |
| **G1** (HIGH) | `PlaybackInfo` (route at `items.rs:105-106`, handler at `2081`) had no step. Now part of step 7 |
| **G2/G4** (HIGH/MEDIUM) | Release date, description and the filename title fallback named in step 10 |
| **D1** (MEDIUM) | `READABLE_BY_CLIENT` dropped as a second authority — derived from `BookFormat` in [data-model.md](./data-model.md) |
| **I5** (MEDIUM) | `Fields` spelling variants (V69) added to step 3 and the Constitution Check |
| Dep discovery | `quick-xml` is already a `pharos-scanner` dep — no XML crate needed, contrary to the earlier implication |

## Next command

`/speckit-tasks` — regenerate tasks against this order. The existing `tasks.md`
predates these corrections and is stale in the ways spec.md §"Downstream impact"
lists; steps 0, 5 and 7 have no tasks at all.
