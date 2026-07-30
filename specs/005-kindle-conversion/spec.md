# 005-kindle-conversion — make DRM-free Kindle books readable

**Status**: implemented 2026-07-30
**Depends on**: 004-books (book indexing, `/Items/{id}/Download`, `BookFormat`)

## The problem, measured

004-books shipped `.mobi` / `.azw` / `.azw3` as `BookFormat::Unreadable`: indexed
and downloadable, never claimed as readable, because jellyfin-web ships exactly
three readers — epub.js (`.epub`), libarchive.js (`.cbz`/`.cbr`/`.cbt`/`.cb7`)
and pdf.js (`.pdf`) — and none of them claims a Kindle file. Clicking one gave
"This ebook cannot be opened".

That was **75 of the deployed library's 142 books**. Probing the MOBI
encryption flag (PalmDOC header, offset 12) across all 75 split them:

| | count | outcome |
|---|---|---|
| `mobipocket-drm` | **58** | permanently unopenable — out of scope, see below |
| no DRM | **17** | convertible |

The 17 span every container shape the format has, which is why none of this is
optional:

| shape | count | needs |
|---|---|---|
| MOBI6 + PalmDOC LZ77 | 8 | LZ77 decode |
| KF8 + PalmDOC | 2 | LZ77 + skeleton/fragment rebuild |
| KF8 + HUFF/CDIC | 7 | Huffman decode + skeleton/fragment rebuild |

No file in the library is a dual-format MOBI6+KF8 container (the EXTH type-121
boundary record is absent in all 75), so each is purely one generation.

## Scope

**In**: converting DRM-free `.mobi`/`.azw`/`.azw3` to EPUB, and reading their
metadata and covers.

**Out**: DRM circumvention. The 58 protected files are tied to a registered
Amazon device; pharos does not decrypt them and will not. They keep the exact
004-books behaviour — indexed, downloadable, honestly reported as unreadable.
The only real fix is re-acquiring those titles DRM-free.

## Requirements

- **FR-001** A DRM-free Kindle file is delivered as EPUB from
  `/Items/{id}/Download`, with `Content-Type: application/epub+zip`.
- **FR-002** Its `Path` ends in `epub`, because `bookPlayer`'s gate is
  `item.Path?.endsWith("epub")` (V117 / B170). The advertised path is the REAL
  converted file, never the source path with its extension rewritten.
- **FR-003** A DRM-protected or malformed Kindle file keeps the 004-books
  behaviour and is never reported as readable.
- **FR-004** Title, author, publisher and date are read from the container's
  EXTH records and flow through the ordinary metadata resolver, so a book is no
  longer titled from its filename.
- **FR-005** The cover is extracted through the ordinary cover path.
- **FR-006** Conversion failure degrades to serving the original file, never a
  5xx.

## Success criteria

- **SC-001** All 17 DRM-free library files convert. *Measured: 17/17.*
- **SC-002** Output is a spec-valid OCF zip (`mimetype` first and STORED,
  `META-INF/container.xml` present, CRCs intact). *Measured: 17/17.*
- **SC-003** Covers. *Measured: 17/17, against 31% before (sidecars only).*
- **SC-004** DRM files fail with a named cause and no panic. *Measured: 6/6.*
- **SC-005** Conversion is fast enough to do on demand. *Measured: 8–62 ms.*

## Decisions

### D1 — `boko`, not a hand-written decoder

The HUFF/CDIC Huffman coder and the KF8 skeleton/fragment index rebuild are
roughly 2000 lines against a format with no specification. `boko` 0.5.0 carries
both, in pure Rust with **zero `unsafe`** and no C library — consistent with the
rules that keep `.cbr` cover-less (R7) and PDFs un-rasterised (R11).

Verified empirically before adopting, not from the crate description: all 17
files converted, all 17 outputs validated structurally, all 6 DRM samples
rejected cleanly.

Risks accepted: it is a young crate (v0.5.0, one author) with 393
`unwrap`/`panic` sites. That is the same exposure lopdf already presents (173),
and V119's `guard_parser` is the existing containment — a panic costs one
skipped book, not the scan. On the delivery side `spawn_blocking` gives the
same containment via `JoinError`.

### D2 — GPL-3.0-or-later is compatible, and scoped

`boko` is GPL-3.0-or-later; pharos is AGPL-3.0-or-later. GPLv3 §13 exists for
exactly this pairing, so the combination is permitted rather than tolerated, and
costs nothing — the project is already copyleft. Recorded as a per-crate
exception in `deny.toml`, not a global `GPL-3.0-or-later` allow, so the next
GPL dependency stays a deliberate decision.

### D3 — convert on delivery, read metadata at scan

The media share is mounted **read-only**, so converted bytes cannot go beside
the source. Scanning reads metadata and the cover (the parts an item row
persists) and does not materialise an EPUB nobody has asked for; the download
route converts and caches. At 8–62 ms the first open is imperceptible, and the
cache becomes self-healing: wipe the PVC and the next request rebuilds, with no
rescan and no stale row to correct.

### D4 — converted-ness is derived, not stored

No column, no migration. `book_format` answers "what can a client read this
as", which after conversion genuinely is epub; the path keeps answering "what
is the file on disk". Those disagreeing IS the record of a conversion
(`is_converted_kindle`). See **V121**.

## Observability

`pharos_book_convert_total{stage,source,outcome,reason}`.

- `stage` — `scan` counts what the library CONTAINS (run it over
  `outcome="drm_protected"` for the size of the locked-away shelf); `deliver`
  counts converted-cache MISSES, so a rate that does not settle to ~zero means
  the cache is being evicted or wiped, which the scan-side number cannot show.
- `outcome` — `converted` / `drm_protected` / `failed`. DRM is its own outcome
  and not a failure (**V122**).
- `source` — bounded to `mobi` / `azw` / `azw3` / `other`.

```promql
sum by (outcome) (pharos_book_convert_total{stage="scan"})
```

## Known limitations

- **58 DRM files stay shut.** Not a defect.
- `.cbr` remains cover-less (R7) and PDFs un-rasterised (R11) — unchanged.
- Two advisories against `quick-xml` 0.39, which `boko` pins, are ignored on
  reachability grounds with the argument recorded in `deny.toml`; see **V123**.
  They lift when `boko` moves to 0.41.
