//! Audio-fingerprint intro/outro detection (ADR-0018, T86).
//!
//! - [`align`] — pure fingerprint comparison (ported from intro-skipper's
//!   `ChromaprintAnalyzer`); no ffmpeg, fully unit-tested.
//! - Fingerprint GENERATION (decode a window to PCM + chromaprint) rides the
//!   libav worker pool via `TinyOp::Fingerprint` (see `protocol` + the worker).

pub mod align;
pub mod season;
