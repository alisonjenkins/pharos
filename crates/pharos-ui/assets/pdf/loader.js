// Loads the PDF renderer (quire-pdf, built to a wasm-bindgen `--target web`
// module beside this file) and publishes its document class on `window`.
// The reader adds this script only when a PDF is opened, so a page that
// reads no PDF never fetches the renderer (spec 023 slice 5, ADR-0047).
// A static module script that imports the renderer, rather than an inline
// `import()` in the reader: the UI's CSP allows scripts from 'self' and no
// eval.
//
// The reader loads this as `loader.js?n=<attempt>`. The browser keeps a
// module's result, failure included, for the life of the page, so each
// attempt asks for its own copy of the renderer (`quire_pdf.js?n=<attempt>`):
// a retry after a failed load, or after the module trapped, gets a fresh
// instance. The script's `load` event can fire before the import finishes,
// so start-up is a promise the reader awaits, set before anything is awaited.
const attempt = new URL(import.meta.url).searchParams.get("n") ?? "0";

window.quirePdfReady = import(`./quire_pdf.js?n=${encodeURIComponent(attempt)}`).then(
  async (renderer) => {
    await renderer.default();
    window.QuirePdfDocument = renderer.QuirePdfDocument;
  },
);
