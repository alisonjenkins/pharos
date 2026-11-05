//! Internal script model shared by every input format.
//!
//! SRT and WebVTT are converted into the same model as ASS so the layout and
//! raster stages only ever see one shape of input.

use std::collections::BTreeMap;
use std::sync::Arc;

/// RGBA, alpha 255 = opaque (the renderer's convention, not ASS's inverted one).
pub type Rgba = [u8; 4];

/// Which container format a [`Script`] was parsed from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Srt,
    WebVtt,
    Ass,
    Ssa,
}

/// A named ASS style.
#[derive(Clone, Debug, PartialEq)]
pub struct Style {
    pub name: String,
    pub font: String,
    /// Font size in script (PlayRes) units.
    pub size: f32,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    /// Primary, secondary, outline and back colours.
    pub colours: [Rgba; 4],
    /// Border width in script units.
    pub outline: f32,
    /// Shadow distance in script units.
    pub shadow: f32,
    /// Numpad alignment 1-9.
    pub alignment: u8,
    pub margin_l: i32,
    pub margin_r: i32,
    pub margin_v: i32,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            name: "Default".into(),
            font: "Arial".into(),
            size: 48.0,
            bold: false,
            italic: false,
            underline: false,
            strike: false,
            colours: [
                [255, 255, 255, 255],
                [255, 0, 0, 255],
                [0, 0, 0, 255],
                [0, 0, 0, 255],
            ],
            outline: 2.0,
            shadow: 2.0,
            alignment: 2,
            margin_l: 10,
            margin_r: 10,
            margin_v: 10,
        }
    }
}

/// Inline override tags in effect for one run of text. `None` means "use the
/// style's value".
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Overrides {
    pub font: Option<Arc<str>>,
    pub size: Option<f32>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub underline: Option<bool>,
    pub strike: Option<bool>,
    /// Colour override per slot (primary, secondary, outline, back).
    pub rgb: [Option<[u8; 3]>; 4],
    /// Alpha override per slot, 255 = opaque.
    pub alpha: [Option<u8>; 4],
    pub bord: Option<f32>,
    pub shad: Option<f32>,
}

/// `text[prev.end..end]` is drawn with `ov`.
#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    pub end: usize,
    pub ov: Overrides,
}

/// One timed subtitle event.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub start_ms: i64,
    pub end_ms: i64,
    pub layer: i32,
    /// Index into [`Script::styles`].
    pub style: usize,
    /// Per-event margins, 0 = use the style's.
    pub margin_l: i32,
    pub margin_r: i32,
    pub margin_v: i32,
    /// `\an` override.
    pub alignment: Option<u8>,
    /// Visible text; `'\n'` is a hard line break, `'\u{a0}'` a non-breaking space.
    pub text: String,
    pub runs: Vec<Run>,
}

/// Counters for things the parser could not honour.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParseStats {
    /// Override tags outside the plain tier, by tag name, with occurrence counts.
    pub unsupported_tags: BTreeMap<String, u32>,
    /// Lines that looked like events or styles but could not be parsed.
    pub bad_lines: u32,
}

impl ParseStats {
    pub(crate) fn unsupported(&mut self, tag: &str) {
        *self.unsupported_tags.entry(tag.to_owned()).or_insert(0) += 1;
    }
}

/// A parsed subtitle track.
#[derive(Clone, Debug)]
pub struct Script {
    pub format: Format,
    /// Drawn with the Pharos plain style (`SubtitleStyle`) rather than the
    /// script's own styles. True for SRT and WebVTT; callers may set it for
    /// ASS tracks that are really plain text.
    pub plain: bool,
    pub play_res_x: u32,
    pub play_res_y: u32,
    /// ASS `WrapStyle` 0-3.
    pub wrap_style: u8,
    pub scaled_border_and_shadow: bool,
    pub styles: Vec<Style>,
    /// Sorted by start time.
    pub events: Vec<Event>,
    pub stats: ParseStats,
}

impl Script {
    pub(crate) fn empty(format: Format) -> Self {
        Self {
            format,
            plain: matches!(format, Format::Srt | Format::WebVtt),
            play_res_x: 1920,
            play_res_y: 1080,
            wrap_style: 0,
            scaled_border_and_shadow: true,
            styles: vec![Style::default()],
            events: Vec::new(),
            stats: ParseStats::default(),
        }
    }

    pub(crate) fn finish(&mut self) {
        // Stable so same-start events keep file order.
        self.events.sort_by_key(|e| e.start_ms);
    }
}
