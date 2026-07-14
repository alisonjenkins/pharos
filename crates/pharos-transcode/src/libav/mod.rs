//! In-process libav (software) implementations of the high-frequency
//! "tiny" ffmpeg ops — probe, single-frame image extract, trickplay
//! tiles, subtitle extract/convert, waveform. These replace per-call
//! `ffmpeg`/`ffprobe` fork/exec; they run inside the persistent
//! `transcode-worker` process (so a libav crash is contained — V6), via
//! `ffmpeg-the-third` v5 (ffmpeg 8.1).
//!
//! Behaviour is matched to the spawn path (`backend/spawn.rs`,
//! `pharos-scanner::ffmpeg`) so output is identical. Functions are
//! blocking; callers run them on a blocking thread.

#[cfg(feature = "backend-lib")]
pub mod attachment;
#[cfg(feature = "backend-lib")]
pub mod fingerprint;
#[cfg(feature = "backend-lib")]
pub mod frames;
pub mod image;
#[cfg(feature = "backend-lib")]
pub mod probe;
#[cfg(feature = "backend-lib")]
pub mod subtitle;
#[cfg(feature = "backend-lib")]
pub mod subtitle_windows;
#[cfg(feature = "backend-lib")]
pub mod trickplay;
#[cfg(feature = "backend-lib")]
pub mod waveform;
