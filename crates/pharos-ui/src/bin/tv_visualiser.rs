//! The music visualiser, for the Android TV app's page (B1032, V992).
//!
//! The TV app's Rust runs natively and its page in the system WebView, where
//! the visualiser's WebGL2 surface and Web Audio analyser live. This crate's
//! own visualiser loop (`native::listen::run_page_visualiser`), built to wasm
//! with `--features web`, runs there instead: `scripts/tv-visualiser.sh`
//! builds it and the APK carries it (`assets/generated/`).

use futures_util::future::{abortable, AbortHandle};
use wasm_bindgen::prelude::*;

/// A running visualiser; `stop` ends it.
#[wasm_bindgen]
pub struct PageVisualiser {
    abort: AbortHandle,
}

#[wasm_bindgen]
impl PageVisualiser {
    pub fn stop(&self) {
        self.abort.abort();
    }
}

/// Draw `style` on the page's visualiser canvas until stopped. A style that
/// cannot draw calls `on_fallback` with the reason's label; a stopped one
/// calls nothing.
#[wasm_bindgen]
pub fn start(style: String, on_fallback: js_sys::Function) -> PageVisualiser {
    let (run, abort) =
        abortable(async move { pharos_ui::native::listen::run_page_visualiser(&style).await });
    wasm_bindgen_futures::spawn_local(async move {
        if let Ok(Err(fallback)) = run.await {
            let label = JsValue::from_str(fallback.label());
            if let Err(e) = on_fallback.call1(&JsValue::NULL, &label) {
                web_sys::console::warn_1(&e);
            }
        }
    });
    PageVisualiser { abort }
}

fn main() {}
