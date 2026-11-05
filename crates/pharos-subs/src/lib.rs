//! Pure-Rust subtitle renderer: parses SRT, WebVTT and ASS/SSA, lays text
//! out, rasterises it to 8-bit alpha masks and hands those to a compositor.
//!
//! No C dependencies and no `web-sys`; builds for the host and
//! `wasm32-unknown-unknown`.

#![forbid(unsafe_code)]

mod cache;
mod fonts;
mod frame;
mod layout;
mod model;
mod morph;
mod parse;
mod raster;
mod renderer;

pub use fonts::{FontError, FontId};
pub use frame::{Config, Frame, Mask, MaskKind, Rect, RenderStats, SubtitleStyle, Viewport};
pub use model::{Event, Format, Overrides, ParseStats, Rgba, Run, Script, Style};
pub use parse::decode;
pub use renderer::Renderer;
