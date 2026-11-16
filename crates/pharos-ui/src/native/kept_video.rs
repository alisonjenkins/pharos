//! Videos downloaded for watching with no signal (spec 048, ADR 0143).
//!
//! Shared by every app that plays video: the web app stores the bytes in
//! Cache Storage (`kept_video_web.rs`), the phone app under its files dir.
//! Both sit behind the [`VideoStore`] port, so the Download control, the
//! Downloads page and the offline player draw the same on each.
//!
//! A download is the player's own HLS stream at one fixed quality: every
//! playlist, init and segment response stored under the exact URL the
//! player requests (path and query). Played back, the same URLs are asked
//! for and the store answers them, so the player code does not change.
//! `pharos_hls::offline` lists the URLs and estimates the size.

use serde::{Deserialize, Serialize};

use super::offline_ui::{FREE_GUARD_BYTES, GIB};

/// Every downloaded video together stays under this unless the viewer
/// raises it (the same default as offline music on the phone).
pub const DEFAULT_CAP_BYTES: u64 = 4 * GIB;

/// The qualities a download can be made at, as the video bitrate cap the
/// player negotiates with (bits per second), lowest first.
pub const QUALITY_CHOICES_BPS: [u64; 3] = [1_000_000, 2_000_000, 4_000_000];

/// The quality picked when the viewer has not chosen: about 0.9 GiB an hour.
pub const DEFAULT_QUALITY_BPS: u64 = 2_000_000;

/// A quality's name on the Download control.
pub fn quality_label(bps: u64) -> &'static str {
    match bps {
        0..=1_499_999 => "Small (1 Mbps)",
        1_500_000..=2_999_999 => "Medium (2 Mbps)",
        _ => "Large (4 Mbps)",
    }
}

/// One subtitle track kept beside the video, as WebVTT.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeptSubtitle {
    /// The stream index the player names the track by.
    pub index: u32,
    pub label: String,
    pub language: Option<String>,
    /// The exact URL the player fetches it from; stored under that key.
    pub url: String,
}

/// Where a download is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum KeptState {
    /// Fetching: `done` of `total` files stored, `bytes` so far.
    Downloading { done: u32, total: u32, bytes: u64 },
    /// Every file stored: plays with no signal.
    Ready,
    /// Stopped short; `reason` names the cause and the offending value.
    Failed { reason: String },
}

/// One video on this device: what the Downloads page and the offline
/// player need, with no call to the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeptVideo {
    /// The item, as `ItemRef` prints it.
    pub item: String,
    /// The version played; empty is the primary.
    pub version: String,
    /// The signed-in user's id (hex, as `Session.me` reports), so one
    /// user's downloads are never listed for another.
    pub user: String,
    /// The display name ("Show · S1E2 · Title" for an episode).
    pub title: String,
    /// "movie" or "episode".
    pub kind: String,
    pub runtime_ms: u64,
    /// The exact master playlist URL the player opens.
    pub master_url: String,
    /// The audio and subtitle stream indexes the stream was made with.
    pub audio: Option<u32>,
    pub subtitles: Vec<KeptSubtitle>,
    /// Every stored key, master first.
    pub files: Vec<String>,
    /// The video bitrate cap it was made at.
    pub quality_bps: u64,
    /// The size estimated before starting.
    pub estimated_bytes: u64,
    pub state: KeptState,
    /// Where the viewer stopped, saved on the device while offline.
    pub position_ms: u64,
    /// ISO 8601 UTC.
    pub saved_at: String,
}

impl KeptVideo {
    pub fn ready(&self) -> bool {
        self.state == KeptState::Ready
    }

    /// Bytes this download holds now.
    pub fn bytes(&self) -> u64 {
        match &self.state {
            KeptState::Downloading { bytes, .. } => *bytes,
            KeptState::Ready | KeptState::Failed { .. } => self.estimated_bytes,
        }
    }
}

/// What the Download control hands the store to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeepVideoRequest {
    pub item: String,
    pub version: String,
    pub user: String,
    pub title: String,
    pub kind: String,
    pub runtime_ms: u64,
    /// `Decision.stream_url` from a negotiation that forced HLS at
    /// `quality_bps`.
    pub master_url: String,
    pub audio: Option<u32>,
    pub subtitles: Vec<KeptSubtitle>,
    pub quality_bps: u64,
}

/// Why a download was refused before it started.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeepVideoRefusal {
    #[error("needs about {needed} bytes but the downloads cap leaves {available}")]
    OverCap { needed: u64, available: u64 },
    #[error("needs about {needed} bytes but only {free} are free beyond the {guard}-byte guard")]
    LowSpace { needed: u64, free: u64, guard: u64 },
    #[error("already on this device: {item}")]
    AlreadyKept { item: String },
    #[error("video store failed: {0}")]
    Host(String),
}

/// Whether a download of `needed` bytes fits: under the cap with what is
/// kept already, and leaving [`FREE_GUARD_BYTES`] free on the device.
pub fn fits(needed: u64, used: u64, cap: u64, free: u64) -> Result<(), KeepVideoRefusal> {
    let available = cap.saturating_sub(used);
    if needed > available {
        return Err(KeepVideoRefusal::OverCap { needed, available });
    }
    if needed.saturating_add(FREE_GUARD_BYTES) > free {
        return Err(KeepVideoRefusal::LowSpace {
            needed,
            free,
            guard: FREE_GUARD_BYTES,
        });
    }
    Ok(())
}

/// The store of downloaded videos. The host fetches and stores in the
/// background; the UI polls [`VideoStore::videos`] for progress.
/// `Send + Sync` so it registers like the other ports; the web store keeps
/// its state in thread-locals, wasm being single-threaded.
pub trait VideoStore: Send + Sync {
    /// The signed-in user's downloads, newest first.
    fn videos(&self, user: &str) -> Vec<KeptVideo>;
    /// Start fetching: plan the files with `pharos_hls::offline`, check
    /// [`fits`], store each under its exact URL, record progress.
    fn start(&self, request: KeepVideoRequest) -> Result<(), KeepVideoRefusal>;
    /// Delete a download's files and record.
    fn remove(&self, user: &str, item: &str);
    /// Note where the viewer stopped, for the next offline start.
    fn save_position(&self, user: &str, item: &str, position_ms: u64);
    fn cap_bytes(&self) -> u64;
    fn used_bytes(&self, user: &str) -> u64;
    /// Free space as the host last measured it.
    fn free_bytes(&self) -> u64;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_download_must_fit_under_the_cap_and_above_the_guard() {
        assert_eq!(fits(GIB, 0, 4 * GIB, 10 * GIB), Ok(()));
        assert_eq!(
            fits(2 * GIB, 3 * GIB, 4 * GIB, 10 * GIB),
            Err(KeepVideoRefusal::OverCap {
                needed: 2 * GIB,
                available: GIB
            })
        );
        assert!(matches!(
            fits(GIB, 0, 4 * GIB, 2 * GIB),
            Err(KeepVideoRefusal::LowSpace { .. })
        ));
    }

    #[test]
    fn the_quality_labels_are_distinct() {
        let labels: Vec<_> = QUALITY_CHOICES_BPS
            .iter()
            .map(|b| quality_label(*b))
            .collect();
        assert_eq!(
            labels,
            ["Small (1 Mbps)", "Medium (2 Mbps)", "Large (4 Mbps)"]
        );
    }
}
