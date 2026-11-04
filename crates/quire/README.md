# quire

Open EPUB 2 and 3 books from bytes and read them: metadata, table of
contents, reading order, each section as a sanitised element tree with its
stylesheets, and reading positions that do not depend on layout.

`quire` does no layout and touches no DOM, so it builds for
`wasm32-unknown-unknown` and for native targets alike and its tests run with
`cargo test`. A renderer builds pages from `Section::root`; `quire-web` is the
browser one.

## Untrusted books

Everything that would run or fetch is removed before a section is handed out:
scripts, `on*` attributes, forms, frames, objects, `<use>`,
`foreignObject`, `@import`, `@font-face`, and every URL that leaves the book.
A link to another section becomes `data-quire-link="<spine>#<id>"` for the
renderer to turn into a jump; a link to any other file in the book is inert;
an `http(s)` link opens in a new tab with `rel="noopener noreferrer"`.
Images and `url()`s inside the book stay as archive paths that the caller
resolves when it draws (to `blob:` URLs in a browser).

## Positions

A `Locator` is `epub1:<spine index>:<offset>`, the offset counted in UTF-16
code units of `Section::text()`, the unit DOM ranges use. It names the same
character at any window size, font or page size. `Publication::progress`
turns a place into a whole-book fraction, weighing sections by size.

## Use

```rust,no_run
let book = quire::Publication::open_epub(std::fs::read("book.epub")?)?;
for entry in book.toc() {
    println!("{}", entry.title);
}
let section = book.section(0)?;
let html = section.to_html(&mut |path| Some(format!("/book/{path}")));
# Ok::<(), Box<dyn std::error::Error>>(())
```

`cargo run --release --example sweep -- *.epub` opens every section of each
book given and reports any that fail.

## Dependencies

`quick-xml` (behind `xml.rs`), `zip` (behind the `Archive` trait, so another
container can be supplied with `Publication::open_epub_from`) and
`thiserror`. No `unsafe`.

## Licence

AGPL-3.0-or-later, as the repository it was written in.
