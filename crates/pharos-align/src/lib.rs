//! Word-level forced alignment for lyrics, ported from the Python aligner
//! service (`aligner/pharos_aligner`) so the forward pass can move to Rust.
//!
//! This crate holds everything after the acoustic model: text normalisation,
//! the MMS_FA token dictionary, torchaudio's CTC forced-alignment Viterbi and
//! the frame-to-millisecond conversion. Each part is checked bit-for-bit
//! against the Python code by fixtures in `tests/fixtures`.

pub mod audio;
pub mod config;
pub mod ctc;
pub mod http;
pub mod line;
pub mod obs;
pub mod offset;
pub mod plan;
pub mod service;
pub mod text;
pub mod tokens;
pub mod weights;

/// Why a line could not be aligned; each variant carries the values that decided it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AlignError {
    #[error("character {character:?} in token {token:?} has no MMS_FA label")]
    UnknownCharacter { character: char, token: String },
    #[error("emission of {len} values is not {frames} frames x {classes} classes")]
    EmissionShape {
        len: usize,
        frames: usize,
        classes: usize,
    },
    #[error("no targets to align")]
    NoTargets,
    #[error("target {token} is the blank or outside the {classes} emission classes")]
    BadTarget { token: u32, classes: usize },
    #[error("{frames} frames cannot hold {targets} targets with {repeats} repeats")]
    TooFewFrames {
        frames: usize,
        targets: usize,
        repeats: usize,
    },
    #[error("Viterbi backtrack left the state space at frame {frame}")]
    BrokenPath { frame: usize },
    #[error("path collapsed to {spans} spans for {targets} targets")]
    SpanCount { spans: usize, targets: usize },
}
