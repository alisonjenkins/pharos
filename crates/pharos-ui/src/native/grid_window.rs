//! Which of a browse grid's tiles a TV keeps in the page (B1406/V1376):
//! the rows on screen and a few either side, with the rows left out held
//! open by a spacer above and below so the page keeps its full height and
//! every tile its place.

/// Rows kept in the page beyond those on screen, each side: more than a
/// held arrow key crosses before the page reports where it stands again.
pub const OVERSCAN_ROWS: usize = 6;

/// Tiles shown before the page has said where the grid stands.
pub const FIRST_TILES: usize = 96;

/// Where the browse grid stands, as the page measured it.
#[derive(Debug, Clone, Copy, PartialEq, serde::Deserialize)]
pub struct GridView {
    /// The grid's top below the viewport's, in CSS px (negative once
    /// scrolled past).
    pub top: f64,
    /// The viewport's height in CSS px.
    pub height: f64,
    pub cols: u32,
    /// From one row's top to the next one's, in CSS px.
    pub pitch: f64,
    /// The gap between rows, in CSS px.
    pub gap: f64,
    /// The last placement the page had carried out as it measured
    /// (`scroll_to_tile`, `scroll_tile_to`).
    #[serde(default)]
    pub seq: u64,
}

/// The tiles `from..to` of a grid kept in the page, and the height in CSS
/// px of the spacer before and after them (0 for none).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Window {
    pub from: usize,
    pub to: usize,
    pub before: f64,
    pub after: f64,
}

impl Window {
    /// Every one of `count` tiles, with no spacers.
    pub fn all(count: usize) -> Self {
        Self {
            from: 0,
            to: count,
            before: 0.0,
            after: 0.0,
        }
    }

    /// The first [`FIRST_TILES`] of `count`, before the grid is measured.
    pub fn first(count: usize) -> Self {
        Self::all(count.min(FIRST_TILES))
    }
}

/// The tiles of a grid of `count` to keep in the page as it stands in
/// `view`, or around the tile `anchor` the grid is being moved to.
pub fn window(view: &GridView, count: usize, anchor: Option<usize>) -> Window {
    let cols = usize::try_from(view.cols).unwrap_or(1).max(1);
    let measured = view.pitch.is_finite() && view.pitch > 0.0;
    if count == 0 || !measured || !view.height.is_finite() || !view.top.is_finite() {
        return Window::first(count);
    }
    let rows = count.div_ceil(cols);
    // `as` saturates, so a page scrolled past the end clamps.
    let row_at = |px: f64| (px.max(0.0) / view.pitch).floor() as usize;
    let screen = row_at(view.height).saturating_add(1);
    let (first, last) = match anchor {
        // The anchor may land anywhere on screen: a screen of rows above it
        // as well as below.
        Some(at) => {
            let row = at / cols;
            (row.saturating_sub(screen), row.saturating_add(screen))
        }
        None => (row_at(-view.top), row_at(view.height - view.top)),
    };
    let from_row = first.saturating_sub(OVERSCAN_ROWS).min(rows);
    let to_row = last
        .saturating_add(OVERSCAN_ROWS)
        .saturating_add(1)
        .min(rows)
        .max(from_row);
    // A spacer is a row of its own, so the gap after it is already one of
    // the rows' pitch.
    let span = |n: usize| -> f64 {
        if n == 0 {
            0.0
        } else {
            (n as f64 * view.pitch - view.gap).max(0.0)
        }
    };
    Window {
        from: (from_row * cols).min(count),
        to: (to_row * cols).min(count),
        before: span(from_row),
        after: span(rows - to_row),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(top: f64) -> GridView {
        GridView {
            top,
            height: 540.0,
            cols: 10,
            pitch: 150.0,
            gap: 12.0,
            seq: 0,
        }
    }

    #[test]
    fn the_top_of_a_grid_keeps_the_rows_on_screen_and_some_below() {
        let w = window(&view(80.0), 1000, None);
        // Rows 0..=3 reach the screen; six more below.
        assert_eq!((w.from, w.to), (0, 100));
        assert!(w.before.abs() < f64::EPSILON);
        assert!((w.after - (90.0 * 150.0 - 12.0)).abs() < 1e-9);
    }

    #[test]
    fn far_down_keeps_rows_either_side_and_spacers_hold_the_rest() {
        // Scrolled 30 rows past the grid's top.
        let w = window(&view(-4500.0), 1000, None);
        assert_eq!((w.from, w.to), (240, 400));
        assert!((w.before - (24.0 * 150.0 - 12.0)).abs() < 1e-9);
        assert!((w.after - (60.0 * 150.0 - 12.0)).abs() < 1e-9);
        // Every row is accounted for: kept, or held by a spacer.
        let kept = (w.to - w.from) / 10;
        assert_eq!(24 + kept + 60, 100);
    }

    #[test]
    fn the_last_row_may_be_part_full() {
        let w = window(&view(-14_000.0), 995, None);
        assert_eq!(w.to, 995);
        assert!(w.after.abs() < f64::EPSILON);
    }

    #[test]
    fn a_tile_being_moved_to_is_kept_with_a_screen_either_side() {
        let w = window(&view(80.0), 1000, Some(605));
        // Row 60, four rows on screen: rows 50..=70 (with overscan).
        assert_eq!((w.from, w.to), (500, 710));
        assert!(w.from <= 605 && 605 < w.to);
    }

    #[test]
    fn an_anchor_not_loaded_yet_keeps_nothing_past_the_end() {
        let w = window(&view(80.0), 100, Some(900));
        assert_eq!((w.from, w.to), (100, 100));
        assert!(w.after.abs() < f64::EPSILON);
    }

    #[test]
    fn an_unmeasured_grid_shows_its_first_tiles() {
        let mut v = view(0.0);
        v.pitch = 0.0;
        assert_eq!(window(&v, 1000, None), Window::first(1000));
        assert_eq!(Window::first(10), Window::all(10));
        assert_eq!(window(&view(0.0), 0, None), Window::all(0));
    }
}
