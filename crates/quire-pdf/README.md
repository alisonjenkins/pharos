# quire-pdf

Draw PDF pages from bytes, behind `quire`'s `FixedLayout` port: page count,
page size, and a page drawn at a pixel height as RGBA.

The engine is [`hayro`](https://crates.io/crates/hayro) (MIT or Apache-2.0), a
pure-Rust renderer, so the same code runs on the host, where the tests build
their PDFs from strings, and in a browser. `quire` does not depend on it: this
crate is the adapter, and another engine could replace it without a reader
noticing.

## Browser module

On `wasm32-unknown-unknown` the crate exports `QuirePdfDocument`
(`new(bytes)`, `pageCount()`, `pageAspect(i)`, `render(i, heightPx)` to an
`ImageData`). It is built as its own module with `wasm-bindgen --target web`
and loaded by a page only when a PDF is opened, so the renderer's size
(about 1.7 MB gzip) never reaches a page that reads no PDF.

## Limits

hayro has no decryption, so encrypted PDFs are refused with an error. A page
is drawn at most 16384 px on a side. The whole file is held in memory.

## Licence

AGPL-3.0-or-later, as the repository it was written in. `hayro` and its
dependencies are permissively licensed and keep their own terms.
