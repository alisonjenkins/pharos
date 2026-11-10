# ADR-0119: A book's own fonts load from the book, through the document's font set

- **Status:** Accepted
- **Date:** 2026-11-10T23:04:59Z
- **Deciders:** Alison (asked for the badly drawn ebook to be fixed), Claude

## Context

`quire` treats book content as untrusted (V351) and dropped every `@font-face`
along with `@import`, so nothing in a book could fetch from outside it. Many
books ship their own fonts, and a fixed-layout book made by a PDF-to-EPUB
converter places every line of text by the point in a subset font that came
with it; drawn in a fallback face, the text no longer matches the page it sits
on (B1320). Two facts bound the fix:

- `quire-web` draws a book inside a shadow root, and Chromium ignores
  `@font-face` inside a shadow root (checked 2026-11-10 with a font from a
  real book: the rule had no effect, the same font added through
  `document.fonts` did).
- Font files are parsed by the browser's font sanitiser (OTS in Chromium and
  Firefox), the same exposure a book's images already have.

## Decision

We will load a book's fonts, but only from files inside the book:

- `quire` parses each `@font-face` into a `FontFace` (family; the archive
  paths of its `url()` sources, in order; `font-weight`, `font-style`,
  `font-stretch` and `unicode-range` when they hold only the characters such
  values use). A rule with no source inside the book is dropped, as before.
  `local()` and remote sources are ignored. The rule never appears in the
  stylesheet text `Css::render` returns.
- `quire-web` creates each face with the `FontFace` constructor from
  `blob:` URLs of the book's own bytes, adds it to `document.fonts` once per
  view, waits for `document.fonts.ready` before measuring pages (fonts move
  text, like images), and deletes the faces when the view is removed.

## Consequences

- Books look as their designers set them, and fixed-layout books line up.
- While a book is open its families are in the page's font set, so a book
  that names a family the app's own UI uses could change the UI's text until
  the reader closes. Accepted: Pharos's UI names system families, and the
  faces go when the view does.
- Each loaded font is held as a `blob:` URL for the life of the view; only
  faces the page uses are fetched and decoded (one test book registered 59 and
  loaded 7 for a page).
- The reader's Serif and Sans settings still override the book's fonts
  (`!important`), so a reader who dislikes a book's face can still change it.

## Alternatives considered

- **Keep `@font-face` in the shadow root's stylesheet.** Ignored by Chromium,
  so it would not work where most readers are.
- **Rename each family per view to avoid clashes.** Means rewriting every
  `font-family` in the book's CSS and inline styles; more parsing surface for
  a clash that has not happened.
- **Keep dropping fonts.** Leaves fixed-layout books unreadable in places.

## References

B1320, V1291, V351; `crates/quire/src/css.rs`, `crates/quire-web/src/view.rs`.
