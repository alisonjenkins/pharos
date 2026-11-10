//! Text burned into the bottom of the picture (spec 028, B1207).
//!
//! Pure maths over one 8-bit luma plane, so it is unit-tested on synthetic
//! frames; the libav subtitle-signal pass decodes keyframes and calls
//! [`bottom_text_share`]. A styled line drawn over burned-in captions or a
//! sign at the bottom is unreadable, so the director lifts it to the top.
//!
//! # What counts as text
//!
//! Captions and most signs are bright strokes against a dark outline or
//! background (or the reverse): a row through them has many strong
//! horizontal edges, and bright pixels with a dark pixel within a couple of
//! pixels. A row of the bottom band is a *text row* when both are dense
//! enough over its middle 80%. Text is a short run of such rows: at least
//! [`MIN_RUN_SHARE`] of the frame's height (a line of small print), and at
//! most [`MAX_RUN_SHARE`] (taller than that is a textured scene, not text).
//! Smooth pictures, mid-tone texture and a lone bright edge do not qualify.

use crate::picture_area::LumaPlane;

/// The band searched: from this share of the height down to [`BAND_END`].
const BAND_START: f32 = 0.70;
const BAND_END: f32 = 0.97;
/// The middle share of the width searched; logos sit in the corners.
const SIDE_MARGIN: f32 = 0.10;
/// A horizontal step at least this large (over two pixels) is an edge.
const EDGE_STEP: u8 = 48;
/// Bright and dark, for the outline test.
const BRIGHT: u8 = 190;
const DARK: u8 = 70;
/// How far, in pixels, a bright pixel's dark partner may be.
const OUTLINE_REACH: usize = 2;
/// Shares of a row's searched width that must be edges, and bright-by-dark
/// outline pixels, for the row to be a text row.
const MIN_EDGE_SHARE: f32 = 0.08;
const MIN_OUTLINE_SHARE: f32 = 0.02;
/// A run of text rows between these shares of the frame height is text.
pub const MIN_RUN_SHARE: f32 = 0.025;
pub const MAX_RUN_SHARE: f32 = 0.16;
/// One row inside a run may miss (the gap between two caption lines, or a
/// row through the middle of the letters with few edges).
const MAX_GAP_ROWS: usize = 1;

/// The height of the tallest text block in the bottom band, as a share of
/// the frame's height; 0.0 when there is none (or the plane is unusable).
pub fn bottom_text_share(plane: &LumaPlane) -> f32 {
    let (w, h) = (plane.width as usize, plane.height as usize);
    if w < 16 || h < 16 || plane.stride < w || plane.data.len() < plane.stride * h {
        return 0.0;
    }
    let top = (h as f32 * BAND_START) as usize;
    let bottom = ((h as f32 * BAND_END) as usize).min(h);
    let x0 = ((w as f32 * SIDE_MARGIN) as usize).max(OUTLINE_REACH + 1);
    let x1 = (w - (w as f32 * SIDE_MARGIN) as usize).min(w - OUTLINE_REACH - 1);
    if x1 <= x0 || bottom <= top {
        return 0.0;
    }
    let row = |y: usize| &plane.data[y * plane.stride..y * plane.stride + w];
    let span = (x1 - x0) as f32;

    let mut best = 0usize;
    let mut run = 0usize;
    let mut gap = 0usize;
    for y in top..bottom {
        let r = row(y);
        let mut edges = 0usize;
        let mut outlined = 0usize;
        for x in x0..x1 {
            if r[x + 1].abs_diff(r[x - 1]) >= EDGE_STEP {
                edges += 1;
            }
            if r[x] >= BRIGHT && has_dark_near(plane, x, y, w, h) {
                outlined += 1;
            }
        }
        let text_row =
            edges as f32 >= MIN_EDGE_SHARE * span && outlined as f32 >= MIN_OUTLINE_SHARE * span;
        if text_row {
            run += 1 + gap;
            gap = 0;
        } else if run > 0 && gap < MAX_GAP_ROWS {
            gap += 1;
        } else {
            best = best.max(run);
            run = 0;
            gap = 0;
        }
        if run as f32 > MAX_RUN_SHARE * h as f32 {
            // A whole textured band: not text, and nothing below it can be.
            return 0.0;
        }
    }
    best = best.max(run);
    let share = best as f32 / h as f32;
    if share >= MIN_RUN_SHARE {
        share
    } else {
        0.0
    }
}

fn has_dark_near(plane: &LumaPlane, x: usize, y: usize, w: usize, h: usize) -> bool {
    let at = |xx: usize, yy: usize| plane.data[yy * plane.stride + xx];
    (1..=OUTLINE_REACH).any(|d| {
        (x >= d && at(x - d, y) <= DARK)
            || (x + d < w && at(x + d, y) <= DARK)
            || (y >= d && at(x, y - d) <= DARK)
            || (y + d < h && at(x, y + d) <= DARK)
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    const W: usize = 480;
    const H: usize = 270;

    fn plane(f: impl Fn(usize, usize) -> u8) -> LumaPlane {
        let mut data = vec![0u8; W * H];
        for y in 0..H {
            for x in 0..W {
                data[y * W + x] = f(x, y);
            }
        }
        LumaPlane {
            width: W as u32,
            height: H as u32,
            stride: W,
            data,
        }
    }

    /// A caption: white vertical strokes 2 px wide every 6 px, rows
    /// `y0..y0 + rows`, columns `x0..x1`, each with a 2 px black outline, over
    /// a mid-grey picture.
    fn caption(y0: usize, rows: usize, x0: usize, x1: usize) -> impl Fn(usize, usize) -> u8 {
        move |x, y| {
            let inside = (y0..y0 + rows).contains(&y) && (x0..x1).contains(&x);
            let outline = (y0.saturating_sub(2)..y0 + rows + 2).contains(&y)
                && (x0.saturating_sub(2)..x1 + 2).contains(&x);
            if inside && (x - x0) % 6 < 2 {
                235
            } else if outline {
                20
            } else {
                110
            }
        }
    }

    #[test]
    fn a_caption_at_the_bottom_is_found_with_its_height() {
        let p = plane(caption(225, 14, 120, 360));
        let share = bottom_text_share(&p);
        assert!(share >= 14.0 / H as f32, "{share}");
        assert!(share <= 20.0 / H as f32, "{share}");
    }

    #[test]
    fn a_caption_higher_up_is_not_bottom_text() {
        let p = plane(caption(60, 14, 120, 360));
        assert_eq!(bottom_text_share(&p), 0.0);
    }

    #[test]
    fn small_print_below_the_minimum_height_is_ignored() {
        let p = plane(caption(240, 4, 120, 360));
        assert_eq!(bottom_text_share(&p), 0.0);
    }

    #[test]
    fn a_smooth_picture_has_none() {
        assert_eq!(bottom_text_share(&plane(|x, y| ((x + y) / 4) as u8)), 0.0);
        assert_eq!(bottom_text_share(&plane(|_, _| 16)), 0.0);
        assert_eq!(bottom_text_share(&plane(|_, _| 235)), 0.0);
    }

    #[test]
    fn mid_tone_texture_has_none() {
        // Strong edges everywhere but never bright against dark.
        let p = plane(|x, y| if (x / 2 + y) % 2 == 0 { 90 } else { 170 });
        assert_eq!(bottom_text_share(&p), 0.0);
    }

    #[test]
    fn a_whole_textured_band_is_scenery_not_text() {
        // Bright and dark stripes over the whole bottom: a fence, not a caption.
        let p = plane(|x, y| {
            if y > 150 && x % 6 < 2 {
                235
            } else if y > 150 {
                20
            } else {
                110
            }
        });
        assert_eq!(bottom_text_share(&p), 0.0);
    }

    #[test]
    fn a_lone_bright_edge_has_none() {
        // A horizon: bright above, dark below, one edge row.
        let p = plane(|_, y| if y < 240 { 230 } else { 15 });
        assert_eq!(bottom_text_share(&p), 0.0);
    }

    #[test]
    fn a_bad_plane_is_none_not_a_panic() {
        let mut p = plane(|_, _| 0);
        p.data.truncate(10);
        assert_eq!(bottom_text_share(&p), 0.0);
        let tiny = LumaPlane {
            width: 4,
            height: 4,
            stride: 4,
            data: vec![0; 16],
        };
        assert_eq!(bottom_text_share(&tiny), 0.0);
    }
}
