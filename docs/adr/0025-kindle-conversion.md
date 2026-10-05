# ADR-0025: Kindle-format book conversion (boko, EPUB on delivery, derived state)

- **Status:** Accepted
- **Date:** 2026-10-05T00:00:00Z (shipped 2026-07-30; backfilled 2026-10-05)
- **Deciders:** Alison

Backfill: this ADR records a decision already in the code, written from
`specs/005-kindle-conversion/spec.md` and the code it points to.

## Context

ADR-0024 (Books library kind) indexes `.mobi`, `.azw` and `.azw3` files but
marked them `BookFormat::Unreadable`: listed and downloadable, never claimed as
readable. jellyfin-web ships three book readers (epub.js, libarchive.js,
pdf.js) and none opens a Kindle file, so a click gave "This ebook cannot be
opened".

On the deployed library that was 75 of 142 books. Probing the MOBI encryption
flag split them:

- **58 are DRM-protected** (`mobipocket-drm`). Permanently unopenable.
- **17 are DRM-free.** They span three container shapes: MOBI6 with PalmDOC
  LZ77 (8), KF8 with PalmDOC (2), KF8 with HUFF/CDIC (7).

The same files were also the metadata hole: titles came from the filename and
the cover rate was 31%, all from `cover.jpg` sidecars.

## Decision

Convert DRM-free Kindle files to EPUB with the `boko` crate, on the first
download, and cache the result. Read their metadata and cover at scan.

- **What and to what.** DRM-free `.mobi`/`.azw`/`.azw3` become EPUB, served
  from `/Items/{id}/Download` as `application/epub+zip`. The item's `Path`
  advertises the real converted file, ending in `epub`, because `bookPlayer`
  gates on `item.Path?.endsWith("epub")` (V117 / B170).
- **Library: `boko` 0.5.0, not a hand-written decoder.** The HUFF/CDIC coder
  and the KF8 skeleton/fragment rebuild are about 2000 lines against an
  undocumented format. `boko` is pure Rust with no `unsafe` and no C library.
  A panic costs one skipped book: `guard_parser` (V119) contains it at scan and
  `spawn_blocking` contains it on delivery.
- **When.** Scan reads EXTH metadata (title, author, publisher, date) and the
  cover. Delivery converts. The media share is read-only, so converted bytes
  cannot sit beside the source, and scan-time output would be written for
  books nobody opens.
- **Cache.** One file per item, `<media id>.epub`, keyed on the integer id and
  never on the source path. Written by tmp-file rename. Two simultaneous first
  opens both convert; last writer wins on identical bytes. Disk is the
  authority: a miss regenerates, so wiping the cache PVC costs one rebuild.
- **State is derived, not stored (V121).** `book_format` records what a client
  can read the item as (epub). `path` records what is on disk (`.azw3`). Their
  disagreement identifies a converted item (`is_converted_kindle`). No column,
  no migration.
- **Rescan trigger.** Book rows are stamped with `BOOK_SCHEMA_VERSION` (in
  `pharos-core`), not `PROBE_SCHEMA_VERSION`. Bumping the probe version would
  re-probe about 14,000 videos; leaving it alone left the 75 Kindle rows
  skipped by `(mtime, size)`. Both versions use the same
  `probe_schema_version` column, and the row's `kind` picks which governs.
- **Failure degrades.** A conversion failure serves the original file, never a
  5xx (FR-006). DRM files keep the ADR-0024 behaviour.
- **DRM is out of scope.** pharos does not decrypt and will not. DRM is its own
  outcome, not a failure, logged at debug (V122).
- **Signal.** `pharos_book_convert_total{stage,source,outcome,reason}`.
  `stage=scan` counts what the library holds. `stage=deliver` counts cache
  misses, so a rate that does not settle near zero means eviction.

## Consequences

- 17 of 17 DRM-free files convert, with valid OCF zips and 17 of 17 covers.
  Conversion took 8 to 62 ms, so the first open is not noticeable.
- The 58 DRM files stay shut. The only fix is re-acquiring them DRM-free.
- `boko` is young (v0.5.0, one author, 393 `unwrap`/`panic` sites). The
  exposure is accepted on the strength of the panic containment above.
- `boko` is GPL-3.0-or-later and pharos is AGPL-3.0-or-later, which GPLv3 s13
  permits. `deny.toml` carries a per-crate exception, not a global allow, so
  the next GPL dependency stays a deliberate decision.
- `boko` pins `quick-xml` 0.39. Two advisories (RUSTSEC-2026-0194, -0195) are
  ignored on reachability arguments recorded in `deny.toml` (V123). Re-check
  them if pharos ever hands `boko` an `.epub`. Drop the entries when `boko`
  moves to `quick-xml` 0.41.
- The converted-ness check is two fields agreeing or not. A future change that
  stores `book_format` differently breaks it silently, so keep V121 in view.

## Alternatives considered

- **Hand-written decoder.** Rejected on cost: about 2000 lines for a format
  with no specification (spec D1).
- **Convert at scan.** Rejected: read-only share, and output for unread books
  (spec D3).
- **A stored "converted" flag.** Replaced by the derived check (spec D4). The
  spec says only that the cache is self-healing and the flag was not; any
  further rationale is not recorded.
- **DRM circumvention.** Out of scope by decision (spec Scope).

## References

- `specs/005-kindle-conversion/spec.md` (FR-001 to FR-006, D1 to D4,
  Observability, Known limitations); `specs/004-books/`
- `specs/001-pharos-baseline/invariants.md` V117, V119, V121, V122, V123
- `specs/001-pharos-baseline/bugs.md` B170
- `crates/pharos-scanner/src/book/kindle.rs`,
  `crates/pharos-server/src/book_convert.rs`,
  `crates/pharos-server/src/api/jellyfin/download.rs`
- `crates/pharos-core/src/lib.rs` (`BOOK_SCHEMA_VERSION`), `deny.toml`
- ADR-0024 (Books library kind)
