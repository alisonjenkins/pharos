//! P48 — `FfmpegBackend` trait — single abstraction for every
//! ffmpeg operation pharos performs.
//!
//! Three backends:
//!
//! 1. `SpawnBackend` (feature `backend-spawn`, default) — wraps a
//!    real `ffmpeg` binary via `tokio::process::Command`. Preserves
//!    V6 (child crash never crashes server) since the OS process
//!    boundary keeps faults isolated.
//!
//! 2. `LibBackend` (feature `backend-lib`, P49-P51) — calls
//!    `libavcodec` / `libavformat` / `libavfilter` / `libswscale`
//!    via the `ffmpeg-next` crate. Saves ~30-100ms per operation
//!    on macOS (no fork + exec + dyld) and avoids JSON parsing the
//!    spawn path needs to read ffprobe output. Faults are in-process
//!    so callers must defensively recover via `std::panic::catch_unwind`
//!    if V6 still matters (the higher-level scanner / HLS handlers
//!    do).
//!
//! 3. Tests can implement `FfmpegBackend` directly to short-circuit
//!    the real ffmpeg dependency entirely.
//!
//! Dyn-safe via `async-trait`-style `Pin<Box<dyn Future>>` returns.
//! `Arc<dyn FfmpegBackend>` lives on the server's `AppState` so
//! handlers reach the backend without static-dispatch ripple.

use crate::TranscodeOptions;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("ffmpeg invocation failed: {0}")]
    Ffmpeg(String),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("path is not utf-8")]
    NonUtf8Path,
    #[error("stream index {0} not found in source")]
    StreamMissing(u32),
    #[error("codec {0} not supported for this operation")]
    UnsupportedCodec(String),
    #[error("backend not built with this feature")]
    NotImplemented,
}

/// Probe metadata in the shape the scanner consumes. Mirrors
/// `pharos_scanner::FfprobeOutput` keys but stays a flat byte buffer
/// so the spawn backend can pass the raw ffprobe JSON through to the
/// existing `parse_ffprobe_output` parser without a copy.
#[derive(Debug, Clone)]
pub struct ProbeJson(pub Vec<u8>);

/// One reading from the audio waveform endpoint (P42). Each value is
/// the RMS level for one bin in dB; callers convert to linear amplitude.
pub type WaveformPoint = f32;

/// Subtitle target wire format requested by the caller. Determines
/// the ffmpeg muxer (`webvtt` vs `srt`) for the embedded-stream
/// extraction path.
#[derive(Debug, Clone, Copy)]
pub enum SubtitleFormat {
    WebVtt,
    Srt,
}

/// P48 — every ffmpeg operation pharos performs, behind a single
/// dyn-safe trait. `Box<Pin<dyn Future>>` returns rather than `async
/// fn in trait` so `Arc<dyn FfmpegBackend>` is dyn-safe in stable
/// Rust without pulling the `async-trait` macro into our deps.
pub trait FfmpegBackend: Send + Sync {
    /// Run ffprobe + return its raw JSON. Scanner code calls
    /// `parse_ffprobe_output` on the bytes; this trait stays JSON-
    /// agnostic so a future binary protocol (e.g. avformat direct
    /// reads) can short-circuit the JSON roundtrip.
    fn probe<'a>(
        &'a self,
        path: &'a Path,
    ) -> Pin<Box<dyn Future<Output = Result<ProbeJson, BackendError>> + Send + 'a>>;

    /// Extract a single image frame at the given seek point and
    /// write it as JPEG bytes to `out`. Used by ImageCache for
    /// Primary / Backdrop / Thumb / Chapter thumb extraction.
    fn extract_image<'a>(
        &'a self,
        src: &'a Path,
        seek_ms: u64,
        width: u32,
        out: &'a Path,
    ) -> Pin<Box<dyn Future<Output = Result<(), BackendError>> + Send + 'a>>;

    /// Extract one embedded subtitle stream as bytes in the requested
    /// `format`. Returns `UnsupportedCodec` on image-codec streams
    /// (PGS / DVB / VobSub) before spawning ffmpeg.
    fn extract_subtitle<'a>(
        &'a self,
        src: &'a Path,
        stream_idx: u32,
        format: SubtitleFormat,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, BackendError>> + Send + 'a>>;

    /// SRT sidecar → WebVTT conversion. The input path points at the
    /// sibling `.srt` file; the output WebVTT bytes are returned.
    fn convert_srt_to_webvtt<'a>(
        &'a self,
        src: &'a Path,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, BackendError>> + Send + 'a>>;

    /// P46 — JPEG → WebP / AVIF transcode for the image format
    /// query. `target_ext` is one of `"webp"` or `"avif"`; output
    /// bytes are written to `out`.
    fn transcode_image<'a>(
        &'a self,
        src: &'a Path,
        target_ext: &'a str,
        out: &'a Path,
    ) -> Pin<Box<dyn Future<Output = Result<(), BackendError>> + Send + 'a>>;

    /// P42 — sample the source's audio stream into `target_bins`
    /// RMS-dB readings. Returns a `Vec<f32>` of length `target_bins`
    /// (zero-padded on short reads); callers convert dB → linear.
    fn waveform_rms<'a>(
        &'a self,
        src: &'a Path,
        samples_per_bin: u64,
        target_bins: u32,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<WaveformPoint>, BackendError>> + Send + 'a>>;

    /// Existing HLS / direct-stream pipeline. Returns the transcode
    /// stream the caller pipes into the HTTP response. The spawn
    /// backend wires this directly to `FfmpegTranscoder::transcode`.
    /// The lib backend (P50) implements via libavformat write+mux.
    fn transcode_stream<'a>(
        &'a self,
        src: &'a Path,
        opts: &'a TranscodeOptions,
    ) -> Pin<Box<dyn Future<Output = Result<crate::TranscodeStream, BackendError>> + Send + 'a>>;
}

#[cfg(feature = "backend-spawn")]
pub mod spawn;
#[cfg(feature = "backend-spawn")]
pub use spawn::SpawnBackend;

#[cfg(feature = "backend-lib")]
pub mod lib_backend;
#[cfg(feature = "backend-lib")]
pub use lib_backend::LibBackend;
