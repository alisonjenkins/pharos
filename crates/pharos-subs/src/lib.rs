//! Pure-Rust subtitle renderer: parses SRT, WebVTT and ASS/SSA, lays text
//! out, rasterises it to 8-bit alpha masks and hands those to a compositor.
//!
//! No C dependencies and no `web-sys`; builds for the host and
//! `wasm32-unknown-unknown`.

#![forbid(unsafe_code)]
