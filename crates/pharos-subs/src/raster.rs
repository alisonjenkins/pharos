//! Own anti-aliased scanline rasteriser. Everything past outline scaling is
//! integer arithmetic so masks are bit-identical on every target (FR-005).
//!
//! Coordinates are 24.8 fixed point, y down. Coverage is an exact-area
//! accumulation buffer: signed area is summed along each row and clamped, so
//! same-direction overlaps saturate at 255 (the non-zero rule for the
//! overlapping contours fonts contain) and opposite-direction contours cut
//! holes.

use skrifa::font::glyph::outline::PathSink;

pub(crate) const FP: i32 = 256;

/// 8-bit coverage bitmap, `stride == w`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Bitmap {
    pub w: u32,
    pub h: u32,
    pub data: Vec<u8>,
}

impl Bitmap {
    pub fn new(w: u32, h: u32) -> Self {
        Self {
            w,
            h,
            data: vec![0; (w as usize) * (h as usize)],
        }
    }

    pub fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    pub fn bytes(&self) -> usize {
        self.data.len()
    }

    pub fn row(&self, y: u32) -> &[u8] {
        let s = (y as usize) * (self.w as usize);
        self.data.get(s..s + self.w as usize).unwrap_or(&[])
    }

    fn row_mut(&mut self, y: u32) -> &mut [u8] {
        let s = (y as usize) * (self.w as usize);
        let w = self.w as usize;
        self.data.get_mut(s..s + w).unwrap_or(&mut [])
    }

    /// Unions `src` into `self` with its top-left at (`x`, `y`), clipping.
    pub fn blit_max(&mut self, x: i32, y: i32, src: &Bitmap) {
        for sy in 0..src.h {
            let dy = y.saturating_add(sy as i32);
            if dy < 0 || dy >= self.h as i32 {
                continue;
            }
            let x0 = x.max(0);
            let x1 = x.saturating_add(src.w as i32).min(self.w as i32);
            if x1 <= x0 {
                continue;
            }
            let s0 = (x0 - x) as usize;
            let n = (x1 - x0) as usize;
            let srow = src.row(sy);
            let Some(s) = srow.get(s0..s0 + n) else {
                continue;
            };
            let drow = self.row_mut(dy as u32);
            let Some(d) = drow.get_mut(x0 as usize..x0 as usize + n) else {
                continue;
            };
            for (d, s) in d.iter_mut().zip(s) {
                *d = (*d).max(*s);
            }
        }
    }

    /// Fills a rectangle with full coverage, clipping.
    pub fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32) {
        for yy in y.max(0)..(y.saturating_add(h)).min(self.h as i32) {
            let x0 = x.max(0);
            let x1 = x.saturating_add(w).min(self.w as i32);
            if x1 > x0 {
                if let Some(d) = self.row_mut(yy as u32).get_mut(x0 as usize..x1 as usize) {
                    d.fill(255);
                }
            }
        }
    }
}

/// Outline accumulated as flattened integer line segments.
#[derive(Default)]
pub(crate) struct LinePath {
    lines: Vec<[i32; 4]>,
    cur: (i32, i32),
    start: (i32, i32),
}

fn fx(v: f32) -> i32 {
    // f32 -> fixed is the one float step; IEEE multiply+round is exact and
    // identical on every target.
    (v * FP as f32).round() as i32
}

pub(crate) fn isqrt(n: u64) -> u64 {
    if n < 2 {
        return n;
    }
    let mut x = n;
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

impl LinePath {
    pub fn clear(&mut self) {
        self.lines.clear();
    }

    fn line(&mut self, x: i32, y: i32) {
        self.lines.push([self.cur.0, self.cur.1, x, y]);
        self.cur = (x, y);
    }

    fn quad_fixed(&mut self, c: (i32, i32), p: (i32, i32)) {
        let p0 = self.cur;
        let ddx = (p0.0 - 2 * c.0 + p.0).unsigned_abs();
        let ddy = (p0.1 - 2 * c.1 + p.1).unsigned_abs();
        let dev = u64::from(ddx.max(ddy));
        let n = (1 + isqrt(dev / 16)).min(32) as i64;
        let n2 = n * n;
        for i in 1..n {
            let a = (n - i) * (n - i);
            let b = 2 * i * (n - i);
            let d = i * i;
            let x = (a * i64::from(p0.0) + b * i64::from(c.0) + d * i64::from(p.0) + n2 / 2) / n2;
            let y = (a * i64::from(p0.1) + b * i64::from(c.1) + d * i64::from(p.1) + n2 / 2) / n2;
            self.line(x as i32, y as i32);
        }
        self.line(p.0, p.1);
    }

    fn cubic_fixed(&mut self, c1: (i32, i32), c2: (i32, i32), p: (i32, i32)) {
        let p0 = self.cur;
        let d = |a: i32, b: i32, c: i32, e: i32| {
            (a - 2 * b + c)
                .unsigned_abs()
                .max((b - 2 * c + e).unsigned_abs())
        };
        let dev = u64::from(d(p0.0, c1.0, c2.0, p.0).max(d(p0.1, c1.1, c2.1, p.1)));
        let n = (1 + isqrt(dev / 8)).min(48) as i64;
        let n3 = n * n * n;
        for i in 1..n {
            let m = n - i;
            let (w0, w1, w2, w3) = (m * m * m, 3 * m * m * i, 3 * m * i * i, i * i * i);
            let ev = |a: i32, b: i32, c: i32, e: i32| {
                ((w0 * i64::from(a)
                    + w1 * i64::from(b)
                    + w2 * i64::from(c)
                    + w3 * i64::from(e)
                    + n3 / 2)
                    / n3) as i32
            };
            self.line(ev(p0.0, c1.0, c2.0, p.0), ev(p0.1, c1.1, c2.1, p.1));
        }
        self.line(p.0, p.1);
    }

    /// Rasterises into a tight bitmap; returns it with its top-left offset
    /// from the path origin in whole pixels. `None` for an empty outline.
    pub fn rasterise(&self, scratch: &mut Vec<i32>) -> Option<(Bitmap, i32, i32)> {
        let mut it = self.lines.iter();
        let first = it.next()?;
        let (mut minx, mut miny, mut maxx, mut maxy) = (first[0], first[1], first[0], first[1]);
        for l in &self.lines {
            minx = minx.min(l[0]).min(l[2]);
            maxx = maxx.max(l[0]).max(l[2]);
            miny = miny.min(l[1]).min(l[3]);
            maxy = maxy.max(l[1]).max(l[3]);
        }
        let left = minx.div_euclid(FP);
        let top = miny.div_euclid(FP);
        let w = ((maxx + FP - 1).div_euclid(FP) - left).max(1) as usize;
        let h = ((maxy + FP - 1).div_euclid(FP) - top).max(1) as usize;
        if w > 8192 || h > 8192 {
            return None;
        }
        let stride = w + 2;
        scratch.clear();
        scratch.resize(stride * h, 0);
        let (ox, oy) = (left * FP, top * FP);
        for l in &self.lines {
            accumulate(
                scratch,
                stride,
                h,
                l[0] - ox,
                l[1] - oy,
                l[2] - ox,
                l[3] - oy,
            );
        }
        let mut bm = Bitmap::new(w as u32, h as u32);
        for (y, acc_row) in scratch.chunks_exact(stride).enumerate() {
            let mut sum: i64 = 0;
            let out = bm.row_mut(y as u32);
            for (o, a) in out.iter_mut().zip(acc_row) {
                sum += i64::from(*a);
                *o = ((sum.abs() * 255 + 32768) >> 16).min(255) as u8;
            }
        }
        Some((bm, left, top))
    }
}

impl PathSink for LinePath {
    fn move_to(&mut self, x: f32, y: f32) {
        // Close the previous contour implicitly: fill rules need closed shapes.
        self.close();
        self.cur = (fx(x), -fx(y));
        self.start = self.cur;
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.line(fx(x), -fx(y));
    }

    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        self.quad_fixed((fx(cx0), -fx(cy0)), (fx(x), -fx(y)));
    }

    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        self.cubic_fixed((fx(cx0), -fx(cy0)), (fx(cx1), -fx(cy1)), (fx(x), -fx(y)));
    }

    fn close(&mut self) {
        if self.cur != self.start {
            let s = self.start;
            self.line(s.0, s.1);
        }
    }
}

/// Adds one edge to the accumulation buffer (see module docs).
fn accumulate(acc: &mut [i32], stride: usize, h: usize, x0: i32, y0: i32, x1: i32, y1: i32) {
    if y0 == y1 {
        return;
    }
    // Always walk top to bottom; `dir` carries the winding sign.
    let (dir, x0, y0, x1, y1) = if y0 < y1 {
        (1i64, x0, y0, x1, y1)
    } else {
        (-1i64, x1, y1, x0, y0)
    };
    let top = y0.max(0);
    let bot = y1.min(h as i32 * FP);
    if top >= bot {
        return;
    }
    let dy_total = i64::from(y1 - y0);
    let dx_total = i64::from(x1 - x0);
    let x_at = |y: i32| -> i64 { i64::from(x0) + i64::from(y - y0) * dx_total / dy_total };
    let max_x = (stride as i64 - 2) * i64::from(FP);
    let fp = i64::from(FP);
    let mut row = top.div_euclid(FP);
    while i64::from(row) * fp < i64::from(bot) {
        let ya = top.max(row * FP);
        let yb = bot.min((row + 1) * FP);
        let d = dir * i64::from(yb - ya);
        let mut xa = x_at(ya).clamp(0, max_x);
        let mut xb = x_at(yb).clamp(0, max_x);
        if xa > xb {
            std::mem::swap(&mut xa, &mut xb);
        }
        let base = row as usize * stride;
        let c0 = xa.div_euclid(fp);
        let c1 = (xb + fp - 1).div_euclid(fp);
        let mut put = |cell: i64, share: i64, xm: i64| {
            let i = base + cell as usize;
            if let Some(v) = acc.get_mut(i) {
                *v += (share * (fp - xm)) as i32;
            }
            if let Some(v) = acc.get_mut(i + 1) {
                *v += (share * xm) as i32;
            }
        };
        if c1 <= c0 + 1 {
            put(c0, d, (xa + xb) / 2 - c0 * fp);
        } else {
            let dx = xb - xa;
            let mut left = d;
            for cell in c0..c1 {
                let lo = xa.max(cell * fp);
                let hi = xb.min((cell + 1) * fp);
                let share = if cell == c1 - 1 {
                    left
                } else {
                    d * (hi - lo) / dx
                };
                left -= share;
                put(cell, share, (lo + hi) / 2 - cell * fp);
            }
        }
        row += 1;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn square(x: f32, y: f32, s: f32, cw: bool) -> LinePath {
        let mut p = LinePath::default();
        p.move_to(x, y);
        if cw {
            p.line_to(x, y + s);
            p.line_to(x + s, y + s);
            p.line_to(x + s, y);
        } else {
            p.line_to(x + s, y);
            p.line_to(x + s, y + s);
            p.line_to(x, y + s);
        }
        p.close();
        p
    }

    #[test]
    fn aligned_square_is_fully_covered() {
        let (bm, l, t) = square(2.0, 3.0, 4.0, true)
            .rasterise(&mut Vec::new())
            .unwrap();
        assert_eq!((bm.w, bm.h), (4, 4));
        assert_eq!((l, t), (2, -7)); // y is flipped to y-down
        assert!(bm.data.iter().all(|&c| c == 255), "{:?}", bm.data);
    }

    #[test]
    fn half_pixel_edges_give_half_coverage() {
        let (bm, _, _) = square(0.5, 0.0, 2.0, true)
            .rasterise(&mut Vec::new())
            .unwrap();
        assert_eq!(bm.w, 3);
        assert_eq!(bm.row(0), [128, 255, 128]);
    }

    #[test]
    fn same_direction_overlap_saturates_opposite_cuts_hole() {
        let mut p = square(0.0, 0.0, 4.0, true);
        p.lines.extend(square(1.0, 1.0, 2.0, true).lines);
        let (bm, _, _) = p.rasterise(&mut Vec::new()).unwrap();
        assert!(bm.data.iter().all(|&c| c == 255));

        let mut p = square(0.0, 0.0, 4.0, true);
        p.lines.extend(square(1.0, 1.0, 2.0, false).lines);
        let (bm, _, _) = p.rasterise(&mut Vec::new()).unwrap();
        assert_eq!(bm.data[4 + 1], 0);
        assert_eq!(bm.data[0], 255);
    }

    #[test]
    fn curve_circle_area_is_close() {
        let mut p = LinePath::default();
        let k = 0.552_284_75 * 10.0;
        p.move_to(10.0, 0.0);
        p.curve_to(10.0, k, k, 10.0, 0.0, 10.0);
        p.curve_to(-k, 10.0, -10.0, k, -10.0, 0.0);
        p.curve_to(-10.0, -k, -k, -10.0, 0.0, -10.0);
        p.curve_to(k, -10.0, 10.0, -k, 10.0, 0.0);
        let (bm, _, _) = p.rasterise(&mut Vec::new()).unwrap();
        let area: u32 = bm.data.iter().map(|&c| u32::from(c)).sum();
        let expect = std::f32::consts::PI * 100.0 * 255.0;
        assert!(
            (area as f32 - expect).abs() / expect < 0.01,
            "{area} vs {expect}"
        );
    }

    #[test]
    fn raster_is_deterministic() {
        let mk = || {
            let mut p = LinePath::default();
            p.move_to(0.3, 0.1);
            p.quad_to(5.7, 9.9, 11.1, 0.2);
            p.line_to(5.5, -4.4);
            p.close();
            p.rasterise(&mut Vec::new()).unwrap().0
        };
        assert_eq!(mk(), mk());
    }
}
