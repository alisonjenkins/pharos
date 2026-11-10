//! Word-level forced alignment for lyrics, ported from the Python aligner
//! service (`aligner/pharos_aligner`) so the forward pass can move to Rust.
//!
//! This crate holds everything after the acoustic model: text normalisation,
//! the MMS_FA token dictionary, torchaudio's CTC forced-alignment Viterbi and
//! the frame-to-millisecond conversion. Each part is checked bit-for-bit
//! against the Python code by fixtures in `tests/fixtures`.

pub mod text;
