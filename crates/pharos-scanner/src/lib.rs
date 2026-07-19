//! Filesystem media scanner. Generic over `Prober` so tests can swap a fake
//! for `FfmpegProber` (V12). Walk runs inside `spawn_blocking` so it never
//! parks the async runtime (V5).

pub mod detect;
pub mod ffmpeg;
pub mod fingerprint;
pub mod fs;
#[cfg(feature = "libav-probe")]
pub mod libav_prober;
pub mod metadata;
#[cfg(feature = "watch")]
pub mod watcher;

pub use detect::{detect_root_watchability, watchability_from_magic, RootWatchability};
pub use ffmpeg::{parse_ffprobe_output, FfmpegProber};
pub use fingerprint::{fingerprint, fingerprint_async};
pub use fs::{
    is_episode_path, parse_series_info, stable_id, FsScanner, PathUpdate, ProgressSink,
    ScanProgress, DEFAULT_EXTENSIONS,
};
#[cfg(feature = "libav-probe")]
pub use libav_prober::LibavProber;
pub use metadata::filename::{FilenameProvider, ParsedName};
pub use metadata::nfo::NfoProvider;
pub use metadata::sidecar::SidecarArtworkProvider;
pub use metadata::{ErasedProvider, MetadataResolver};
#[cfg(feature = "watch")]
pub use watcher::{spawn_watch, WatchError, WatchHandle, WatchOptions};
