// Loads the PDF renderer (quire-pdf, built to a wasm-bindgen `--target web`
// module beside this file) and publishes its document class on `window`.
// The reader adds this script only when a PDF is opened, so a page that
// reads no PDF never fetches the renderer (spec 023 slice 5, ADR-0047).
// A static module rather than an inline `import()`: the UI's CSP allows
// scripts from 'self' and no eval.
//
// The script's `load` event can fire before a top-level await finishes, so
// the start-up is a promise the reader awaits, set before anything is awaited.
import init, { QuirePdfDocument } from "./quire_pdf.js";

window.quirePdfReady = init().then(() => {
  window.QuirePdfDocument = QuirePdfDocument;
});
