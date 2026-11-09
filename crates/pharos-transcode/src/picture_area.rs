//! Where the picture sits inside a frame that has black bars baked in.
//!
//! Pure maths over 8-bit luma planes, so it is unit-tested on synthetic frames
//! without a decoder; the libav worker (`libav::picture_area`) samples frames
//! and calls [`analyse`].
//!
//! A row or column is picture when its mean luma is above [`LIMIT`], the same
//! default `cropdetect` uses (limited-range black is 16). The result is the
//! bounding box over every usable frame, so one bright frame can only widen
//! it: the check never crops real picture.

use pharos_core::PictureArea;
use serde::{Deserialize, Serialize};

/// Stored with every result; bump it to have the sweep redo older ones.
pub const PICTURE_CHECK_VERSION: u32 = 1;

/// A row, column or frame whose mean luma is at or below this is black.
pub const LIMIT: u64 = 24;

/// Fewer usable frames than this say too little to call bars or no bars.
pub const MIN_USABLE_FRAMES: usize = 3;

/// A box smaller than the frame by under this share (percent) of the width or
/// height on both axes is edge noise, not bars.
const MIN_BAR_PERCENT: u32 = 1;

/// A box under this share (percent) of the frame in either dimension is a
/// mostly-black title card, not bars.
const MIN_AREA_PERCENT: u32 = 40;

/// One decoded frame's 8-bit luma. `stride` is the libav linesize, which can
/// exceed `width`.
#[derive(Debug, Clone)]
pub struct LumaPlane {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub data: Vec<u8>,
}

/// The verdict on a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PictureVerdict {
    /// Bars found; the picture is this area of the frame.
    Bars(PictureArea),
    /// The picture fills the frame.
    NoBars,
    /// A box was found but it covers under 40% of the frame in a dimension:
    /// a dark title card or a very dark film, not bars. Treated as no bars.
    Implausible,
    /// Too few usable frames to tell (dark film, unseekable file).
    Undetermined,
}

/// What the check saw, for the verdict and for the log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PictureAnalysis {
    pub verdict: PictureVerdict,
    /// Frame size, 0x0 when no frame decoded.
    pub width: u32,
    pub height: u32,
    pub usable_frames: u32,
    /// Frames that decoded.
    pub sampled_frames: u32,
}

impl PictureAnalysis {
    /// A check that decoded nothing.
    pub fn nothing_decoded() -> Self {
        Self {
            verdict: PictureVerdict::Undetermined,
            width: 0,
            height: 0,
            usable_frames: 0,
            sampled_frames: 0,
        }
    }
}

/// Bounding box `(left, top, right, bottom)`, right and bottom exclusive, of
/// the picture in one plane. `None` when the plane is malformed or too dark to
/// say anything (its overall mean is at or below [`LIMIT`]).
fn frame_box(p: &LumaPlane) -> Option<(u32, u32, u32, u32)> {
    let (w, h) = (p.width as usize, p.height as usize);
    if w == 0 || h == 0 || p.stride < w {
        return None;
    }
    let mut col_sums = vec![0u64; w];
    let mut row_sums = Vec::with_capacity(h);
    for row in p.data.chunks(p.stride).take(h) {
        let px = row.get(..w)?;
        let mut sum = 0u64;
        for (c, &v) in col_sums.iter_mut().zip(px) {
            *c = c.saturating_add(u64::from(v));
            sum = sum.saturating_add(u64::from(v));
        }
        row_sums.push(sum);
    }
    if row_sums.len() != h {
        return None;
    }
    let total: u64 = row_sums.iter().sum();
    let pixels = (w as u64).saturating_mul(h as u64);
    if total <= LIMIT.saturating_mul(pixels) {
        return None;
    }
    let row_limit = LIMIT.saturating_mul(w as u64);
    let col_limit = LIMIT.saturating_mul(h as u64);
    let first_row = row_sums.iter().position(|&s| s > row_limit)?;
    let last_row = row_sums.iter().rposition(|&s| s > row_limit)?;
    let first_col = col_sums.iter().position(|&s| s > col_limit)?;
    let last_col = col_sums.iter().rposition(|&s| s > col_limit)?;
    Some((
        u32::try_from(first_col).ok()?,
        u32::try_from(first_row).ok()?,
        u32::try_from(last_col.saturating_add(1)).ok()?,
        u32::try_from(last_row.saturating_add(1)).ok()?,
    ))
}

/// Judge the sampled frames. Frames whose size differs from the first are
/// ignored, since their coordinates mean something else.
pub fn analyse(frames: &[LumaPlane]) -> PictureAnalysis {
    let Some(first) = frames.first() else {
        return PictureAnalysis::nothing_decoded();
    };
    let (width, height) = (first.width, first.height);
    let mut sampled = 0u32;
    let mut usable = 0u32;
    let mut union: Option<(u32, u32, u32, u32)> = None;
    for f in frames
        .iter()
        .filter(|f| (f.width, f.height) == (width, height))
    {
        sampled = sampled.saturating_add(1);
        if let Some((l, t, r, b)) = frame_box(f) {
            usable = usable.saturating_add(1);
            union = Some(match union {
                None => (l, t, r, b),
                Some((ul, ut, ur, ub)) => (ul.min(l), ut.min(t), ur.max(r), ub.max(b)),
            });
        }
    }
    let verdict = match union {
        Some(bx) if usable as usize >= MIN_USABLE_FRAMES => decide(bx, width, height),
        _ => PictureVerdict::Undetermined,
    };
    PictureAnalysis {
        verdict,
        width,
        height,
        usable_frames: usable,
        sampled_frames: sampled,
    }
}

/// The area from the union box, or why it is not bars.
fn decide((l, t, r, b): (u32, u32, u32, u32), width: u32, height: u32) -> PictureVerdict {
    let noise = |dim: u32| (dim.saturating_mul(MIN_BAR_PERCENT) / 100).max(1);
    let trimmed_x = width.saturating_sub(r.saturating_sub(l));
    let trimmed_y = height.saturating_sub(b.saturating_sub(t));
    if trimmed_x < noise(width) && trimmed_y < noise(height) {
        return PictureVerdict::NoBars;
    }
    // Inward to even numbers: yuv420 crops and encoders want them.
    let x = l.saturating_add(l & 1);
    let y = t.saturating_add(t & 1);
    let x_end = r & !1;
    let y_end = b & !1;
    let (w, h) = (x_end.saturating_sub(x), y_end.saturating_sub(y));
    let too_small =
        |len: u32, dim: u32| u64::from(len) * 100 < u64::from(dim) * u64::from(MIN_AREA_PERCENT);
    if w == 0 || h == 0 || too_small(w, width) || too_small(h, height) {
        return PictureVerdict::Implausible;
    }
    PictureVerdict::Bars(PictureArea {
        x,
        y,
        width: w,
        height: h,
    })
}

/// The picture area when `frames` show bars, else `None` (no bars, or too
/// little to tell: see [`analyse`] for the difference).
pub fn picture_area(frames: &[LumaPlane]) -> Option<PictureArea> {
    match analyse(frames).verdict {
        PictureVerdict::Bars(a) => Some(a),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u32 = 1920;
    const H: u32 = 1080;

    /// A frame with `picture` (x, y, w, h) at luma `fill` and the rest at
    /// `bar`, on a plane whose stride is wider than the frame.
    fn frame(picture: (u32, u32, u32, u32), fill: u8, bar: u8) -> LumaPlane {
        let stride = W as usize + 32;
        let mut data = vec![bar; stride * H as usize];
        let (px, py, pw, ph) = picture;
        for y in py..py + ph {
            for x in px..px + pw {
                if let Some(v) = data.get_mut(y as usize * stride + x as usize) {
                    *v = fill;
                }
            }
        }
        LumaPlane {
            width: W,
            height: H,
            stride,
            data,
        }
    }

    fn pillarbox() -> (u32, u32, u32, u32) {
        (242, 0, 1436, 1080)
    }

    #[test]
    fn a_pillarboxed_picture_is_found_across_frames() {
        let frames: Vec<_> = (0..7).map(|_| frame(pillarbox(), 120, 16)).collect();
        assert_eq!(
            picture_area(&frames),
            Some(PictureArea {
                x: 242,
                y: 0,
                width: 1436,
                height: 1080
            })
        );
    }

    #[test]
    fn a_letterboxed_picture_is_found() {
        let frames: Vec<_> = (0..4).map(|_| frame((0, 138, 1920, 804), 100, 0)).collect();
        assert_eq!(
            picture_area(&frames),
            Some(PictureArea {
                x: 0,
                y: 138,
                width: 1920,
                height: 804
            })
        );
    }

    #[test]
    fn odd_edges_round_inward_to_even() {
        let frames: Vec<_> = (0..3)
            .map(|_| frame((241, 1, 1437, 1077), 120, 16))
            .collect();
        // Box is x 241..1678, y 1..1078: x/y round up, far edges round down.
        assert_eq!(
            picture_area(&frames),
            Some(PictureArea {
                x: 242,
                y: 2,
                width: 1436,
                height: 1076
            })
        );
    }

    #[test]
    fn a_full_frame_picture_has_no_bars() {
        let frames: Vec<_> = (0..5).map(|_| frame((0, 0, W, H), 120, 16)).collect();
        let a = analyse(&frames);
        assert_eq!(a.verdict, PictureVerdict::NoBars);
        assert_eq!((a.width, a.height, a.usable_frames), (W, H, 5));
    }

    #[test]
    fn a_few_pixels_of_edge_noise_are_not_bars() {
        // 6 px off each side is under 1% of 1920 (19 px) and of 1080 (10 px).
        let frames: Vec<_> = (0..5)
            .map(|_| frame((6, 4, W - 12, H - 8), 120, 16))
            .collect();
        assert_eq!(analyse(&frames).verdict, PictureVerdict::NoBars);
    }

    #[test]
    fn one_wide_frame_widens_the_area() {
        let mut frames: Vec<_> = (0..6)
            .map(|_| frame((480, 0, 960, 1080), 120, 16))
            .collect();
        frames.push(frame(pillarbox(), 120, 16));
        assert_eq!(
            picture_area(&frames),
            Some(PictureArea {
                x: 242,
                y: 0,
                width: 1436,
                height: 1080
            })
        );
    }

    #[test]
    fn dark_frames_are_skipped_not_counted() {
        let mut frames: Vec<_> = (0..3).map(|_| frame(pillarbox(), 120, 16)).collect();
        // A near-black frame (a fade) would otherwise shrink the box to nothing.
        frames.push(frame((0, 0, W, H), 20, 16));
        frames.push(frame((900, 500, 40, 40), 30, 0));
        let a = analyse(&frames);
        assert_eq!(a.usable_frames, 3);
        assert_eq!(a.sampled_frames, 5);
        assert!(matches!(a.verdict, PictureVerdict::Bars(_)));
    }

    #[test]
    fn fewer_than_three_usable_frames_is_undetermined() {
        let mut frames: Vec<_> = (0..2).map(|_| frame(pillarbox(), 120, 16)).collect();
        frames.extend((0..5).map(|_| frame((0, 0, W, H), 0, 0)));
        let a = analyse(&frames);
        assert_eq!(a.verdict, PictureVerdict::Undetermined);
        assert_eq!(a.usable_frames, 2);
        assert_eq!(picture_area(&frames), None);
        assert_eq!(analyse(&[]).verdict, PictureVerdict::Undetermined);
    }

    #[test]
    fn a_mostly_black_title_card_is_not_bars() {
        // A bright strip 30% of the width wide.
        let frames: Vec<_> = (0..4).map(|_| frame((800, 0, 576, 1080), 200, 0)).collect();
        assert_eq!(analyse(&frames).verdict, PictureVerdict::Implausible);
        assert_eq!(picture_area(&frames), None);
    }

    #[test]
    fn a_short_or_malformed_plane_is_unusable() {
        let mut f = frame(pillarbox(), 120, 16);
        f.data.truncate(1000);
        let frames = vec![f.clone(), f.clone(), f];
        let a = analyse(&frames);
        assert_eq!(a.usable_frames, 0);
        assert_eq!(a.verdict, PictureVerdict::Undetermined);
    }

    #[test]
    fn frames_of_another_size_are_ignored() {
        let mut frames: Vec<_> = (0..3).map(|_| frame(pillarbox(), 120, 16)).collect();
        frames.push(LumaPlane {
            width: 640,
            height: 360,
            stride: 640,
            data: vec![200; 640 * 360],
        });
        let a = analyse(&frames);
        assert_eq!(a.sampled_frames, 3);
        assert!(matches!(a.verdict, PictureVerdict::Bars(_)));
    }
}
