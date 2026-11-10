# Implementation plan: read books in pharos-ui

**Spec**: [spec.md](spec.md)
**Created**: 2026-11-04T00:55:00Z

## Shape

```text
crates/quire            (no web-sys, no pharos)
  Publication ── open(bytes) ─▶ Reflowable { meta, toc, spine }   EPUB
                              └▶ FixedLayout { meta, toc, pages }  PDF, CBZ (later)
  Section { xhtml (sanitised), resources: id ─▶ bytes + media type }
  Locator  "epub1:<spine>:<char offset>"  + fraction        (FR-005)
  ports:   Archive (zip), Xml (quick-xml), Sanitiser (HTML)  (FR-002)

crates/quire-web        (web-sys, wasm-bindgen, no framework, no pharos)
  View::mount(host: &Element, pub: Publication, settings) -> View
    shadow root ─▶ section XHTML, resources as blob: URLs
    CSS columns, one column = one page; turn = translateX
    next() / prev() / go(Locator) / locator() / set(Settings)
    on_position(cb), on_error(cb)

crates/pharos-ui        (the only pharos-aware layer)
  /read/{id} route  ─▶ GET /Items/{id}/Download (Authorization header)
  ReaderView (Dioxus): toolbar, toc, settings panel, page-turn zones
  position ─▶ UserData.setReadPosition (debounced 5 s, and on leave)
  settings ─▶ localStorage `pharos.reader.settings`
```

## Rendering

The browser lays text out; `quire-web` does not. A section is mounted in a
shadow root whose host has a fixed height and `column-width` equal to the page
width, so the browser flows it into columns that are pages. A page turn
translates the column strip. Page count comes from `scrollWidth`. Fonts,
sizes and the page theme are CSS custom properties on the host, with the
theme's text and background set `!important` over the book's own colours
(US2 acceptance 2).

Offsets are UTF-16 code units into the concatenated text of the sanitised
section, the unit DOM `Range` offsets use; `quire` exposes the section text
and converts to and from `char` positions, so the Rust and DOM sides agree on
text outside the Basic Multilingual Plane. They do not depend on layout
(V352).

A locator is restored by walking the section's text nodes to the stored
offset, making a collapsed `Range` there and showing the page whose column
holds its rect. The current locator is the first character drawn on the
visible page: `caretPositionFromPoint` (or `caretRangeFromPoint`) at the
page's top-left content corner, falling back to a binary search over
`Range.getClientRects()` within the text node that spans the page's left
edge. A paragraph crossing a page break is therefore located mid-node, not at
its start.

Images and inline styles reach the page as `blob:` and inline `<style>`,
which the UI CSP already allows (`img-src 'self' data: blob:`,
`style-src 'self' 'unsafe-inline'`). Fonts do not (FR-010).

## Native API (one PR, one `schemaVersion` bump)

Ordinals are fixed when the PR is written, after rebasing over the schema PRs
in flight (#617 group, #620 3D, #621 lyrics, #625 tracks, #629 artist and
album, #633 cast, which takes `ItemDetail @8`). No ordinal below is final.
Shape:

```capnp
enum BookFormat { epub @0; pdf @1; comic @2; unreadable @3; }
struct BookInfo { format @0 :BookFormat; author @1 :Text; pageCount @2 :UInt32; }
struct ItemDetail { ...; book @N :BookInfo; }          # set for kind == book
struct UserState  { ...; readLocator @N :Text; readFraction @M :Float32; }
interface UserData {
  ...
  setReadPosition @N (ctx :CallContext, id :ItemId, locator :Text,
                      fraction :Float32) -> (result :UserStateResult);
}
```

Storage: two nullable columns on `user_data` (`read_locator TEXT`,
`read_fraction REAL`), as a migration in both `migrations/sqlite` and
`migrations/postgres` (next free number in each, 0062 at the time of writing),
with the matching column lists in both stores. `setReadPosition` with a
fraction of 0.99 or more also sets `played`. The locator is opaque to the
server; FR-011 lists what it checks.

`resume` (both stores) also lists a book whose `read_fraction` is above 0 and
that is not played, ordered by the same last-activity time, and
`Library.home` gains a `reading` list for them (FR-012). `/Items/{id}/Download`
moves from `AuthUser` to the media-credential extractor (`MediaUser`), so the
`pharos_media` cookie authenticates it (FR-009): a GET that only reads an
item's bytes, the same class as images and streams under V191.

## Slices (one PR each, each through /review and the merge queue)

| # | PR | Lands | Guards |
|---|---|---|---|
| 0 | this spec + ADR-0047 | docs | — |
| 1 | `quire`: EPUB open (container, OPF, spine, nav or NCX), sanitiser, link rewriting, resources, locator; `quire` added to hakari `final-excludes` | crate + fixtures built in tests, `.config/hakari.toml` | V350, V351, V352 (model half) |
| 2 | native API: `BookInfo`, `setReadPosition`, storage, reading row, Download on the media cookie | schema bump, sqlite + postgres migrations, it_rpc | V353 (server half), V354, V355 |
| 3 | `quire-web`: mount, paginate, turn, settings, locator from and to the DOM; `quire-web` added to hakari `final-excludes` | crate + wasm-bindgen tests in headless Chromium | V352 (DOM half) |
| 4 | pharos-ui reader: route, Read button, toc, settings panel, position sync, DRM and error states | UI + Playwright | V353, SC-004, B410 |
| 5 | PDF: `hayro` behind `quire`'s `FixedLayout` port, as `quire-pdf` and its own lazily loaded wasm module; canvas reader in pharos-ui | ADR-0047 amended | US5, V356–V358, B411 |
| 6 | CBZ, when the library has a comic | | US6 |

Slices 1 and 2 do not depend on each other and can be built in parallel.

## Status and changes, 2026-11-09

Slices 1 (`quire`, #690) and 3 (`quire-web`, #738) are on main;
`just quire-web-check` passes on main at 0ea9de98e. Slices 2 and 4 are being
built now ("can you kick something off to make an Ebook reader for Pharos
please? We have the books but cannot read yet", Alison, 2026-11-09).

The library re-measured on hss1 by extension: 39 `.epub` (836 MB, median
6.6 MB, 90th percentile 35 MB, largest 214 MB), 75 Kindle files (17 convert,
58 DRM), 28 `.pdf` (median 13 MB, largest 83 MB), no comics. EPUB first
stands.

The quire-web harness is 282 KB gzip on its own (std and web-sys included),
under SC-003's 400 KB before any sharing with pharos-ui.

Changes to slice 2 against the shape above:

- **Storage is its own table, not two `user_data` columns.**
  `UserItemData` is `Copy` and built in about 40 files; a `String` locator
  would end that for one feature. A `read_positions` table (`user_id`,
  `item_id`, `locator`, `fraction`, `updated_at`, primary key on the pair,
  cascading with the user and the item) behind a `ReadPositionStore` port
  keeps the two positions apart by construction (V354). Migration 0076 in
  both stores.
- **Ordinals, at main today (schema 31):** `schemaVersion` 32,
  `ItemDetail.book @14`, `UserState.readLocator @4`, `readFraction @5`,
  `UserData.setReadPosition @2`, `Home.reading @4`. Fixed again at queue time
  if another schema PR lands first.
- **Observability.** `pharos_read_position_total{outcome}` counts
  `setReadPosition` calls by outcome (`saved`, `finished`, `not_book`,
  `unreadable`, `bad_locator`, `bad_fraction`, `hidden`), and each refusal
  logs the item id and the reason. The UI logs open time, section parse
  failures and the book's size with the item id and format.

Weak devices (TV, Quest, phones): the whole book is fetched, and a book over
64 MiB is refused there with the download offered; desktop has no cap
("1 please", Alison, 2026-11-09, choosing this over loading chapter by
chapter from the server). Wasm memory never shrinks once grown, and the
library's largest EPUB is 214 MB.

## Slice 5: PDF (built 2026-11-10)

```text
quire       FixedLayout { page_count, page_size, render(i, height_px) -> Raster }
            PageLocator "pdf1:<page>" (from 1), page_progress = page / count
quire-pdf   hayro adapter + wasm-bindgen QuirePdfDocument
            (no pharos dependency, hakari final-exclude, own README)
bundle      assets/pdf/{loader.js, quire_pdf.js, quire_pdf_bg.wasm}
            built in flake.nix's pharosUiBundle from the vendored Cargo.lock
pharos-ui   pdf_web.rs  loads the module on demand, canvas + one spare
            reader.rs   layout caps, turns, taps, swipes, pdf1 saves (host tests)
```

Decisions and measured sizes are in ADR-0047. The module is 4.99 MB raw and
1.86 MB gzip; the main wasm grew 23 KB raw (7 KB gzip) for the reader code. A page turn draws the next page
ahead (one spare canvas); the position saves through the same debounce and
page-hide flush as an EPUB. Left for later: offline reading (nothing stores
the module or book yet), PDF outlines as a contents panel, text selection and
search.

## Testing

- `quire`: unit tests on EPUBs built in the test from strings (no binary
  fixtures in git), covering EPUB 2 (NCX) and EPUB 3 (nav), a malicious
  section (script, `onload`, `javascript:` link, remote image, form, iframe),
  a missing resource, and locator round trips. A `proptest` that sanitised
  output never contains a script element or an `on*` attribute.
- `quire-web`: `wasm-bindgen-test` in headless Chromium: paginate a section at
  two widths and two font sizes and check the locator lands on the same first
  character.
- pharos-ui: Playwright on `compat-dioxus-full` with a seeded EPUB: open,
  turn, change font size, reload in a second context at another viewport,
  check the same passage. A seeded `.azw3` with the DRM flag set shows the
  locked message.
- CI: `cargo build -p quire -p quire-web --target wasm32-unknown-unknown`,
  a check that `cargo tree -p quire -p quire-web` lists neither `pharos-*`
  nor `workspace-hack`, and the existing `hakari-check` (V350).

## Risks

- **Sanitiser size.** `ammonia` brings html5ever; if it breaks SC-003, the
  `Sanitiser` port takes an allow-list walker over quick-xml events instead,
  which EPUB's XHTML (well-formed XML) makes possible.
- **CSS columns edge cases.** Images taller than a page and wide tables. The
  host caps media at page size (`max-height`, `max-width`), as epub.js does.
- **Schema contention.** Six schema PRs are open (listed above); slice 2
  rebases after them, takes the next `schemaVersion` and the next free
  ordinals, and copies the schema to `specs/017-native-api/contracts/`.
