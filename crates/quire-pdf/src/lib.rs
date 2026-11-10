//! Draw PDF pages from their bytes, behind [`quire::FixedLayout`].
//!
//! The engine is `hayro`, a pure-Rust renderer, so the same code draws on the
//! host (for tests) and in a browser. On `wasm32` the crate also exports a
//! small wasm-bindgen API (`QuirePdfDocument`) for a module that a page loads
//! only when a PDF is opened.
//!
//! A panic inside the engine is caught and reported as an error on the host
//! only. On `wasm32` panics abort, which traps the module and leaves its
//! instance unusable; the page that loaded it must discard the instance and
//! load a fresh one (the module logs the panic message to the console first).

#![forbid(unsafe_code)]

use std::panic::{catch_unwind, AssertUnwindSafe};

use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{render, PixmapSettings, RenderCache, RenderSettings};
use quire::{FixedLayout, FixedLayoutError, PageSize, Raster};

#[cfg(target_arch = "wasm32")]
mod web;

/// The longest side a page is drawn at, in pixels. The engine's canvas is
/// 16-bit, and a larger one would be wrapped, not drawn.
pub const MAX_SIDE_PX: u32 = 16_384;

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("not a readable PDF: {0}")]
    Parse(String),
    #[error("the PDF has no pages")]
    NoPages,
    #[error("the PDF engine failed while opening it")]
    Engine,
}

/// An open PDF.
pub struct PdfBook {
    pdf: Pdf,
}

impl PdfBook {
    /// Opens a PDF held in memory. Encrypted files are refused (the engine
    /// has no decryption), as are files without pages.
    pub fn open(bytes: Vec<u8>) -> Result<Self, OpenError> {
        let pdf = catch_unwind(AssertUnwindSafe(|| Pdf::new(bytes)))
            .map_err(|_| OpenError::Engine)?
            .map_err(|e| OpenError::Parse(format!("{e:?}")))?;
        if pdf.pages().is_empty() {
            return Err(OpenError::NoPages);
        }
        Ok(Self { pdf })
    }

    fn page(&self, index: usize) -> Result<&hayro::hayro_syntax::page::Page<'_>, FixedLayoutError> {
        let pages = self.pdf.pages();
        pages.get(index).ok_or(FixedLayoutError::OutOfRange {
            index,
            len: pages.len(),
        })
    }
}

impl FixedLayout for PdfBook {
    fn page_count(&self) -> usize {
        self.pdf.pages().len()
    }

    fn page_size(&self, index: usize) -> Result<PageSize, FixedLayoutError> {
        let (width, height) = self.page(index)?.render_dimensions();
        Ok(PageSize { width, height })
    }

    fn render(&self, index: usize, height_px: u32) -> Result<Raster, FixedLayoutError> {
        let page = self.page(index)?;
        let (width, height) = page.render_dimensions();
        let scale = scale_for(width, height, height_px);
        let drawn = catch_unwind(AssertUnwindSafe(|| {
            render(
                page,
                &RenderCache::new(),
                &InterpreterSettings::default(),
                &RenderSettings::default(),
                &PixmapSettings {
                    x_scale: scale,
                    y_scale: scale,
                    bg_color: WHITE,
                },
            )
        }))
        .map_err(|_| FixedLayoutError::Render {
            index,
            reason: "the PDF engine failed".to_owned(),
        })?;
        Ok(Raster {
            width: u32::from(drawn.width()),
            height: u32::from(drawn.height()),
            rgba: drawn.data_as_u8_slice().to_vec(),
        })
    }
}

/// The scale that draws a page `height_px` tall, held so neither side passes
/// [`MAX_SIDE_PX`] and a degenerate page size cannot make it 0 or infinite.
fn scale_for(width: f32, height: f32, height_px: u32) -> f32 {
    let wanted = height_px.clamp(1, MAX_SIDE_PX) as f32;
    let (width, height) = (width.max(1.0), height.max(1.0));
    let by_height = wanted / height;
    let by_width = MAX_SIDE_PX as f32 / width;
    by_height.min(by_width)
}

#[cfg(test)]
mod tests;
