//! The GPU layer (009 FR-005, ADR-0039): one allow-listed fragment shader
//! filling a `<canvas>`, behind the [`Surface`] port so WebGPU can replace
//! WebGL2 later. Without WebGL2, or with reduced motion, nothing is drawn and
//! the page keeps its CSS look; the reason is returned for the log.

pub mod catalogue;
#[cfg(all(feature = "web", target_arch = "wasm32"))]
mod webgl;

pub use catalogue::Program;

use crate::theme::value::Rgba;

/// Texels per row of the data texture a shader samples.
pub const DATA_WIDTH: usize = 256;
/// Rows of the data texture: 0 is the spectrum, 1 the waveform.
pub const DATA_ROWS: usize = 2;
/// `u_time` wraps here so a long session keeps float precision in shaders.
const TIME_WRAP_S: f64 = 3600.0;

/// A drawing surface for one shader program.
pub trait Surface {
    /// Draw one frame, sizing the canvas to its box first.
    fn draw(&mut self, frame: &Frame<'_>) -> Result<(), Fallback>;
}

/// What a shader gets for one frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frame<'a> {
    pub time_s: f32,
    pub palette: Palette,
    /// Overall loudness, 0 to 1.
    pub level: f32,
    /// `DATA_WIDTH * DATA_ROWS` bytes, row by row; `None` draws silence.
    pub data: Option<&'a [u8]>,
}

impl Frame<'_> {
    /// Seconds since `start_ms`, wrapped to keep shader maths precise.
    pub fn clock(now_ms: f64, start_ms: f64) -> f32 {
        (((now_ms - start_ms) / 1000.0).max(0.0) % TIME_WRAP_S) as f32
    }
}

/// The theme colours a shader tints with, as linear 0 to 1 RGB.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    pub bg: [f32; 3],
    pub accent: [f32; 3],
    pub fg: [f32; 3],
}

impl Default for Palette {
    /// `pharos.css`'s own defaults, for a page whose tokens cannot be read.
    fn default() -> Self {
        Self {
            bg: rgb(Rgba::parse("#0e1116")),
            accent: rgb(Rgba::parse("#4cb4ff")),
            fg: rgb(Rgba::parse("#e6e8eb")),
        }
    }
}

impl Palette {
    /// From the page's `--px-color-*` values; a role that does not parse
    /// keeps the default.
    pub fn from_tokens(bg: &str, accent: &str, fg: &str) -> Self {
        let fallback = Self::default();
        let pick = |css: &str, or: [f32; 3]| match Rgba::parse(css.trim()) {
            Some(c) => rgb(Some(c)),
            None => or,
        };
        Self {
            bg: pick(bg, fallback.bg),
            accent: pick(accent, fallback.accent),
            fg: pick(fg, fallback.fg),
        }
    }
}

fn rgb(c: Option<Rgba>) -> [f32; 3] {
    let c = c.unwrap_or(Rgba {
        r: 0,
        g: 0,
        b: 0,
        a: 255,
    });
    [c.r, c.g, c.b].map(|v| f32::from(v) / 255.0)
}

/// Why the GPU layer is off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fallback {
    /// The user or the system asked for reduced motion.
    ReducedMotion,
    /// No WebGL2 context, or no browser at all.
    NoWebgl2,
    /// The GPU dropped the context; the page does not try to restore it.
    ContextLost,
    /// A catalogue shader failed to compile or link on this GPU.
    Shader(String),
    /// The canvas the surface draws on is not in the page.
    NoCanvas,
}

impl Fallback {
    /// The `reason` field of the renderer-mode log line (016 FR-011).
    pub fn label(&self) -> &'static str {
        match self {
            Fallback::ReducedMotion => "reduced_motion",
            Fallback::NoWebgl2 => "no_webgl2",
            Fallback::ContextLost => "context_lost",
            Fallback::Shader(_) => "shader",
            Fallback::NoCanvas => "no_canvas",
        }
    }
}

/// A surface drawing `program` on the canvas with id `canvas_id`, or why
/// there is none.
pub fn open(
    canvas_id: &str,
    program: Program,
    reduced_motion: bool,
) -> Result<Box<dyn Surface>, Fallback> {
    if reduced_motion {
        return Err(Fallback::ReducedMotion);
    }
    #[cfg(all(feature = "web", target_arch = "wasm32"))]
    {
        webgl::open(canvas_id, program)
    }
    #[cfg(not(all(feature = "web", target_arch = "wasm32")))]
    {
        let _ = (canvas_id, program);
        Err(Fallback::NoWebgl2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduced_motion_wins_before_any_gpu_is_asked() {
        assert_eq!(
            open("c", Program::Spectrum, true).err(),
            Some(Fallback::ReducedMotion)
        );
    }

    #[test]
    fn the_host_has_no_webgl2() {
        assert_eq!(
            open("c", Program::Spectrum, false).err(),
            Some(Fallback::NoWebgl2)
        );
    }

    #[test]
    fn tokens_become_unit_rgb_and_bad_ones_keep_the_default() {
        let p = Palette::from_tokens(" #ff0000 ", "nonsense", "#00ff00");
        assert_eq!(p.bg, [1.0, 0.0, 0.0]);
        assert_eq!(p.accent, Palette::default().accent);
        assert_eq!(p.fg, [0.0, 1.0, 0.0]);
    }

    #[test]
    fn the_clock_wraps_and_never_runs_backwards() {
        assert_eq!(Frame::clock(1500.0, 500.0), 1.0);
        assert_eq!(Frame::clock(0.0, 500.0), 0.0);
        assert!(Frame::clock(TIME_WRAP_S * 1000.0 + 2000.0, 0.0) < 3.0);
    }

    #[test]
    fn fallback_labels_are_distinct() {
        let all = [
            Fallback::ReducedMotion,
            Fallback::NoWebgl2,
            Fallback::ContextLost,
            Fallback::Shader(String::new()),
            Fallback::NoCanvas,
        ];
        let labels: std::collections::HashSet<_> = all.iter().map(Fallback::label).collect();
        assert_eq!(labels.len(), all.len());
    }
}
