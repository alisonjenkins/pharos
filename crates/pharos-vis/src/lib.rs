//! Decodes a song just ahead of the playhead into the visualiser's frames
//! (ADR-0133): a spectrum in dBFS and a waveform window, the two things the
//! web build reads from an `AnalyserNode`.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod decode;
#[cfg(test)]
mod fixture;
pub mod frame;
pub mod spectrum;
