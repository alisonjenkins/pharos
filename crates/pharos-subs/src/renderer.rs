//! The renderer: active-event tracking, per-event rasterisation, caches.

use std::sync::Arc;

use skrifa::font::glyph::outline::{DrawMemory, DrawOptions};
use skrifa::GlyphId;

use crate::cache::Lru;
use crate::fonts::{FontDb, FontError, FontId};
use crate::frame::{Config, Frame, Mask, MaskKind, RenderStats, SubtitleStyle, Viewport};
use crate::layout::{self, Line, RunFmt, Scratch};
use crate::model::{Event, Rgba, Script, Style};
use crate::morph::{dilate, rounded_rect};
use crate::raster::{Bitmap, LinePath};

/// Raster cap (spec: render at min(display, 1080p), upscale on the GPU).
const MAX_RENDER_W: u32 = 1920;
const MAX_RENDER_H: u32 = 1080;
/// Horizontal share of the picture plain text may fill.
const PLAIN_MAX_WIDTH: f32 = 0.9;
/// Plain text sits 5% in from each side.
const PLAIN_SIDE_MARGIN: f32 = 0.05;

/// Render-space geometry derived from a [`Viewport`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Geo {
    rw: u32,
    rh: u32,
    cx: i32,
    cy: i32,
    cw: u32,
    ch: u32,
    /// Lowest y subtitles may reach (picture bottom or above the controls).
    bottom: i32,
    /// Display-per-render pixel scale, as bits so Geo stays Hash.
    scale_bits: u32,
}

impl Geo {
    fn new(vp: &Viewport) -> Self {
        let w = vp.width.max(1);
        let h = vp.height.max(1);
        let s = (MAX_RENDER_W as f32 / w as f32)
            .min(MAX_RENDER_H as f32 / h as f32)
            .min(1.0);
        let sc = |v: i32| (v as f32 * s).round() as i32;
        let scu = |v: u32| ((v as f32 * s).round() as u32).max(1);
        let c = vp.content;
        let content_bottom = c.y.saturating_add(c.h as i32);
        let limit = content_bottom.min(vp.height as i32 - vp.bottom_inset as i32);
        // Never push subtitles above the picture's top quarter.
        let floor = c.y + (c.h / 4) as i32;
        Self {
            rw: scu(w),
            rh: scu(h),
            cx: sc(c.x),
            cy: sc(c.y),
            cw: scu(c.w),
            ch: scu(c.h),
            bottom: sc(limit.max(floor)),
            scale_bits: (1.0 / s).to_bits(),
        }
    }
}

struct Loaded {
    script: Script,
    /// Every distinct start/end time, sorted: the active set only changes
    /// when time crosses one of these.
    boundaries: Vec<i64>,
    max_dur: i64,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct FrameKey {
    script_gen: u64,
    style_gen: u64,
    geo: Geo,
    segment: usize,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct EvKey {
    event: u32,
    geo: Geo,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct GlyphKey {
    font: u32,
    gid: u32,
    /// Size in 1/64 px.
    size_q: u32,
    fake_italic: bool,
}

struct GlyphBmp {
    bm: Bitmap,
    left: i32,
    top: i32,
}

struct EventRaster {
    masks: Vec<Mask>,
}

/// Subtitle renderer. Feed it fonts and a [`Script`], then call
/// [`Renderer::frame`] once per video frame (or per timeupdate).
pub struct Renderer {
    db: FontDb,
    loaded: Option<Loaded>,
    style: SubtitleStyle,
    script_gen: u64,
    style_gen: u64,
    last: Option<FrameKey>,
    events: Lru<EvKey, EventRaster>,
    engine: Engine,
    stats: RenderStats,
}

struct Engine {
    glyphs: Lru<GlyphKey, GlyphBmp>,
    scratch: Scratch,
    raster_scratch: Vec<i32>,
    path: LinePath,
    next_id: u64,
    glyphs_rastered: u64,
    glyph_hits: u64,
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new(Config::default())
    }
}

impl Renderer {
    pub fn new(cfg: Config) -> Self {
        Self {
            db: FontDb::default(),
            loaded: None,
            style: SubtitleStyle::default(),
            script_gen: 0,
            style_gen: 0,
            last: None,
            events: Lru::new(cfg.event_cache_bytes),
            engine: Engine {
                glyphs: Lru::new(cfg.glyph_cache_bytes),
                scratch: Scratch::default(),
                raster_scratch: Vec::new(),
                path: LinePath::default(),
                next_id: 1,
                glyphs_rastered: 0,
                glyph_hits: 0,
            },
            stats: RenderStats::default(),
        }
    }

    /// Adds a font (TTF/OTF/TTC, face 0). The first font added is the
    /// default, used when a style names a family that was not supplied.
    pub fn add_font(&mut self, data: Vec<u8>) -> Result<FontId, FontError> {
        let id = self.db.add(data)?;
        // New font can change family resolution for cached events.
        self.events.clear();
        self.last = None;
        Ok(id)
    }

    /// Replaces the track. Cached rasters are dropped.
    pub fn load(&mut self, script: Script) {
        let mut boundaries: Vec<i64> = script
            .events
            .iter()
            .flat_map(|e| [e.start_ms, e.end_ms])
            .collect();
        boundaries.sort_unstable();
        boundaries.dedup();
        let max_dur = script
            .events
            .iter()
            .map(|e| e.end_ms - e.start_ms)
            .max()
            .unwrap_or(0);
        self.stats.unsupported_tags = script.stats.unsupported_tags.clone();
        self.loaded = Some(Loaded {
            script,
            boundaries,
            max_dur,
        });
        self.script_gen += 1;
        self.events.clear();
        self.last = None;
    }

    /// Changes the Pharos plain style; takes effect on the next frame.
    pub fn set_style(&mut self, style: SubtitleStyle) {
        if style != self.style {
            self.style = style;
            self.style_gen += 1;
            self.events.clear();
        }
    }

    pub fn stats(&self) -> RenderStats {
        let mut s = self.stats.clone();
        s.glyphs_rastered = self.engine.glyphs_rastered;
        s.glyph_cache_hits = self.engine.glyph_hits;
        s.event_cache_bytes = self.events.bytes();
        s.glyph_cache_bytes = self.engine.glyphs.bytes();
        s
    }

    /// Masks for time `t_ms`. When nothing changed since the previous call
    /// this does an O(log n) lookup and returns `changed == false`.
    pub fn frame(&mut self, t_ms: i64, vp: &Viewport) -> Frame {
        self.stats.frames += 1;
        let geo = Geo::new(vp);
        let segment = self
            .loaded
            .as_ref()
            .map_or(0, |l| l.boundaries.partition_point(|b| *b <= t_ms));
        let key = FrameKey {
            script_gen: self.script_gen,
            style_gen: self.style_gen,
            geo,
            segment,
        };
        let scale = f32::from_bits(geo.scale_bits);
        let render_size = (geo.rw, geo.rh);
        if self.last.as_ref() == Some(&key) {
            self.stats.idle_frames += 1;
            return Frame {
                changed: false,
                masks: Vec::new(),
                render_size,
                scale,
            };
        }
        self.last = Some(key);

        let mut masks = Vec::new();
        if let Some(l) = &self.loaded {
            let evs = &l.script.events;
            let hi = evs.partition_point(|e| e.start_ms <= t_ms);
            let mut active: Vec<usize> = Vec::new();
            for i in (0..hi).rev() {
                let Some(e) = evs.get(i) else { continue };
                if e.start_ms < t_ms - l.max_dur {
                    break;
                }
                if e.end_ms > t_ms {
                    active.push(i);
                }
            }
            active.sort_by_key(|&i| (evs.get(i).map_or(0, |e| e.layer), i));
            for i in active {
                let ek = EvKey {
                    event: i as u32,
                    geo,
                };
                let raster = match self.events.get(&ek) {
                    Some(r) => {
                        self.stats.event_cache_hits += 1;
                        r
                    }
                    None => {
                        self.stats.events_rastered += 1;
                        let r = Arc::new(self.engine.raster_event(
                            &self.db,
                            &l.script,
                            i,
                            &geo,
                            &self.style,
                        ));
                        let bytes: usize = r.masks.iter().map(|m| m.coverage.len()).sum();
                        self.events.insert(ek, Arc::clone(&r), bytes);
                        r
                    }
                };
                masks.extend(raster.masks.iter().cloned());
            }
        }
        Frame {
            changed: true,
            masks,
            render_size,
            scale,
        }
    }
}

struct Placement {
    bmp: Arc<GlyphBmp>,
    x: i32,
    y: i32,
    run: usize,
}

impl Engine {
    fn mask_from(&mut self, bm: Bitmap, x: i32, y: i32, tint: Rgba, kind: MaskKind) -> Mask {
        let id = self.next_id;
        self.next_id += 1;
        Mask {
            id,
            x,
            y,
            w: bm.w,
            h: bm.h,
            stride: bm.w,
            coverage: Arc::from(bm.data),
            tint,
            kind,
        }
    }

    fn glyph(
        &mut self,
        db: &FontDb,
        font: usize,
        gid: u32,
        size: f32,
        fake_italic: bool,
    ) -> Option<Arc<GlyphBmp>> {
        let size_q = (size * 64.0).round().max(1.0) as u32;
        let key = GlyphKey {
            font: font as u32,
            gid,
            size_q,
            fake_italic,
        };
        if let Some(g) = self.glyphs.get(&key) {
            self.glyph_hits += 1;
            return Some(g);
        }
        let entry = db.get(font)?;
        let outlines = entry.font.outline_glyphs();
        let glyph = outlines.get(GlyphId::new(gid))?;
        self.path.clear();
        self.path.shear = if fake_italic { 0.2 } else { 0.0 };
        let opts = DrawOptions::new().with_ppem(Some(size_q as f32 / 64.0));
        glyph
            .draw(&opts, DrawMemory::Scratch, &mut self.path)
            .ok()?;
        self.glyphs_rastered += 1;
        let (bm, left, top) = self.path.rasterise(&mut self.raster_scratch)?;
        let bytes = bm.bytes() + 64;
        let g = Arc::new(GlyphBmp { bm, left, top });
        self.glyphs.insert(key, Arc::clone(&g), bytes);
        Some(g)
    }

    fn raster_event(
        &mut self,
        db: &FontDb,
        script: &Script,
        idx: usize,
        geo: &Geo,
        sub: &SubtitleStyle,
    ) -> EventRaster {
        let mut out = EventRaster { masks: Vec::new() };
        let Some(ev) = script.events.get(idx) else {
            return out;
        };
        let fallback = Style::default();
        let style = script.styles.get(ev.style).or(script.styles.first());
        let style = style.unwrap_or(&fallback);
        let plain = script.plain;
        let ky = geo.ch as f32 / script.play_res_y.max(1) as f32;
        let kx = geo.cw as f32 / script.play_res_x.max(1) as f32;
        let runs = resolve_runs(db, ev, style, sub, plain, ky, geo.ch as f32);
        if runs.is_empty() {
            return out;
        }
        let lead_size = runs.first().map_or(0.0, |r| r.size);

        // Margins and alignment.
        let align = ev
            .alignment
            .unwrap_or(if plain { 2 } else { style.alignment });
        let (ml, mr, mv) = if plain {
            let side = geo.cw as f32 * PLAIN_SIDE_MARGIN;
            (side, side, sub.bottom_margin * geo.ch as f32)
        } else {
            let pick = |e: i32, s: i32| if e != 0 { e } else { s } as f32;
            (
                pick(ev.margin_l, style.margin_l) * kx,
                pick(ev.margin_r, style.margin_r) * kx,
                pick(ev.margin_v, style.margin_v) * ky,
            )
        };
        let pad_x = if plain {
            sub.padding_x_em * lead_size
        } else {
            0.0
        };
        let pad_y = if plain {
            sub.padding_y_em * lead_size
        } else {
            0.0
        };
        let area_w = if plain {
            (geo.cw as f32 * PLAIN_MAX_WIDTH).min(geo.cw as f32 - ml - mr)
        } else {
            geo.cw as f32 - ml - mr
        };
        let wrap_w = (area_w - 2.0 * pad_x).max(1.0);

        let lines = layout::layout(
            db,
            &mut self.scratch,
            &ev.text,
            &runs,
            script.wrap_style,
            wrap_w,
        );
        if lines.iter().all(|l| l.glyphs.is_empty()) {
            return out;
        }

        // Vertical stacking. Plain lines are separated by their backdrop
        // padding so the translucent boxes tile instead of overlapping.
        let gap = 2.0 * pad_y;
        let heights: Vec<f32> = lines.iter().map(|l| l.ascent + l.descent).collect();
        let block_h: f32 =
            heights.iter().sum::<f32>() + gap * (lines.len() as f32 - 1.0) + 2.0 * pad_y;
        let area_bottom = if plain {
            geo.bottom as f32
        } else {
            (geo.cy + geo.ch as i32) as f32
        };
        let row = (align - 1) / 3; // 0 bottom, 1 middle, 2 top
        let block_top = match row {
            0 => area_bottom - mv - block_h,
            2 => geo.cy as f32 + mv,
            _ => geo.cy as f32 + (geo.ch as f32 - block_h) / 2.0,
        };
        let col = (align - 1) % 3; // 0 left, 1 centre, 2 right
        let area_left = geo.cx as f32 + ml;

        let mut placed: Vec<Placement> = Vec::new();
        let mut backdrops: Vec<(Bitmap, i32, i32)> = Vec::new();
        let mut top = block_top + pad_y;
        for (line, h) in lines.iter().zip(&heights) {
            let x0 = match col {
                0 => area_left,
                2 => area_left + area_w - line.width,
                _ => area_left + (area_w - line.width) / 2.0,
            };
            let baseline = top + line.ascent;
            self.place_line(db, &runs, line, x0, baseline, &mut placed);
            if plain && line.width > 0.0 {
                let bx = (x0 - pad_x).round() as i32;
                let by = (top - pad_y).round() as i32;
                let bw = (line.width + 2.0 * pad_x).round().max(1.0) as u32;
                let bh = (h + 2.0 * pad_y).round().max(1.0) as u32;
                let r = (sub.corner_radius_em * lead_size).round() as u32;
                backdrops.push((rounded_rect(bw, bh, r), bx, by));
            }
            top += h + gap;
        }

        for (bm, x, y) in backdrops {
            let m = self.mask_from(bm, x, y, sub.backdrop, MaskKind::Backdrop);
            out.masks.push(m);
        }
        self.compose(&runs, &placed, &mut out);
        out
    }

    fn place_line(
        &mut self,
        db: &FontDb,
        runs: &[RunFmt],
        line: &Line,
        x0: f32,
        baseline: f32,
        placed: &mut Vec<Placement>,
    ) {
        let mut pen = x0;
        let mut span: Option<(usize, f32)> = None;
        let flush = |placed: &mut Vec<Placement>, span: Option<(usize, f32)>, end: f32| {
            let Some((ri, start)) = span else { return };
            let Some(r) = runs.get(ri) else { return };
            let thick = (r.size / 18.0).round().max(1.0) as i32;
            let x = start.round() as i32;
            let w = (end.round() as i32 - x).max(0);
            for (on, dy) in [(r.underline, r.size * 0.1), (r.strike, -r.size * 0.28)] {
                if on && w > 0 {
                    let mut bm = Bitmap::new(w as u32, thick as u32);
                    bm.fill_rect(0, 0, w, thick);
                    placed.push(Placement {
                        bmp: Arc::new(GlyphBmp {
                            bm,
                            left: 0,
                            top: 0,
                        }),
                        x,
                        y: (baseline + dy).round() as i32,
                        run: ri,
                    });
                }
            }
        };
        for g in &line.glyphs {
            let ri = g.run as usize;
            if span.map(|s| s.0) != Some(ri) {
                flush(placed, span, pen);
                span = Some((ri, pen));
            }
            if let Some(r) = runs.get(ri) {
                if g.gid != 0 && !g.space {
                    if let Some(b) = self.glyph(db, r.font, g.gid, r.size, r.italic) {
                        if !b.bm.is_empty() {
                            let gx = (pen + g.xo).round() as i32;
                            let gy = (baseline - g.yo).round() as i32;
                            placed.push(Placement {
                                x: gx + b.left,
                                y: gy + b.top,
                                bmp: b,
                                run: ri,
                            });
                        }
                    }
                }
            }
            pen += g.adv;
        }
        flush(placed, span, pen);
    }

    /// Groups placements by colour and builds shadow, border and fill masks.
    fn compose(&mut self, runs: &[RunFmt], placed: &[Placement], out: &mut EventRaster) {
        // Fill groups: placements sharing the primary tint.
        let mut fills: Vec<(Rgba, Vec<usize>)> = Vec::new();
        // Border groups: (outline tint, shadow tint, border px, shadow px).
        type BKey = (Rgba, Rgba, u32, i32);
        let mut borders: Vec<(BKey, Vec<usize>)> = Vec::new();
        for (i, p) in placed.iter().enumerate() {
            let Some(r) = runs.get(p.run) else { continue };
            match fills.iter_mut().find(|(t, _)| *t == r.colours[0]) {
                Some((_, v)) => v.push(i),
                None => fills.push((r.colours[0], vec![i])),
            }
            if r.bord > 0 || r.shad > 0 {
                let k: BKey = (r.colours[2], r.colours[3], r.bord, r.shad);
                match borders.iter_mut().find(|(bk, _)| *bk == k) {
                    Some((_, v)) => v.push(i),
                    None => borders.push((k, vec![i])),
                }
            }
        }

        let mut unions: Vec<(Vec<usize>, Arc<Union>)> = Vec::new();
        let mut union_of = |idxs: &Vec<usize>| -> Arc<Union> {
            if let Some((_, u)) = unions.iter().find(|(k, _)| k == idxs) {
                return Arc::clone(u);
            }
            let u = Arc::new(union_bitmap(placed, idxs));
            unions.push((idxs.clone(), Arc::clone(&u)));
            u
        };

        for ((outline, back, bord, shad), idxs) in &borders {
            let u = union_of(idxs);
            let (bm, x, y) = if *bord > 0 {
                (dilate(&u.0, *bord), u.1 - *bord as i32, u.2 - *bord as i32)
            } else {
                (u.0.clone(), u.1, u.2)
            };
            let (w, h) = (bm.w, bm.h);
            let id = self.next_id;
            self.next_id += 1;
            let cov: Arc<[u8]> = Arc::from(bm.data);
            let mk = |x: i32, y: i32, tint: Rgba, kind: MaskKind| Mask {
                id,
                x,
                y,
                w,
                h,
                stride: w,
                coverage: Arc::clone(&cov),
                tint,
                kind,
            };
            if *shad > 0 && back[3] > 0 {
                out.masks
                    .push(mk(x + shad, y + shad, *back, MaskKind::Shadow));
            }
            if *bord > 0 && outline[3] > 0 {
                out.masks.push(mk(x, y, *outline, MaskKind::Border));
            }
        }
        for (tint, idxs) in &fills {
            if tint[3] == 0 {
                continue;
            }
            let u = union_of(idxs);
            let m = self.mask_from(u.0.clone(), u.1, u.2, *tint, MaskKind::Fill);
            out.masks.push(m);
        }
    }
}

/// Bitmap plus its top-left position.
type Union = (Bitmap, i32, i32);

fn union_bitmap(placed: &[Placement], idxs: &[usize]) -> (Bitmap, i32, i32) {
    let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
    for &i in idxs {
        if let Some(p) = placed.get(i) {
            x0 = x0.min(p.x);
            y0 = y0.min(p.y);
            x1 = x1.max(p.x + p.bmp.bm.w as i32);
            y1 = y1.max(p.y + p.bmp.bm.h as i32);
        }
    }
    if x1 <= x0 || y1 <= y0 {
        return (Bitmap::default(), 0, 0);
    }
    let mut bm = Bitmap::new((x1 - x0) as u32, (y1 - y0) as u32);
    for &i in idxs {
        if let Some(p) = placed.get(i) {
            bm.blit_max(p.x - x0, p.y - y0, &p.bmp.bm);
        }
    }
    (bm, x0, y0)
}

fn resolve_runs(
    db: &FontDb,
    ev: &Event,
    style: &Style,
    sub: &SubtitleStyle,
    plain: bool,
    ky: f32,
    ch: f32,
) -> Vec<RunFmt> {
    let mut out = Vec::with_capacity(ev.runs.len());
    for run in &ev.runs {
        let ov = &run.ov;
        let family: &str = match (&ov.font, plain && !sub.font.is_empty()) {
            (Some(f), _) => f,
            (None, true) => &sub.font,
            (None, false) => &style.font,
        };
        let bold = ov.bold.unwrap_or(style.bold);
        let italic = ov.italic.unwrap_or(style.italic);
        let Some(fi) = db.find(family, bold, italic) else {
            return Vec::new();
        };
        let Some(entry) = db.get(fi) else {
            return Vec::new();
        };
        // The scale from the script is in "cell height" units like VSFilter:
        // ascent + descent equals the font size.
        let size = if plain {
            sub.base_size * sub.size_scale * ch
        } else {
            let cell = (entry.ascent - entry.descent).max(1.0);
            ov.size.unwrap_or(style.size) * ky * entry.upem / cell
        };
        let mut colours = [[0u8; 4]; 4];
        for (i, c) in colours.iter_mut().enumerate() {
            let base = if plain && i == 0 {
                sub.text
            } else {
                style.colours.get(i).copied().unwrap_or([255; 4])
            };
            let rgb = ov
                .rgb
                .get(i)
                .copied()
                .flatten()
                .unwrap_or([base[0], base[1], base[2]]);
            let a = ov.alpha.get(i).copied().flatten().unwrap_or(base[3]);
            *c = [rgb[0], rgb[1], rgb[2], a];
        }
        let bord = ov.bord.unwrap_or(if plain { 0.0 } else { style.outline });
        let shad = ov.shad.unwrap_or(if plain { 0.0 } else { style.shadow });
        let k = if plain { 1.0 } else { ky };
        out.push(RunFmt {
            end: run.end,
            font: fi,
            size,
            italic: italic && !entry.italic,
            underline: ov.underline.unwrap_or(style.underline),
            strike: ov.strike.unwrap_or(style.strike),
            colours,
            bord: (bord * k).round().max(0.0) as u32,
            shad: (shad * k).round().max(0.0) as i32,
        });
    }
    out
}
