# ADR-0144: PDFs reach the Kobo as pages drawn by Pharos

- **Status:** Accepted
- **Date:** 2026-11-16T23:40:00Z
- **Deciders:** Alison

## Context

The Kobo app (ADR-0136) reads EPUBs with Cobalt's `kobo-bookview`. It had no
way to open a PDF and said "This reader can't open PDF books yet."

A PDF is laid out for a fixed page. Reading one well on a 6–7 inch panel
means drawing each page as a picture and letting the reader zoom and move
around it. Cobalt already has a comic viewer that does that for CBZ files:
page turns, fit page or width, zoom to 400 %, page list, go to page. It loads
the whole archive into memory, capped at 32 MiB.

Pharos already carries a pure-Rust PDF engine, `hayro`, behind `quire-pdf`
(the web reader's PDF module). The Kobo has 1 GB of RAM shared with the
system, a slow ARM CPU, and keeps everything it downloads on a FAT card.

Measured on a dense text page drawn at the Libra Colour's 1264 x 1680:
about 15 ms to draw on the server, 300 KB as a colour JPEG, 86 KB as a
16-grey PNG.

## Decision

We will have Pharos draw a PDF's pages and the Kobo show them as pictures.

- `GET /Items/{id}/Download?pages=pictures&maxWidth=W&maxHeight=H[&grey=true]`
  on a PDF answers with a CBZ (`application/vnd.comicbook+zip`): one stored
  member per page, `page-NNNN.png` or `.jpg` in order, each drawn by
  `quire-pdf` to fit the box (default 1264 x 1680, no edge over 3000 px).
- A page with no colour on it (at most one pixel in 1000 with an RGB spread
  over 24) is a 4-bit greyscale PNG, the panel's 16 greys. A page with colour
  is a JPEG at quality 80, unless `grey=true` asks for grey throughout.
- The copy is built once into the book cache directory (`pages/`), keyed by
  item, version, source mtime and size, box and colour choice. One book is
  drawn at a time; a request waits up to 120 s for the slot, then re-checks
  the cache. The drawing is not cancelled when its request goes away.
- There is no fallback to the PDF: a client that asked for pictures cannot
  open it. A PDF that will not draw answers 422 with the reason; no cache, a
  busy slot or a source that changed mid-draw answers 503. A page that fails
  to draw alone is a white page, so the page numbers still match.
- Every request counts on `pharos_book_pages_total{outcome=drawn|cache_hit|
  no_cache|busy|failed}` and logs one `book pages:` line with the box, the
  page counts by kind, the bytes and the time taken.
- The Kobo app asks for its panel's size (portrait), and for grey on a panel
  without colour. It saves the CBZ to the shelf like any book (ADR-0142).
- `kobo-bookview` gains `ShelfComic` (in `kobo/cobalt.patch`): it reads the
  archive's directory off the card, then each page when it is wanted, and
  the next two and the one before while a page is shown. `kobo-comic`'s
  reader holds at most 8 pages' encoded bytes. The comic viewer stays on the
  page drawn until the one asked for is in, so each page is drawn once.

## Consequences

- PDFs open on the Kobo with fast page turns, since the next page is read
  ahead and the device only decodes a PNG or a JPEG.
- No size limit beyond the shelf's (ADR-0142) on the Kobo, and 512 MiB and
  4000 pages on the server, where `hayro` holds the whole PDF in memory.
- Text is sharp at the panel's size but softens when zoomed past it: the
  pages are drawn for the panel, not for zoom.
- The first download of a PDF waits while the server draws every page, which
  for a long book can outlast a proxy's timeout. The drawing finishes anyway,
  and asking again is served from the cache.
- An encrypted PDF is refused: `hayro` cannot decrypt.
- The server binary now links `hayro`.
- "Read the opening" (ADR-0140) is not offered for a PDF, since it reads only
  EPUB text.
- CBZ comics could use the same viewer and path; they are not wired up yet.

## Alternatives considered

- **Draw pages on the Kobo** (`hayro` or `mupdf` on ARM). Sharp at any zoom
  and no server work, but every page turn renders on a slow CPU and the
  whole PDF sits in scarce RAM. Larger to build (XL against L).
- **Stream one page picture at a time while reading.** Smallest to build, but
  nothing reads without Wi-Fi, and every turn waits on the network.
- **Load the CBZ whole into Cobalt's comic viewer.** The 32 MiB cap is
  around 100 to 350 pages, so long PDFs would not open.

## References

- V1460, V1461, V1462 in `specs/001-pharos-baseline/invariants.md`.
- ADR-0136 (Cobalt app), ADR-0138 (slimmed EPUB), ADR-0142 (books off the card).
- `crates/pharos-server/src/book_pages.rs`, `kobo/cobalt.patch`
  (`crates/kobo-bookview/src/shelf_comic.rs`), `kobo/app/src/reading.rs`.
