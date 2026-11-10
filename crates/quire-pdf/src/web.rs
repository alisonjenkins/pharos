//! The wasm-bindgen module a page loads on demand. Its names are what the
//! reader's loader script publishes on `window`; keep them in step with
//! `crates/pharos-ui/assets/vendor/quire-pdf/loader.js`.

use quire::FixedLayout;
use wasm_bindgen::prelude::*;
use wasm_bindgen::Clamped;
use web_sys::ImageData;

use crate::PdfBook;

/// An open PDF in the browser.
#[wasm_bindgen(js_name = QuirePdfDocument)]
pub struct Document {
    book: PdfBook,
}

#[wasm_bindgen(js_class = QuirePdfDocument)]
impl Document {
    /// Opens a PDF from its bytes (copied into this module's memory).
    #[wasm_bindgen(constructor)]
    pub fn new(bytes: Vec<u8>) -> Result<Document, JsError> {
        let book = PdfBook::open(bytes).map_err(|e| JsError::new(&e.to_string()))?;
        Ok(Self { book })
    }

    #[wasm_bindgen(js_name = pageCount)]
    pub fn page_count(&self) -> u32 {
        u32::try_from(self.book.page_count()).unwrap_or(u32::MAX)
    }

    /// The page's width over its height, as drawn (rotation applied).
    #[wasm_bindgen(js_name = pageAspect)]
    pub fn page_aspect(&self, index: u32) -> Result<f64, JsError> {
        let size = self
            .book
            .page_size(index as usize)
            .map_err(|e| JsError::new(&e.to_string()))?;
        Ok(f64::from(size.width) / f64::from(size.height.max(1.0)))
    }

    /// Page `index` (from 0) drawn `height_px` tall, ready for
    /// `putImageData`.
    pub fn render(&self, index: u32, height_px: u32) -> Result<ImageData, JsError> {
        let raster = self
            .book
            .render(index as usize, height_px)
            .map_err(|e| JsError::new(&e.to_string()))?;
        ImageData::new_with_u8_clamped_array_and_sh(
            Clamped(&raster.rgba),
            raster.width,
            raster.height,
        )
        .map_err(|_| JsError::new("the page image could not be made"))
    }
}
