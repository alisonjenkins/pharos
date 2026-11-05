//! Shaping, bidi, line breaking and wrapping. Float math lives here (layout
//! only); glyph positions are rounded to whole pixels before rasterising.

use harfrust::{Buffer, Direction, ShapeOptions, ShaperFont};
use unicode_bidi::{BidiInfo, Level};

use crate::fonts::FontDb;
use crate::model::Rgba;

/// A run with every style/override resolved to concrete values, in render
/// pixels.
#[derive(Clone, Debug)]
pub(crate) struct RunFmt {
    /// Byte offset in the event text where this run ends.
    pub end: usize,
    pub font: usize,
    /// Em size in pixels.
    pub size: f32,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    /// Fill, secondary, border, shadow.
    pub colours: [Rgba; 4],
    pub bord: u32,
    pub shad: i32,
}

#[derive(Clone, Debug)]
pub(crate) struct Glyph {
    pub gid: u32,
    pub run: u32,
    pub level: u8,
    pub byte: u32,
    pub adv: f32,
    pub xo: f32,
    pub yo: f32,
    pub space: bool,
    /// A line may break after this glyph.
    pub brk: bool,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Line {
    /// Visual order.
    pub glyphs: Vec<Glyph>,
    /// Advance width without trailing spaces.
    pub width: f32,
    pub ascent: f32,
    pub descent: f32,
}

/// Reusable buffers so a cue change allocates little.
#[derive(Default)]
pub(crate) struct Scratch {
    buf: Option<Buffer>,
}

fn run_at(runs: &[RunFmt], byte: usize) -> usize {
    runs.partition_point(|r| r.end <= byte)
        .min(runs.len().saturating_sub(1))
}

fn is_rtl_script(c: char) -> bool {
    matches!(c as u32, 0x0590..=0x08FF | 0xFB1D..=0xFDFF | 0xFE70..=0xFEFF | 0x10800..=0x10FFF | 0x1E800..=0x1EFFF)
}

/// Lays `text` out into wrapped lines. `max_w` is the available width.
pub(crate) fn layout(
    db: &FontDb,
    scratch: &mut Scratch,
    text: &str,
    runs: &[RunFmt],
    wrap_style: u8,
    max_w: f32,
) -> Vec<Line> {
    let mut lines = Vec::new();
    if runs.is_empty() {
        return lines;
    }
    let mut start = 0usize;
    for para in text.split('\n') {
        let glyphs = shape_paragraph(db, scratch, text, start, para, runs);
        let ranges = wrap(&glyphs, wrap_style, max_w);
        if ranges.is_empty() {
            // Empty line: carries the height of the run it sits in.
            let r = runs.get(run_at(runs, start)).or_else(|| runs.last());
            let mut line = Line::default();
            if let Some((a, d)) = r.and_then(|r| metrics(db, r)) {
                line.ascent = a;
                line.descent = d;
            }
            lines.push(line);
        }
        for (s, e) in ranges {
            lines.push(build_line(db, runs, glyphs.get(s..e).unwrap_or(&[])));
        }
        start += para.len() + 1;
    }
    lines
}

fn metrics(db: &FontDb, r: &RunFmt) -> Option<(f32, f32)> {
    let f = db.get(r.font)?;
    let k = r.size / f.upem;
    Some((f.ascent * k, -f.descent * k))
}

fn build_line(db: &FontDb, runs: &[RunFmt], logical: &[Glyph]) -> Line {
    let mut glyphs: Vec<Glyph> = logical.to_vec();
    if glyphs.iter().any(|g| g.level != 0) {
        let levels: Vec<Level> = glyphs
            .iter()
            .map(|g| Level::new(g.level).unwrap_or_else(|_| Level::ltr()))
            .collect();
        let order = BidiInfo::reorder_visual(&levels);
        glyphs = order
            .into_iter()
            .filter_map(|i| logical.get(i).cloned())
            .collect();
    }
    let trailing = glyphs.iter().rev().take_while(|g| g.space).count();
    let width: f32 = glyphs
        .iter()
        .take(glyphs.len() - trailing)
        .map(|g| g.adv)
        .sum();
    let (mut asc, mut desc) = (0f32, 0f32);
    let mut last_run = u32::MAX;
    for g in &glyphs {
        if g.run != last_run {
            last_run = g.run;
            if let Some((a, d)) = runs.get(g.run as usize).and_then(|r| metrics(db, r)) {
                asc = asc.max(a);
                desc = desc.max(d);
            }
        }
    }
    Line {
        glyphs,
        width,
        ascent: asc,
        descent: desc,
    }
}

fn shape_paragraph(
    db: &FontDb,
    scratch: &mut Scratch,
    full: &str,
    para_start: usize,
    para: &str,
    runs: &[RunFmt],
) -> Vec<Glyph> {
    let mut out: Vec<Glyph> = Vec::with_capacity(para.len());
    if para.is_empty() {
        return out;
    }
    let levels: Option<Vec<Level>> = para.chars().any(is_rtl_script).then(|| {
        let info = BidiInfo::new(para, None);
        info.levels
    });
    let level_at = |off: usize| -> u8 {
        levels
            .as_ref()
            .and_then(|l| l.get(off))
            .map_or(0, |l| l.number())
    };

    let mut pos = 0usize;
    while pos < para.len() {
        let abs = para_start + pos;
        let ri = run_at(runs, abs);
        let Some(run) = runs.get(ri) else { break };
        let lvl = level_at(pos);
        let mut end = para
            .len()
            .min(run.end.saturating_sub(para_start))
            .max(pos + 1);
        if levels.is_some() {
            // Split at the next level change, on a char boundary.
            for (off, _) in para.get(pos..end).unwrap_or("").char_indices().skip(1) {
                if level_at(pos + off) != lvl {
                    end = pos + off;
                    break;
                }
            }
        }
        while !para.is_char_boundary(end) && end < para.len() {
            end += 1;
        }
        let piece = para.get(pos..end).unwrap_or("");
        let before = out.len();
        shape_piece(db, scratch, run, ri as u32, piece, lvl, abs, &mut out);
        // Harfrust returns RTL pieces in visual order; keep paragraphs logical.
        if lvl % 2 == 1 {
            if let Some(tail) = out.get_mut(before..) {
                tail.reverse();
            }
        }
        pos = end;
    }
    // Break opportunities, from the paragraph text.
    let mut allowed = vec![false; para.len() + 1];
    for (idx, _) in unicode_linebreak::linebreaks(para) {
        if let Some(a) = allowed.get_mut(idx) {
            *a = true;
        }
    }
    let n = out.len();
    for i in 0..n {
        let next = out.get(i + 1).map(|g| g.byte as usize - para_start);
        let this = out.get(i).map_or(0, |g| g.byte as usize - para_start);
        let brk = match next {
            Some(nb) => nb != this && allowed.get(nb).copied().unwrap_or(false),
            None => false,
        };
        if let Some(g) = out.get_mut(i) {
            g.brk = brk;
            g.space = full
                .get(g.byte as usize..)
                .and_then(|s| s.chars().next())
                .is_some_and(|c| c == ' ');
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn shape_piece(
    db: &FontDb,
    scratch: &mut Scratch,
    run: &RunFmt,
    run_idx: u32,
    piece: &str,
    level: u8,
    abs: usize,
    out: &mut Vec<Glyph>,
) {
    let Some(entry) = db.get(run.font) else {
        return;
    };
    let sf = ShaperFont::new(&entry.font);
    let mut buf = scratch.buf.take().unwrap_or_default();
    buf.clear();
    buf.push_str(piece);
    buf.set_direction(if level % 2 == 1 {
        Direction::RightToLeft
    } else {
        Direction::LeftToRight
    });
    buf.guess_segment_properties();
    if harfrust::shape(&sf, &mut buf, ShapeOptions::new()).is_ok() {
        let k = run.size / entry.upem;
        for (info, pos) in buf.glyph_infos().iter().zip(buf.glyph_positions()) {
            let byte = abs + info.cluster as usize;
            out.push(Glyph {
                gid: info.glyph_id,
                run: run_idx,
                level,
                byte: byte as u32,
                adv: pos.x_advance as f32 * k,
                xo: pos.x_offset as f32 * k,
                yo: pos.y_offset as f32 * k,
                space: false,
                brk: false,
            });
        }
    }
    scratch.buf = Some(buf);
}

/// Greedy line starts for `max` width; ends are the next start / len.
fn greedy(g: &[Glyph], max: f32) -> Vec<usize> {
    let mut starts = vec![0usize];
    let mut start = 0usize;
    let mut i = 0usize;
    let mut x = 0f32;
    let mut vis = 0f32;
    let mut last_brk: Option<usize> = None;
    while i < g.len() {
        let Some(gl) = g.get(i) else { break };
        x += gl.adv;
        if !gl.space {
            vis = x;
        }
        if vis > max && i > start {
            let b = match last_brk {
                Some(b) if b > start => b,
                _ => i,
            };
            starts.push(b);
            start = b;
            i = b;
            x = 0.0;
            vis = 0.0;
            last_brk = None;
            continue;
        }
        if gl.brk {
            last_brk = Some(i + 1);
        }
        i += 1;
    }
    starts
}

/// Splits glyphs into line ranges per `WrapStyle`.
fn wrap(g: &[Glyph], wrap_style: u8, max_w: f32) -> Vec<(usize, usize)> {
    if g.is_empty() {
        return Vec::new();
    }
    let mut starts = if wrap_style == 2 || max_w <= 0.0 {
        vec![0]
    } else {
        greedy(g, max_w)
    };
    if starts.len() > 1 && wrap_style != 1 {
        // Smart wrapping: narrowest width that keeps the line count, so
        // lines come out even instead of one long and one stub.
        let n = starts.len();
        let (mut lo, mut hi) = (max_w / n as f32, max_w);
        for _ in 0..10 {
            let mid = (lo + hi) / 2.0;
            if greedy(g, mid).len() == n {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        starts = greedy(g, hi);
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &s) in starts.iter().enumerate() {
        let e = starts.get(k + 1).copied().unwrap_or(g.len());
        out.push((s, e));
    }
    out
}
