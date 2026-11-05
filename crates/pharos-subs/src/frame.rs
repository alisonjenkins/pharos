//! Public output and configuration types.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::model::Rgba;

/// What a mask is for; the compositor tints it with [`Mask::tint`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MaskKind {
    Backdrop,
    Shadow,
    Border,
    Fill,
}

/// An 8-bit coverage bitmap with its placement and colour; upload as an R8
/// texture and tint in the shader. Positions are in render pixels (see
/// [`Frame::render_size`]).
#[derive(Clone, Debug)]
pub struct Mask {
    /// Stable per distinct bitmap: a border and its shadow share an `id`, and
    /// a bitmap keeps its `id` for as long as it stays cached, so a
    /// compositor can keep one texture per `id`.
    pub id: u64,
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    /// Bytes per row; equals `w`.
    pub stride: u32,
    pub coverage: Arc<[u8]>,
    /// Straight (non-premultiplied) RGBA.
    pub tint: Rgba,
    pub kind: MaskKind,
}

/// One frame of output.
#[derive(Clone, Debug)]
pub struct Frame {
    /// False when the active event set, viewport and style are the same as
    /// the previous call: `masks` is then empty and the compositor keeps what
    /// it drew. True means `masks` is the complete picture (possibly empty).
    pub changed: bool,
    /// Draw order, back to front.
    pub masks: Vec<Mask>,
    /// Size the masks are positioned in: `min(display, 1920x1080)`.
    pub render_size: (u32, u32),
    /// Display pixels per render pixel; the GPU scales quads by this.
    pub scale: f32,
}

/// A pixel rectangle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// Where subtitles may be drawn, in display pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
    /// The displayed picture (excludes letterbox bars).
    pub content: Rect,
    /// Height of UI (the control bar) covering the bottom of the viewport.
    pub bottom_inset: u32,
}

impl Viewport {
    /// Picture fills the viewport, no controls showing.
    pub fn full(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            content: Rect {
                x: 0,
                y: 0,
                w: width,
                h: height,
            },
            bottom_inset: 0,
        }
    }
}

/// The Pharos look for plain tracks (FR-009).
#[derive(Clone, Debug, PartialEq)]
pub struct SubtitleStyle {
    pub text: Rgba,
    pub backdrop: Rgba,
    /// Backdrop corner radius in em.
    pub corner_radius_em: f32,
    pub padding_x_em: f32,
    pub padding_y_em: f32,
    /// Gap from the bottom of the picture, as a fraction of picture height.
    pub bottom_margin: f32,
    /// 0.75, 1.0, 1.25 or 1.5 (Small to Extra large).
    pub size_scale: f32,
    /// Text height at scale 1.0, as a fraction of picture height.
    pub base_size: f32,
    /// Preferred family for plain text; empty uses the default font.
    pub font: String,
}

impl Default for SubtitleStyle {
    fn default() -> Self {
        Self {
            text: [255, 255, 255, 255],
            backdrop: [0, 0, 0, 140],
            corner_radius_em: 0.45,
            padding_x_em: 0.5,
            padding_y_em: 0.12,
            bottom_margin: 0.06,
            size_scale: 1.0,
            base_size: 0.046,
            font: String::new(),
        }
    }
}

/// Cache sizes. Defaults keep the renderer well under the 48 MB budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    pub event_cache_bytes: usize,
    pub glyph_cache_bytes: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            event_cache_bytes: 16 << 20,
            glyph_cache_bytes: 8 << 20,
        }
    }
}

/// Counters for the observability channel.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RenderStats {
    pub frames: u64,
    /// Frames answered with `changed == false` (no work done).
    pub idle_frames: u64,
    pub events_rastered: u64,
    pub event_cache_hits: u64,
    pub glyphs_rastered: u64,
    pub glyph_cache_hits: u64,
    pub event_cache_bytes: usize,
    pub glyph_cache_bytes: usize,
    /// Override tags seen but not drawn, by name.
    pub unsupported_tags: BTreeMap<String, u32>,
}
