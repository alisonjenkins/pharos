//! The arithmetic of pages and positions, kept free of the DOM so it is
//! tested natively.

/// How a section's column strip is cut into pages.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Columns {
    /// Width of one page's text column, in CSS pixels.
    pub width: f64,
    /// Space between columns: a page's two side margins.
    pub gap: f64,
}

impl Columns {
    /// Distance from one page's left edge to the next's.
    pub fn stride(&self) -> f64 {
        self.width + self.gap
    }

    /// Pages a strip `scroll_width` wide holds; at least one.
    pub fn pages(&self, scroll_width: f64) -> u32 {
        if self.stride() <= 0.0 || self.stride().is_nan() || !scroll_width.is_finite() {
            return 1;
        }
        // The last column has no gap after it.
        let n = ((scroll_width + self.gap) / self.stride()).round();
        clamp_page(n).max(1)
    }

    /// The page a point `x` pixels from the strip's left edge falls on.
    pub fn page_at(&self, x: f64) -> u32 {
        if self.stride() <= 0.0 || self.stride().is_nan() || !x.is_finite() {
            return 0;
        }
        // A rect a hair left of a column edge belongs to the column it is in.
        clamp_page(((x + 0.5) / self.stride()).floor())
    }

    /// The translation that shows `page`.
    pub fn offset(&self, page: u32) -> f64 {
        -(f64::from(page) * self.stride())
    }
}

/// Where a fixed-layout page sits in a frame: shrunk or grown to fit inside
/// it, less `gutter` on every side, keeping its shape, and centred.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fit {
    pub scale: f64,
    pub left: f64,
    pub top: f64,
}

impl Fit {
    pub fn of(page: (f64, f64), frame: (f64, f64), gutter: f64) -> Fit {
        let (pw, ph) = page;
        let (fw, fh) = frame;
        let room_w = (fw - 2.0 * gutter).max(1.0);
        let room_h = (fh - 2.0 * gutter).max(1.0);
        let scale = if pw > 0.0 && ph > 0.0 && pw.is_finite() && ph.is_finite() {
            (room_w / pw).min(room_h / ph)
        } else {
            1.0
        };
        let scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        Fit {
            scale,
            left: ((fw - pw * scale) / 2.0).max(0.0),
            top: ((fh - ph * scale) / 2.0).max(0.0),
        }
    }
}

fn clamp_page(n: f64) -> u32 {
    if n.is_nan() || n <= 0.0 {
        0
    } else if n >= f64::from(u32::MAX) {
        u32::MAX
    } else {
        // In range and integral by the checks above.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let p = n as u32;
        p
    }
}

/// Which text node holds a section offset: the index into `starts` (each
/// node's first offset, ascending, the first being 0) and the offset inside
/// that node. Past the end lands at the end of the last node.
pub fn node_at(starts: &[usize], lens: &[usize], offset: usize) -> Option<(usize, usize)> {
    let i = starts.partition_point(|&s| s <= offset).checked_sub(1)?;
    let start = *starts.get(i)?;
    let len = *lens.get(i)?;
    Some((i, offset.saturating_sub(start).min(len)))
}

/// The first index in `0..len` for which `on_or_after` holds, given that it
/// holds for every index after the first one it holds for. `len` when none.
pub fn first_true(len: usize, mut on_or_after: impl FnMut(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (0usize, len);
    while lo < hi {
        let mid = lo.saturating_add(hi.saturating_sub(lo) / 2);
        if on_or_after(mid) {
            hi = mid;
        } else {
            lo = mid.saturating_add(1);
        }
    }
    lo
}

#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing)]
    use super::*;

    const COLS: Columns = Columns {
        width: 600.0,
        gap: 80.0,
    };

    #[test]
    fn a_strip_counts_its_columns() {
        assert_eq!(COLS.pages(600.0), 1);
        assert_eq!(COLS.pages(1280.0), 2);
        assert_eq!(COLS.pages(1960.0), 3);
        assert_eq!(COLS.pages(0.0), 1);
        assert_eq!(COLS.pages(f64::NAN), 1);
    }

    #[test]
    fn a_point_falls_on_the_page_of_its_column() {
        assert_eq!(COLS.page_at(0.0), 0);
        assert_eq!(COLS.page_at(599.0), 0);
        assert_eq!(COLS.page_at(680.0), 1);
        assert_eq!(COLS.page_at(679.7), 1, "sub-pixel left of the edge");
        assert_eq!(COLS.page_at(-5.0), 0);
        assert_eq!(COLS.offset(2), -1360.0);
    }

    #[test]
    fn a_fixed_page_fits_its_frame_and_is_centred() {
        // A portrait page in a landscape frame is bounded by the height.
        let f = Fit::of((600.0, 800.0), (1280.0, 800.0), 0.0);
        assert_eq!(
            f,
            Fit {
                scale: 1.0,
                left: 340.0,
                top: 0.0
            }
        );
        // In a narrow frame, by the width, less the gutters.
        let f = Fit::of((600.0, 800.0), (320.0, 800.0), 10.0);
        assert!((f.scale - 0.5).abs() < 1e-9);
        assert!((f.left - 10.0).abs() < 1e-9);
        assert!((f.top - 200.0).abs() < 1e-9);
        // A small page grows to fill the frame.
        let f = Fit::of((100.0, 100.0), (400.0, 400.0), 0.0);
        assert!((f.scale - 4.0).abs() < 1e-9);
        // A degenerate page is drawn at its own size.
        assert!((Fit::of((0.0, 0.0), (400.0, 400.0), 0.0).scale - 1.0).abs() < 1e-9);
        assert!((Fit::of((f64::NAN, 1.0), (400.0, 400.0), 0.0).scale - 1.0).abs() < 1e-9);
    }

    #[test]
    fn an_offset_finds_its_text_node() {
        let starts = [0, 5, 5, 12];
        let lens = [5, 0, 7, 3];
        assert_eq!(node_at(&starts, &lens, 0), Some((0, 0)));
        assert_eq!(node_at(&starts, &lens, 4), Some((0, 4)));
        assert_eq!(
            node_at(&starts, &lens, 5),
            Some((2, 0)),
            "skips the empty node"
        );
        assert_eq!(node_at(&starts, &lens, 13), Some((3, 1)));
        assert_eq!(node_at(&starts, &lens, 99), Some((3, 3)));
        assert_eq!(node_at(&[], &[], 0), None);
    }

    #[test]
    fn the_first_visible_character_is_found_by_halving() {
        let xs = [10.0, 300.0, 590.0, 690.0, 700.0, 1400.0];
        let first_on_page_1 = first_true(xs.len(), |i| COLS.page_at(xs[i]) >= 1);
        assert_eq!(first_on_page_1, 3);
        assert_eq!(first_true(4, |_| false), 4);
        assert_eq!(first_true(0, |_| true), 0);
    }
}
