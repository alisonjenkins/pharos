//! Mask-space operations: border dilation and the rounded backdrop.

use crate::raster::{isqrt, Bitmap, FP};

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

/// Rounded-rectangle coverage, `w` x `h`, corner radius `r` px. Integer
/// distance to the corner circle at pixel centres (24.8).
pub(crate) fn rounded_rect(w: u32, h: u32, r: u32) -> Bitmap {
    let mut bm = Bitmap::new(w, h);
    bm.data.fill(255);
    let r = r.min(w / 2).min(h / 2);
    if r == 0 {
        return bm;
    }
    let fp = i64::from(FP);
    let rf = i64::from(r) * fp;
    for cy in 0..r {
        for cx in 0..r {
            let dx = rf - (i64::from(cx) * fp + fp / 2);
            let dy = rf - (i64::from(cy) * fp + fp / 2);
            let dist = isqrt((dx * dx + dy * dy) as u64) as i64;
            let cov = ((rf - dist + fp / 2).clamp(0, fp) * 255 / fp) as u8;
            for (x, y) in [
                (cx, cy),
                (w - 1 - cx, cy),
                (cx, h - 1 - cy),
                (w - 1 - cx, h - 1 - cy),
            ] {
                if let Some(p) = bm.data.get_mut(y as usize * w as usize + x as usize) {
                    *p = cov;
                }
            }
        }
    }
    bm
}

#[cfg(test)]
mod tests {
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
    fn rounded_rect_corners_cut_middle_solid() {
        let rr = rounded_rect(40, 20, 8);
        let at = |x: usize, y: usize| rr.data[y * 40 + x];
        assert_eq!(at(0, 0), 0);
        assert_eq!(at(20, 10), 255);
        assert_eq!(at(0, 10), 255);
        assert_eq!(at(20, 0), 255);
        assert_eq!(at(1, 0), at(38, 0));
        assert_eq!(at(0, 1), at(0, 18));
    }
}
