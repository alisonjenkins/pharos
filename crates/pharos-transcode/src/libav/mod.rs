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
#[cfg(feature = "backend-lib")]
pub mod image;
#[cfg(feature = "backend-lib")]
pub mod probe;

/// Initialise libav once and quiet its logging. Drop-in for `ffmpeg::init()`
/// — same return type — but the first call also caps libav's global log
/// level and enables duplicate-collapsing.
///
/// libav writes decoder/demuxer diagnostics straight to the process's
/// stderr (fd 2), which in the crash-isolated `transcode-worker` is
/// inherited to the pod's stdout. A single hard-to-decode HEVC / Dolby
/// Vision source (matroska "Unexpected BlockAdditions", hevc "Duplicate POC
/// in a sequence", swscaler "deprecated pixel format") emitted ~25k lines in
/// ~13s — a firehose that evicted every structured JSON log from `kubectl
/// logs` within minutes and made an incident nearly undiagnosable. Capping
/// to `Error` drops that per-frame chatter while still surfacing genuine
/// faults; `SKIP_REPEATED` collapses consecutive identical lines.
#[cfg(feature = "backend-lib")]
pub fn init() -> Result<(), ffmpeg_the_third::Error> {
    use ffmpeg_the_third as ffmpeg;
    ffmpeg::init()?;
    static QUIET: std::sync::Once = std::sync::Once::new();
    QUIET.call_once(|| {
        ffmpeg::util::log::set_level(ffmpeg::util::log::Level::Error);
        ffmpeg::util::log::set_flags(ffmpeg::util::log::Flags::SKIP_REPEATED);
    });
    Ok(())
}
#[cfg(feature = "backend-lib")]
pub mod subtitle;
#[cfg(feature = "backend-lib")]
pub mod subtitle_windows;
#[cfg(feature = "backend-lib")]
pub mod trickplay;
#[cfg(feature = "backend-lib")]
pub mod waveform;
