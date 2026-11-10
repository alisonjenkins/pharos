# quire-web

Draw a [`quire`](../quire) book in a browser.

`View::mount(host, book, settings)` attaches an open shadow root to `host`,
an element the caller sizes, so the book's styles and the page's never mix.
`View::open` builds a section node by node from `quire::Section::root` and
flows it into CSS columns, one column per page; the browser does the layout.
A fixed-layout section (`quire::Section::page`, from an EPUB marked
`pre-paginated`) is drawn whole instead: one page of its own size, scaled to
fit the host and centred, with the book's sizes and colours untouched by the
reader settings. Images are decoded and the book's own fonts loaded before
pages are counted. The fonts go into `document.fonts` (a shadow root ignores
`@font-face`) and are removed, and the `blob:` URLs for the book's files
revoked, when the view is dropped.

```rust,ignore
let view = quire_web::View::mount(&host, Rc::new(book), Settings::default())?;
view.on_action(|a| { /* Next, Prev, Jump(target), Relayout: caller decides */ });
view.go_to("epub1:3:1200".parse()?).await?;
view.next().await?;
let place = view.locator(); // first character on the page shown
```

- `next` / `prev` turn pages and cross into the next or previous linear
  section; `go` follows a link or table-of-contents target.
- `set_settings` changes font size, typeface (`Book`, `Serif`, `Sans`), line
  height, margins and colours (`Theme::paper`, `sepia`, `night`, or any CSS
  colours, including `var(--x)` from the host page) and keeps the reader's
  place.
- Clicks on the outer thirds, horizontal swipes, link clicks and resizes come
  back as `Action`s; the view binds no keys and needs no UI framework.

A `Locator` names the first character drawn on the page, found by measuring
character boxes, so a page that starts mid-paragraph is located mid-paragraph.

`just quire-web-check` builds `examples/harness` for wasm32 and checks in
headless Chromium that a place survives a change of font and window size, and
that a fixed-layout page is scaled, centred and drawn with its own fonts.
`mount_book` in the harness opens any EPUB's bytes for a look by hand.

## Licence

AGPL-3.0-or-later, as the repository it was written in.
