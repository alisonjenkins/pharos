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
- Both crates are excluded from `workspace-hack`, carry their own README and
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

### Open: the PDF engine

Slice 5 picks it after a spike that opens the 28 live PDFs and measures wasm
size and first-page time. Candidates:

- a pure-Rust PDF renderer compiled to wasm (for example `hayro`; maturity and
  coverage to be measured, not assumed);
- PDFium compiled to wasm (complete, several MB);
- pdf.js behind the fixed-layout port (complete, JavaScript).

Whatever wins sits behind `quire`'s fixed-layout port, so the choice can be
revisited without touching `quire-web` or pharos-ui. This ADR is amended with
the result.

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
