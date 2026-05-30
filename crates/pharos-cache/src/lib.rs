//! pharos-cache — data-plane caches lifted out of `pharos-server`.
//!
//! Four caches, all variations on the same theme: keyed on a stable
//! input tuple, materialise to disk on miss, LRU-evict by total bytes.
//! No HTTP, no server state — `pub fn new(...)` constructors take
//! plain paths + caps.
//!
//! - [`hls_cache`] — HLS segment ABR cache. Spawns ffmpeg per (item,
//!   variant, segment) tuple, deduplicates concurrent requests via
//!   `tokio::sync::Mutex`.
//! - [`image_cache`] — poster / backdrop / chapter thumb extractor.
//!   Falls through to ffmpeg `-ss` for video, ID3 attached_pic for
//!   audio. Format-negotiates webp / avif via the FfmpegBackend
//!   transcode_image path.
//! - [`trickplay_cache`] — sprite-sheet thumb grid used by jellyfin-web
//!   for scrub previews. Generates one tile-grid per interval.
//! - [`subtitle_cache`] — extracted / converted subtitle blob cache.
//!   Embedded streams via ffmpeg `-map`, sidecar SRT → WebVTT.

pub mod hls_cache;
pub mod image_cache;
pub mod subtitle_cache;
pub mod trickplay_cache;

pub use hls_cache::HlsSegmentCache;
pub use image_cache::{ImageCache, ImageRole};
pub use subtitle_cache::SubtitleCache;
pub use trickplay_cache::TrickplayCache;
