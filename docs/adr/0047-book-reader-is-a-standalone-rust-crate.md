# ADR-0047: The book reader is a standalone Rust crate pair, laid out by the browser

- **Status:** Proposed
- **Date:** 2026-11-04T00:55:00Z
- **Deciders:** Alison Jenkins

## Context

pharos-ui cannot open books (B410, spec 023). The library holds 56 books
delivered as EPUB (17 of them Kindle files converted by 005), 28 PDFs and 58
DRM-protected Kindle files. Alison asked for the reader to be Rust and WASM,
and for it to be its own reusable crate she could use outside pharos.

The options for EPUB were:

1. **epub.js behind a port**, as jellyfin-web does. Proven, but JavaScript,
   which is what Alison asked to move away from, and not reusable from Rust.
2. **A Rust layout engine** that shapes and breaks text itself and draws to a
   canvas. Full control, but it rebuilds CSS, fonts, bidi and hyphenation the
   browser already has, and loses text selection and accessibility.
3. **Rust parses and sanitises; the browser lays out.** Rust owns the
   container, metadata, reading order, sanitising and positions; the browser's
   CSS multi-column layout makes pages.

## Decision

We will build option 3 as two crates with no pharos dependency.

- `quire` is the format model and parsers, plain Rust with no DOM, testable
  natively. `quire-web` mounts a `quire` section into a shadow root, paginates
  with CSS columns and reports positions. pharos-ui holds the Dioxus view and
  the native API calls; nothing in the crates names pharos.
- Both crates (and `quire-pdf`, slice 5) are excluded from `workspace-hack`, carry their own README and
  licence, and must build for wasm32 and the host (V350), so either can move
  to its own repository without change.
- Zip, XML, HTML sanitising and any PDF engine are adapters behind traits in
  `quire` (009 §Boundaries).
- Book content is untrusted and sanitised in `quire` before it reaches the DOM
  (V351).
- A position is a `quire` locator string (format, reading-order index,
  offset in UTF-16 code units, the unit DOM ranges use) stored in its own column, not packed into
  `PlaybackPositionTicks`, so the native reader and jellyfin-web never clobber
  each other (V354).

### The PDF engine: hayro, in the browser, as a lazily loaded module

Decided 2026-11-10 (slice 5), after a spike (2026-11-09) on the 28 live PDFs.

- **Engine: `hayro`** (crates.io, MIT or Apache-2.0), a pure-Rust renderer.
  Spike, hayro 0.9.0: all 28 PDFs opened and drew pages 1, 2 and the middle
  with no panic; native first page (open plus render) median 22 ms, p90 43 ms;
  against mupdf at 1080 px the mean grey difference was 2.6 median and 9.1
  worst, all sub-pixel glyph placement. In wasm (node) a page took 39 to
  414 ms, about 2 to 8 times native. Caveats: the corpus is text-heavy
  technical books (no scans, CJK or complex vector art), and hayro has no
  decryption, blending or isolation groups, or knockout groups. Encrypted
  files are refused with an error and the download offered.
- **Rendered in the browser**, not on the server (Alison, 2026-11-10: "I want
  to go the route of local as I want the option of allowing the user to read
  offline ... They would need the wasm reader and the book for that"). Server
  rendering of one page (about 3 ms after open, WebP about 58 KB median) was
  measured and is faster for the client, but it would need the server for
  every page turn.
- **A separate wasm module, loaded on demand.** The engine is 4.99 MB raw and
  1.86 MB gzip (opt-level z, fat LTO, `wasm-opt -Oz`; the spike measured
  1.71 MB gzip), against a main UI wasm of 5.15 MB raw and 2.07 MB gzip that
  TVs and Quest headsets compile at every cold start. So it is never linked
  into the main wasm (which grew 23 KB raw, 7 KB gzip, for the canvas
  reader itself). `quire-pdf` builds as its own `wasm-bindgen --target
  web` module under the UI's `assets/pdf/`, and a 10-line loader script
  (a static `<script type=module>`) starts it; the reader adds that script
  only when a PDF is opened. The UI's CSP needed no change: it already allows
  scripts from 'self' and `'wasm-unsafe-eval'`, and an inline `import()`
  would have needed eval. A Playwright check asserts no request to the module
  happens before a PDF is opened (V356).
- **Behind the port.** `quire` has the `FixedLayout` trait (page count, page
  size, render page `i` at a pixel height to RGBA) and the `pdf1:<page>`
  locator, and does not depend on hayro. `quire-pdf` is the adapter, a
  standalone crate with no pharos dependency (V350). Changing engine means
  replacing `quire-pdf` and the module's file names.
- **The reader** draws one page on a canvas sized to the window at up to 2x
  pixel ratio and 4096 px a side, draws the next page ahead into one spare
  canvas, and saves `pdf1:<page>` through the existing position store. The
  64 MiB weak-device cap applies to PDFs as to EPUBs, checked on
  `Content-Length`. The file goes from the browser's buffer straight into
  the module's memory, not through the main wasm's.

PDFium (2.5 MB gzip) and pdf.js (about 0.7 MB gzip plus decoders and
fonts) were not built; hayro won on size against PDFium, on being Rust against
pdf.js, and on the spike's coverage.

Not done: reading with no network after the first open (the module and the
book are cacheable, but nothing stores them yet), a table of contents (PDF
outlines), text selection and search, zoom beyond the two fits.

## Consequences

- No JavaScript reader ships; the reader's logic is testable with
  `cargo test`.
- Layout quality is the browser's, which is good, and its quirks (column
  breaks inside images and tables) are ours to cap with CSS.
- Embedded EPUB fonts are dropped until the UI CSP allows `font-src blob:`.
- jellyfin-web's readers keep working and keep their own position; a book read
  in both clients has two positions until a later change maps between them.
- A second consumer of `quire` (another app, or a CLI) costs nothing in
  pharos.
