//! Filesystem media scanner. Generic over `Prober` so tests can swap a fake
//! for `FfmpegProber` (V12). Walk runs inside `spawn_blocking` so it never
//! parks the async runtime (V5).

pub mod ffmpeg;
pub mod fs;
#[cfg(feature = "libav-probe")]
pub mod libav_prober;

pub use ffmpeg::{parse_ffprobe_output, FfmpegProber};
pub use fs::{is_episode_path, parse_series_info, stable_id, FsScanner, DEFAULT_EXTENSIONS};
#[cfg(feature = "libav-probe")]
pub use libav_prober::LibavProber;
