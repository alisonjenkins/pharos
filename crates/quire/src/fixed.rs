//! The port for books whose pages are fixed: a PDF, later a comic. A reader
//! asks for a page at a pixel height and gets pixels; the engine behind it
//! (`quire-pdf` for PDF) is an adapter, so it can change without touching a
//! reader (spec 023 FR-002).

/// A page's size in the book's own units (points for a PDF).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageSize {
    pub width: f32,
    pub height: f32,
}

/// A rendered page: `width` x `height` pixels of straight RGBA, row by row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Raster {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FixedLayoutError {
    #[error("no page {index}; the book has {len}")]
    OutOfRange { index: usize, len: usize },
    #[error("page {index} cannot be drawn: {reason}")]
    Render { index: usize, reason: String },
}

/// An open fixed-layout book. Pages are numbered from 0.
pub trait FixedLayout {
    fn page_count(&self) -> usize;

    fn page_size(&self, index: usize) -> Result<PageSize, FixedLayoutError>;

    /// The page drawn `height_px` pixels tall; the width follows the page's
    /// shape. A height of 0 is drawn as 1.
    fn render(&self, index: usize, height_px: u32) -> Result<Raster, FixedLayoutError>;
}

/// How far through the book page `index` (from 0) is, from 0 to 1: the page
/// number over the page count, so the last page reads 1.
pub fn page_progress(index: usize, page_count: usize) -> f64 {
    if page_count == 0 {
        return 0.0;
    }
    let page = index.saturating_add(1).min(page_count);
    (page as f64 / page_count as f64).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]
    use super::*;

    struct Blank(usize);

    impl FixedLayout for Blank {
        fn page_count(&self) -> usize {
            self.0
        }

        fn page_size(&self, index: usize) -> Result<PageSize, FixedLayoutError> {
            if index >= self.0 {
                return Err(FixedLayoutError::OutOfRange { index, len: self.0 });
            }
            Ok(PageSize {
                width: 2.0,
                height: 1.0,
            })
        }

        fn render(&self, index: usize, height_px: u32) -> Result<Raster, FixedLayoutError> {
            let size = self.page_size(index)?;
            let height = height_px.max(1);
            let width = (f64::from(height) * f64::from(size.width / size.height)).round() as u32;
            Ok(Raster {
                width,
                height,
                rgba: vec![255; (width as usize) * (height as usize) * 4],
            })
        }
    }

    #[test]
    fn the_port_is_usable_through_a_trait_object() {
        let book: Box<dyn FixedLayout> = Box::new(Blank(3));
        assert_eq!(book.page_count(), 3);
        let r = book.render(2, 10).unwrap();
        assert_eq!((r.width, r.height, r.rgba.len()), (20, 10, 800));
        assert_eq!(
            book.render(3, 10),
            Err(FixedLayoutError::OutOfRange { index: 3, len: 3 })
        );
    }

    #[test]
    fn progress_is_the_page_over_the_count() {
        assert!((page_progress(0, 4) - 0.25).abs() < 1e-9);
        assert!((page_progress(3, 4) - 1.0).abs() < 1e-9);
        assert!(
            (page_progress(99, 4) - 1.0).abs() < 1e-9,
            "past the end holds"
        );
        assert!(page_progress(0, 0).abs() < 1e-9, "no pages is 0");
    }
}
