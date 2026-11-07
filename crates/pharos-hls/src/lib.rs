//! Pure-Rust HLS playlist parser, timeline and buffer planner (ADR-0071).
//!
//! No browser, `wasm-bindgen` or async-runtime types; builds for the host and
//! `wasm32-unknown-unknown`.

#![forbid(unsafe_code)]

pub mod uri;
