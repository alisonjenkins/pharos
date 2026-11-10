//! A PDF on a canvas (spec 023 US5): loads the separate `quire-pdf` wasm
//! module the first time a PDF is opened, draws one page at a time at the
//! window's size and pixel ratio, and keeps one spare page drawn ahead.
//! Rules (layout, caps, turns) are in `reader.rs`.
//!
//! Cost model, for TVs and headsets: two canvases at most (the one shown and
//! one off-screen for the next page), no timer while reading, and the module
//! is never fetched by a page that opens no PDF (ADR-0047).

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use pharos_client::api::ItemRef;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen::{JsCast as _, JsValue};

use super::platform;
use super::reader::{pdf_layout, prerender_index, turn_to, PdfFit, PdfLayout};

/// The loader script beside the wasm-bindgen module (`flake.nix` installs
/// both under `assets/pdf`). It defines `window.QuirePdfDocument`.
const LOADER_SRC: &str = "/ui/assets/pdf/loader.js";
const LOADED_GLOBAL: &str = "QuirePdfDocument";
/// A promise the loader sets as it starts; it settles when the module has.
const STARTED_PROMISE: &str = "quirePdfReady";

/// A resize redraws the page only once the window has been still this long.
const RESIZE_SETTLE_MS: u32 = 150;

/// The spare page is drawn this long after a turn, so the turn itself paints
/// first.
const PRERENDER_DELAY_MS: u32 = 50;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_name = QuirePdfDocument)]
    type PdfDoc;
    #[wasm_bindgen(constructor, js_class = "QuirePdfDocument", catch)]
    fn new(bytes: &js_sys::Uint8Array) -> Result<PdfDoc, JsValue>;
    #[wasm_bindgen(method, js_class = "QuirePdfDocument", js_name = pageCount)]
    fn page_count(this: &PdfDoc) -> u32;
    #[wasm_bindgen(method, js_class = "QuirePdfDocument", js_name = pageAspect, catch)]
    fn page_aspect(this: &PdfDoc, index: u32) -> Result<f64, JsValue>;
    #[wasm_bindgen(method, js_class = "QuirePdfDocument", catch)]
    fn render(this: &PdfDoc, index: u32, height_px: u32) -> Result<web_sys::ImageData, JsValue>;
    #[wasm_bindgen(method, js_class = "QuirePdfDocument")]
    fn free(this: &PdfDoc);
}

fn describe(e: &JsValue) -> String {
    e.as_string()
        .or_else(|| {
            e.dyn_ref::<js_sys::Error>()
                .map(|e| String::from(e.message()))
        })
        .unwrap_or_else(|| format!("{e:?}"))
}

/// Adds the loader as a module script once and waits for it. The loader
/// defines [`LOADED_GLOBAL`] only when the module started.
async fn load_module() -> Result<(), String> {
    let window = web_sys::window().ok_or("no window")?;
    let ready =
        || js_sys::Reflect::has(&window, &JsValue::from_str(LOADED_GLOBAL)).unwrap_or(false);
    if ready() {
        return Ok(());
    }
    let document = window.document().ok_or("no document")?;
    let script = document
        .create_element("script")
        .map_err(|e| describe(&e))?
        .dyn_into::<web_sys::HtmlScriptElement>()
        .map_err(|_| "not a script element")?;
    script.set_type("module");
    script.set_src(LOADER_SRC);
    let finished = js_sys::Promise::new(&mut |resolve, reject| {
        script.set_onload(Some(&resolve));
        script.set_onerror(Some(&reject));
    });
    document
        .head()
        .ok_or("no <head>")?
        .append_child(&script)
        .map_err(|e| describe(&e))?;
    wasm_bindgen_futures::JsFuture::from(finished)
        .await
        .map_err(|_| "The PDF reader could not be loaded.".to_owned())?;
    // The script's `load` can come before its start-up has finished.
    let started = js_sys::Reflect::get(&window, &JsValue::from_str(STARTED_PROMISE))
        .ok()
        .and_then(|p| p.dyn_into::<js_sys::Promise>().ok())
        .ok_or("The PDF reader did not start.")?;
    wasm_bindgen_futures::JsFuture::from(started)
        .await
        .map_err(|e| format!("The PDF reader could not start: {}", describe(&e)))?;
    if !ready() {
        return Err("The PDF reader did not start.".to_owned());
    }
    Ok(())
}

/// The page drawn off-screen, and what it was drawn for.
struct Spare {
    canvas: web_sys::HtmlCanvasElement,
    drawn: Option<(usize, PdfLayout)>,
}

/// The module's document, freed from its memory when dropped, so an early
/// error return cannot leak it.
struct Doc(PdfDoc);

impl std::ops::Deref for Doc {
    type Target = PdfDoc;

    fn deref(&self) -> &PdfDoc {
        &self.0
    }
}

impl Drop for Doc {
    fn drop(&mut self) {
        self.0.free();
    }
}

/// An open PDF on screen. Dropping it frees the document.
pub struct PdfView {
    doc: Doc,
    id: ItemRef,
    count: usize,
    host: web_sys::HtmlElement,
    shown: RefCell<web_sys::HtmlCanvasElement>,
    spare: RefCell<Spare>,
    index: Cell<usize>,
    fit: Cell<PdfFit>,
    /// Bumped by every resize and turn, so a delayed task can tell it is stale.
    generation: Cell<u64>,
    resize: RefCell<Option<Closure<dyn FnMut()>>>,
}

fn new_canvas(document: &web_sys::Document) -> Result<web_sys::HtmlCanvasElement, String> {
    document
        .create_element("canvas")
        .map_err(|e| describe(&e))?
        .dyn_into::<web_sys::HtmlCanvasElement>()
        .map_err(|_| "not a canvas".to_owned())
}

impl PdfView {
    /// Loads the module if needed, opens the PDF from `bytes` and shows page
    /// `index` (held inside the book).
    pub async fn open(
        host: web_sys::HtmlElement,
        bytes: &js_sys::Uint8Array,
        index: usize,
        fit: PdfFit,
        id: ItemRef,
    ) -> Result<Rc<Self>, String> {
        let began = platform::now_ms();
        load_module().await?;
        let module_ms = platform::now_ms() - began;
        let doc =
            Doc(PdfDoc::new(bytes)
                .map_err(|e| format!("The PDF cannot be read: {}", describe(&e)))?);
        let count = doc.page_count() as usize;
        if count == 0 {
            return Err("The PDF has no pages.".to_owned());
        }
        let document = host.owner_document().ok_or("The reader has no document.")?;
        let first = new_canvas(&document)?;
        let spare = new_canvas(&document)?;
        host.set_text_content(None);
        host.append_child(&first).map_err(|e| describe(&e))?;
        let view = Rc::new(Self {
            doc,
            id,
            count,
            host,
            shown: RefCell::new(first),
            spare: RefCell::new(Spare {
                canvas: spare,
                drawn: None,
            }),
            index: Cell::new(index.min(count.saturating_sub(1))),
            fit: Cell::new(fit),
            generation: Cell::new(0),
            resize: RefCell::new(None),
        });
        let canvas = view.shown.borrow().clone();
        view.draw(&canvas, view.index.get())?;
        view.listen_for_resize();
        view.prerender_later();
        platform::info(&format!(
            "pharos: reader pdf opened item={id} pages={count} module_ms={module_ms:.0} ms={:.0}",
            platform::now_ms() - began
        ));
        Ok(view)
    }

    pub fn index(&self) -> usize {
        self.index.get()
    }

    pub fn count(&self) -> usize {
        self.count
    }

    pub fn set_fit(self: &Rc<Self>, fit: PdfFit) {
        if self.fit.replace(fit) != fit {
            self.relayout();
        }
    }

    /// Shows the next or previous page. `Ok(false)` at either end of the
    /// book, `Ok(true)` once the page is shown.
    pub fn turn(self: &Rc<Self>, forward: bool) -> Result<bool, String> {
        let Some(to) = turn_to(self.index.get(), self.count, forward) else {
            return Ok(false);
        };
        self.generation.set(self.generation.get().wrapping_add(1));
        let layout = self.layout_for(to)?;
        let ready = {
            let spare = self.spare.borrow();
            spare.drawn == Some((to, layout))
        };
        if ready {
            let mut spare = self.spare.borrow_mut();
            let mut shown = self.shown.borrow_mut();
            self.host
                .replace_child(&spare.canvas, &shown)
                .map_err(|e| describe(&e))?;
            std::mem::swap(&mut spare.canvas, &mut *shown);
            spare.drawn = None;
        } else {
            let canvas = self.shown.borrow().clone();
            self.draw(&canvas, to)?;
            self.spare.borrow_mut().drawn = None;
        }
        self.index.set(to);
        self.host.set_scroll_top(0);
        self.prerender_later();
        Ok(true)
    }

    /// Draws the current page again, for a new window size or fit.
    pub fn relayout(self: &Rc<Self>) {
        let canvas = self.shown.borrow().clone();
        if let Err(e) = self.draw(&canvas, self.index.get()) {
            platform::warn(&format!(
                "pharos: reader pdf redraw failed item={} error={e}",
                self.id
            ));
        }
        self.spare.borrow_mut().drawn = None;
        self.prerender_later();
    }

    fn layout_for(&self, index: usize) -> Result<PdfLayout, String> {
        let aspect = self
            .doc
            .page_aspect(u32::try_from(index).unwrap_or(u32::MAX))
            .map_err(|e| describe(&e))?;
        let ratio = web_sys::window().map_or(1.0, |w| w.device_pixel_ratio());
        Ok(pdf_layout(
            f64::from(self.host.client_width()),
            f64::from(self.host.client_height()),
            aspect,
            self.fit.get(),
            ratio,
        ))
    }

    /// Draws page `index` on `canvas` and returns what it was drawn for.
    fn draw(&self, canvas: &web_sys::HtmlCanvasElement, index: usize) -> Result<PdfLayout, String> {
        let layout = self.layout_for(index)?;
        let began = platform::now_ms();
        let image = self
            .doc
            .render(
                u32::try_from(index).unwrap_or(u32::MAX),
                layout.render_height,
            )
            .map_err(|e| {
                format!(
                    "Page {} cannot be drawn: {}",
                    index.saturating_add(1),
                    describe(&e)
                )
            })?;
        canvas.set_width(image.width());
        canvas.set_height(image.height());
        let context = canvas
            .get_context("2d")
            .map_err(|e| describe(&e))?
            .and_then(|c| c.dyn_into::<web_sys::CanvasRenderingContext2d>().ok())
            .ok_or("The page canvas has no 2d context.")?;
        context
            .put_image_data(&image, 0.0, 0.0)
            .map_err(|e| describe(&e))?;
        let style = canvas.style();
        let _ = style.set_property("width", &format!("{}px", layout.css_width));
        let _ = style.set_property("height", &format!("{}px", layout.css_height));
        web_sys::console::debug_1(
            &format!(
                "pharos: reader pdf render item={} page={} px={}x{} ms={:.0}",
                self.id,
                index.saturating_add(1),
                image.width(),
                image.height(),
                platform::now_ms() - began
            )
            .into(),
        );
        Ok(layout)
    }

    /// Draws the next page into the spare canvas once the turn has painted,
    /// unless the reader has moved on.
    fn prerender_later(self: &Rc<Self>) {
        let Some(next) = prerender_index(self.index.get(), self.count) else {
            return;
        };
        let (weak, at) = (Rc::downgrade(self), self.generation.get());
        platform::spawn_detached(async move {
            platform::sleep_ms(PRERENDER_DELAY_MS).await;
            let Some(view) = weak.upgrade() else {
                return;
            };
            if view.generation.get() != at {
                return;
            }
            let canvas = view.spare.borrow().canvas.clone();
            match view.draw(&canvas, next) {
                Ok(layout) => view.spare.borrow_mut().drawn = Some((next, layout)),
                Err(e) => platform::warn(&format!(
                    "pharos: reader pdf prerender failed item={} error={e}",
                    view.id
                )),
            }
        });
    }

    fn listen_for_resize(self: &Rc<Self>) {
        let weak: Weak<Self> = Rc::downgrade(self);
        let on_resize = Closure::<dyn FnMut()>::new(move || {
            let Some(view) = weak.upgrade() else {
                return;
            };
            let at = view.generation.get().wrapping_add(1);
            view.generation.set(at);
            platform::spawn_detached(async move {
                platform::sleep_ms(RESIZE_SETTLE_MS).await;
                if view.generation.get() == at {
                    view.relayout();
                }
            });
        });
        if let Some(window) = web_sys::window() {
            let _ = window
                .add_event_listener_with_callback("resize", on_resize.as_ref().unchecked_ref());
        }
        *self.resize.borrow_mut() = Some(on_resize);
    }

    /// Releases the document and empties the host.
    pub fn unmount(&self) {
        if let (Some(window), Some(on_resize)) =
            (web_sys::window(), self.resize.borrow_mut().take())
        {
            let _ = window
                .remove_event_listener_with_callback("resize", on_resize.as_ref().unchecked_ref());
        }
        self.generation.set(self.generation.get().wrapping_add(1));
        self.host.set_text_content(None);
    }
}
