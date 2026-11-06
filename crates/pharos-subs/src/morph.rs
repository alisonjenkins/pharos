//! Mask-space operations: border dilation and the rounded backdrop.

use crate::raster::{isqrt, Bitmap};

/// Grows coverage by a disc of radius `r` pixels (`\bord`). The output is
/// `2r` larger in each dimension; the source sits at offset (`r`, `r`).
pub(crate) fn dilate(src: &Bitmap, r: u32) -> Bitmap {
    if r == 0 || src.is_empty() {
        return src.clone();
    }
    let ru = r as usize;
    let (w, h) = (src.w as usize + 2 * ru, src.h as usize + 2 * ru);
    // Half-chord of the disc at each row offset.
    let hw: Vec<usize> = (0..=ru)
        .map(|dy| isqrt((ru * ru - dy * dy) as u64) as usize)
        .collect();
    let max_hw = hw.first().copied().unwrap_or(0);

    let mut base = vec![0u8; w * h];
    for y in 0..src.h as usize {
        let at = (y + ru) * w + ru;
        if let Some(d) = base.get_mut(at..at + src.w as usize) {
            d.copy_from_slice(src.row(y as u32));
        }
    }
    // levels[k] = base max-filtered horizontally by +-k, built incrementally
    // from the previous level so each costs three compares per pixel.
    let mut levels: Vec<Vec<u8>> = Vec::with_capacity(max_hw + 1);
    levels.push(base.clone());
    for k in 1..=max_hw {
        let mut cur = levels.last().cloned().unwrap_or_default();
        for (brow, crow) in base.chunks_exact(w).zip(cur.chunks_exact_mut(w)) {
            for (x, c) in crow.iter_mut().enumerate() {
                let l = if x >= k {
                    brow.get(x - k).copied().unwrap_or(0)
                } else {
                    0
                };
                let r = brow.get(x + k).copied().unwrap_or(0);
                *c = (*c).max(l).max(r);
            }
        }
        levels.push(cur);
    }

    let mut out = Bitmap::new(w as u32, h as u32);
    for (y, orow) in out.data.chunks_exact_mut(w).enumerate() {
        for (dy, &k) in hw.iter().enumerate() {
            let Some(lvl) = levels.get(k) else { continue };
            let rows = [y.checked_sub(dy), y.checked_add(dy)];
            for (i, sy) in rows.into_iter().enumerate() {
                if i == 1 && dy == 0 {
                    break;
                }
                if let Some(s) = sy
                    .filter(|s| *s < h)
                    .and_then(|s| lvl.get(s * w..s * w + w))
                {
                    for (o, s) in orow.iter_mut().zip(s) {
                        *o = (*o).max(*s);
                    }
                }
            }
        }
    }
    out
}

/// One backdrop for a stacked block of lines: `rects` are each line's
/// padded box `(x, y, w, h)` in px, top to bottom, touching vertically.
/// Each box is shrunk by `r`, the distance fields are merged with a
/// polynomial smooth-min (`k = 2r`) and offset back out by `r`, so outer
/// corners round at `r`, the seams between lines close with no notch, and
/// the inner corners where line widths differ get a smooth fillet. Returns
/// the coverage and its top-left in px. Cost is one pass over the block's
/// bounding box per line; it runs only when a cue changes.
pub(crate) fn rounded_union(rects: &[(i32, i32, u32, u32)], r: u32) -> Option<(Bitmap, i32, i32)> {
    let x0 = rects.iter().map(|b| b.0).min()?;
    let y0 = rects.iter().map(|b| b.1).min()?;
    let x1 = rects.iter().map(|b| b.0 + b.2 as i32).max()?;
    let y1 = rects.iter().map(|b| b.1 + b.3 as i32).max()?;
    let (w, h) = ((x1 - x0).max(1) as u32, (y1 - y0).max(1) as u32);
    let min_side = rects.iter().map(|b| b.2.min(b.3)).min()?;
    let r = r.min(min_side / 2) as f32;
    let k = 2.0 * r;
    // (centre x, centre y, half width, half height) of each shrunk box.
    let boxes: Vec<[f32; 4]> = rects
        .iter()
        .map(|&(bx, by, bw, bh)| {
            let (hw, hh) = (bw as f32 / 2.0, bh as f32 / 2.0);
            [
                (bx - x0) as f32 + hw,
                (by - y0) as f32 + hh,
                (hw - r).max(0.0),
                (hh - r).max(0.0),
            ]
        })
        .collect();
    // Only pixels near a corner, a seam or a fillet need the full field.
    // Per row, each box's straight band (within `r - 0.5` of the shrunk
    // box) is solid and is filled with a memset; beyond what the smooth-min
    // can pull (`k / 4`) is empty; the field is evaluated only in between,
    // a few radii at each end of each row.
    let solid = r - 0.5;
    let empty = r + 0.5 + k / 4.0;
    let mut bm = Bitmap::new(w, h);
    let wi = w as i32;
    for py in 0..h {
        let fy = py as f32 + 0.5;
        let row = &mut bm.data[(py * w) as usize..((py + 1) * w) as usize];
        let mut edges: Vec<(i32, i32)> = Vec::with_capacity(2 * boxes.len());
        for &[cx, cy, hw, hh] in &boxes {
            let qy = (fy - cy).abs() - hh;
            if qy >= empty {
                continue;
            }
            let ext = if qy <= 0.0 {
                hw + solid
            } else if qy <= solid {
                hw
            } else {
                -1.0
            };
            let (lo, hi) = if ext >= 0.0 {
                let lo = ((cx - ext - 0.5).ceil() as i32).clamp(0, wi);
                let hi = ((cx + ext - 0.5).floor() as i32 + 1).clamp(0, wi);
                row[lo as usize..hi.max(lo) as usize].fill(255);
                (lo, hi.max(lo))
            } else {
                let mid = (cx.round() as i32).clamp(0, wi);
                (mid, mid)
            };
            let reach = hw + empty;
            let elo = ((cx - reach).floor() as i32).clamp(0, wi);
            let ehi = ((cx + reach).ceil() as i32).clamp(0, wi);
            edges.push((elo, lo));
            edges.push((hi, ehi));
        }
        for (from, to) in edges {
            for px in from..to {
                if row[px as usize] == 255 {
                    continue;
                }
                let fx = px as f32 + 0.5;
                let mut d = f32::INFINITY;
                for &[cx, cy, hw, hh] in &boxes {
                    let qx = (fx - cx).abs() - hw;
                    let qy = (fy - cy).abs() - hh;
                    let (ox, oy) = (qx.max(0.0), qy.max(0.0));
                    let di = (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0);
                    d = if d.is_infinite() || k <= 0.0 {
                        d.min(di)
                    } else {
                        let t = (k - (d - di).abs()).max(0.0) / k;
                        d.min(di) - t * t * k / 4.0
                    };
                }
                let cov = (r - d + 0.5).clamp(0.0, 1.0);
                row[px as usize] = (cov * 255.0).round() as u8;
            }
        }
    }
    Some((bm, x0, y0))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    #[test]
    fn dilate_grows_by_radius() {
        let mut src = Bitmap::new(3, 3);
        src.fill_rect(1, 1, 1, 1);
        let d = dilate(&src, 2);
        assert_eq!((d.w, d.h), (7, 7));
        let at = |x: usize, y: usize| d.data[y * 7 + x];
        assert_eq!(at(3, 3), 255);
        assert_eq!(at(1, 3), 255);
        assert_eq!(at(5, 3), 255);
        assert_eq!(at(3, 1), 255);
        assert_eq!(at(0, 0), 0);
        // diagonal corner of the 5x5 box lies outside radius 2
        assert_eq!(at(1, 1), 0);
    }

    #[test]
    fn rounded_union_is_one_bubble_without_a_notch_between_lines() {
        // A wide line over a narrow one, touching at y = 30.
        let (bm, x0, y0) =
            rounded_union(&[(0, 0, 200, 30), (50, 30, 100, 30)], 10).expect("non-empty");
        assert_eq!((x0, y0, bm.w, bm.h), (0, 0, 200, 60));
        let at = |x: u32, y: u32| bm.data[(y * bm.w + x) as usize];
        // Outer corners rounded.
        assert_eq!(at(0, 0), 0);
        assert_eq!(at(100, 15), 255);
        // The seam is solid across the narrow line's full width, edges included.
        for x in 52..148 {
            assert_eq!(at(x, 29), 255, "seam x={x}");
            assert_eq!(at(x, 30), 255, "seam x={x}");
        }
        // The inner corner where the narrow line starts is filleted:
        // filled just outside the narrow box, below the wide one.
        assert!(at(48, 31) > 0);
        // Far outside the narrow line stays empty.
        assert_eq!(at(10, 50), 0);
    }

    #[test]
    fn rounded_union_of_equal_lines_has_straight_sides_across_the_seam() {
        let (bm, ..) = rounded_union(&[(0, 0, 120, 30), (0, 30, 120, 30)], 10).expect("non-empty");
        let at = |x: u32, y: u32| bm.data[(y * bm.w + x) as usize];
        for y in 20..40 {
            assert_eq!(at(0, y), 255, "left edge y={y}");
            assert_eq!(at(119, y), 255, "right edge y={y}");
        }
    }
}
