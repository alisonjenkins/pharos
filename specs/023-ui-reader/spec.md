# Feature specification: read books in pharos-ui

**Feature directory**: `specs/023-ui-reader/`
**Created**: 2026-11-04T00:55:00Z
**Status**: draft
**Input**: "We cannot currently read any of the books, if possible please again
make it so we have something in Rust + WASM for doing that" and "Make sure that
is its own re-usable crate as that is a nice thing that I might want to use
elsewhere if we make it ourselves" (Alison, 2026-11-04).
**Depends on**: 004-books (indexing, `BookFormat`, `/Items/{id}/Download`),
005-kindle-conversion (DRM-free Kindle delivered as EPUB), 017 (native API),
009 §Boundaries (ports and adapters), 012 (browse), ADR-0047
**Ids**: bugs from B410, invariants from V350 (B410–B419 and V350–V359 are
reserved for this feature)

## Problem

pharos-ui lists books, draws their covers and opens their title pages, but no
book can be read in it. Nothing in the UI fetches a book's bytes and there is
no reader. The only way to read one is the old jellyfin-web client, whose
readers are JavaScript (epub.js, pdf.js). This is B410.

The native API also knows nothing a reader needs:

1. `ItemSummary` and `ItemDetail` carry no book format, so the UI cannot tell
   an EPUB from a PDF from a book that cannot be opened at all.
2. `UserData` has `setPlayed` and `setFavorite` but no way to store a reading
   position, and `Playback.negotiate` refuses a book (004 FR-008: a book has
   nothing to stream). So a native reader has nowhere to save where you were.

## The library, measured (2026-11-04)

Live `media_items` where `kind = 'book'`, by stored `book_format`, and the
files behind them:

| `book_format` | count | files | readable by this feature |
|---|---|---|---|
| `epub` | **56** | 39 `.epub`, 17 DRM-free `.azw3`/`.azw`/`.mobi` | yes, slice 1 |
| `pdf` | **28** | 28 `.pdf` | yes, slice 2 |
| `unreadable` | **58** | DRM-protected Kindle files | no (005 §Scope); said so plainly |
| `comic` | **0** | none | later, slice 3 |

The 17 Kindle files read as EPUB because 005 already converts them:
`/Items/{id}/Download` serves the converted EPUB with
`Content-Type: application/epub+zip`. The reader never sees a Kindle file.

So EPUB first reaches 56 of 142 books, PDF next brings it to 84, and the 58
DRM files stay closed whatever the reader does. Comics come last because the
library has none.

## Reusable crates (Alison's requirement)

The reader is a pair of crates that know nothing about pharos, so they can be
used in another project or moved to their own repository unchanged.

- **`quire`**: the model and the parsers. It opens a publication from bytes,
  gives its metadata, table of contents and reading order, hands back each
  section as sanitised XHTML with its resources, and turns a reading position
  into a string and back. Plain Rust: no `web-sys`, no DOM, no async runtime.
  Its tests run natively.
- **`quire-web`**: drawing a `quire` publication in a browser. It mounts a
  section into a shadow root on an element the caller supplies, paginates it,
  turns pages, applies reader settings and reports the position. It depends on
  `quire`, `web-sys` and `wasm-bindgen`, and on no UI framework.
- **pharos-ui** holds the only pharos-specific parts: the `/read/{id}` route,
  fetching the bytes, the settings panel as Dioxus components, and saving the
  position through the native API.

The name `quire` is a working name; it is checked against crates.io before the
first crate PR and changed there if it is taken.

## User stories

### US1: read an EPUB (P1)

From a book's title page, "Read" opens the book full screen, at the start the
first time and where you stopped after that. Pages turn with the arrow keys,
Page Up/Down, Space, a click or tap on the left or right third of the page and
a horizontal swipe. A table of contents jumps to a chapter. A progress line
shows how far through the book you are.

**Acceptance**:

1. Given an EPUB never opened, Read opens it at the first item of the reading
   order and shows page 1.
2. Given a book closed part way through chapter 4, Read reopens it in chapter
   4 with the same passage on screen.
3. Given the table of contents, choosing a chapter shows that chapter's first
   page.
4. Given a DRM-free `.azw3`, Read opens the EPUB the server converted it to.

### US2: make it comfortable (P1)

The reader has its own settings: font size (in steps), font (the book's own,
serif or sans), line spacing, margins, and a page theme (follow the pharos
theme, paper, sepia, night). Settings apply at once without losing your place,
and are kept per device.

**Acceptance**:

1. Given a page in the middle of chapter 2, raising the font size keeps the
   first visible sentence on screen.
2. Given the night page theme, text and background follow it and the book's
   own colours do not override it.

### US3: carry on from another device (P1)

Where you are in a book is saved to your account while you read and when you
leave, so opening the same book on another device starts where you stopped.
The title page shows how far through you are, and a "Continue reading" row on
the home page lists books part read (FR-012).

**Acceptance**:

1. Given a position saved on one browser, opening the book in another browser
   shows the same passage, even with a different window size and font size.
2. Given a book read to the last page, it is marked read.

### US4: a book that cannot be opened says why (P1)

A DRM-protected Kindle book shows no Read button. Its title page says it is
locked to a Kindle device and cannot be opened here, and offers the download.
A file that fails to parse opens to a plain error with the download offered,
never a blank page.

### US5: read a PDF (P2)

A PDF opens in the same reader with fixed pages: page turns, zoom, fit width
or page, the page number as the position. Engine choice is ADR-0047's open
question and is decided by a spike before slice 2.

### US6: read a comic (P3)

CBZ pages shown one or two at a time, right-to-left optional. Deferred until a
comic is in the library; the `quire` model already has fixed-layout
publications so it needs no redesign.

## Requirements

- **FR-001** `quire` and `quire-web` depend on no `pharos-*` crate and on
  `workspace-hack` neither (hakari `final-excludes`), each has its own README
  and licence, and both build for `wasm32-unknown-unknown` and for the host.
  (V350)
- **FR-002** Every outside library the crates use (zip, XML, HTML sanitising,
  a PDF engine) sits behind a trait in the crate, with the concrete library in
  an adapter module (009 §Boundaries).
- **FR-003** Book content is untrusted. Before anything reaches the DOM,
  `quire` removes scripts, `on*` attributes, `javascript:` and other
  non-resource URLs, forms, iframes, objects, `target` attributes and remote
  references, and rewrites resource references (images, inline styles) to
  resources inside the book. A link to another section or an anchor in one
  becomes an in-reader jump; a link to anything else inside the book (an SVG,
  a stylesheet, a stray XHTML file) is made inert; an external `http(s)` link
  opens in a new tab with `rel="noopener noreferrer"`. A `blob:` URL is never
  a link target, so book content is never navigated to as a document in the
  pharos origin. The page's CSP stays as it is for scripts. (V351)
- **FR-004** A section's styles are scoped to it: content is mounted in a
  shadow root, so a book's CSS cannot restyle pharos-ui and pharos-ui's CSS
  cannot break the book's layout.
- **FR-005** A reading position is a `quire` locator: format, reading-order
  index and an offset in UTF-16 code units into that section's sanitised text
  (the unit DOM ranges use), plus the whole-book fraction for progress
  display. It is independent of window size, font size and pagination, and it
  names the first character drawn on the page even when that character is in
  the middle of a paragraph that began on an earlier page. (V352)
- **FR-006** The native API adds, in one schema version: the book format and
  whether it can be read on `ItemDetail`; `UserData.setReadPosition(id,
  locator, fraction)`; and the stored locator and fraction on `UserState`.
  Saving is debounced (at most every 5 s while reading) and also sent on
  `visibilitychange` to hidden and on `pagehide`. Those are best effort over
  the RPC socket: if the page is gone before the call is sent, the last
  debounced save stands, so at most 5 s of reading is lost.
- **FR-007** The locator is stored apart from `PlaybackPositionTicks`, so
  jellyfin-web's readers and the native reader do not overwrite each other's
  positions. (V354)
- **FR-008** An `unreadable` book never opens the reader. (V353)
- **FR-009** The reader fetches bytes from `/Items/{id}/Download` with a
  same-origin, credentialed `fetch`, authenticated by the `pharos_media`
  cookie (ADR-0034), never with a token in the URL. The native session token
  is `HttpOnly`, so the UI has no token to send. The route today takes
  `AuthUser`, which does not read that cookie; slice 2 moves it to the media
  credential, which is the class V191 already allows for GETs of an item's
  bytes.
- **FR-010** Embedded book fonts are dropped in slice 1, because the UI's CSP
  (`font-src 'self'`) blocks `blob:` fonts. "The book's own font" falls back to
  the book's declared generic family. Allowing `font-src blob:` is decided
  separately.

- **FR-011** `setReadPosition` refuses (with `Error`, storing nothing) an id
  that is not a book, a book whose format is `unreadable`, a locator over 256
  bytes, and a fraction that is not finite or is outside 0 to 1. (V355)
- **FR-012** A book with a stored fraction above 0 that is not played comes
  back from `Library.resume` when its kinds include `book` and in a new
  `Home.reading` list, so the home page can draw "Continue reading". Today
  `resume` keys on `last_played_position_ticks > 0`, which a native read never
  sets, so this changes the query in both stores.

## Success criteria

- **SC-001** All 56 `epub` books on the live library open in pharos-ui and
  show text on page 1 (checked with a one-off sweep script, not in CI).
- **SC-002** A 1 MB EPUB opens to its first page in under 1 s on ali-desktop,
  and a page turn takes under one frame (16 ms) once a section is laid out.
- **SC-003** The `quire` crates add under 400 KB (gzip) to the UI's wasm.
- **SC-004** Reopening a book on a second browser at another size shows the
  same first sentence (Playwright, US3 acceptance 1).

## Out of scope

- DRM removal (005 §Scope stands).
- Annotations, highlights, bookmarks beyond the one position, dictionary,
  text-to-speech. They fit on the locator later.
- Moving 005's MOBI to EPUB converter (`pharos-scanner/src/book/kindle.rs`)
  into `quire`. It would make `quire` read Kindle files directly and is a
  natural follow-up, not needed for this feature.
- Audiobooks (already played as audio).

## Invariants reserved

- **V350** `quire*` crates have no pharos dependency and build for wasm32 and
  the host.
- **V351** No script, event handler or remote fetch from book content runs or
  is requested.
- **V352** A locator round-trips: saved at one size and font, restored at
  another, it shows the same first character.
- **V353** An `unreadable` book has no Read action and its page says why.
- **V354** Saving a native read position leaves `PlaybackPositionTicks`
  untouched, and the reverse.
- **V355** `setReadPosition` stores nothing for a non-book, an unreadable
  book, an oversized locator or an out-of-range fraction.

## Bugs reserved

- **B410** Books cannot be read in pharos-ui (this feature).
