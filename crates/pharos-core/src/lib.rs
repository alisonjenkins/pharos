//! pharos-core: domain traits at IO boundary (V12).
//! No IO impls here. Servers/adapters live in pharos-server and friends.

pub mod auth;
pub mod secret;

pub use auth::{
    AuthBackend, AuthError, AuthResult, AuthToken, TokenStore, User, UserId, UserPolicy,
    UserRecord, UserStore,
};
pub use secret::SecretString;

use std::path::PathBuf;

pub type MediaId = u64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaItem {
    pub id: MediaId,
    pub path: PathBuf,
    pub title: String,
    pub kind: MediaKind,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum MediaKind {
    Movie,
    Episode,
    #[default]
    Audio,
}

impl MediaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MediaKind::Movie => "movie",
            MediaKind::Episode => "episode",
            MediaKind::Audio => "audio",
        }
    }
}

impl std::str::FromStr for MediaKind {
    type Err = DomainError;
    fn from_str(s: &str) -> DomainResult<Self> {
        match s {
            "movie" => Ok(MediaKind::Movie),
            "episode" => Ok(MediaKind::Episode),
            "audio" => Ok(MediaKind::Audio),
            other => Err(DomainError::Backend(format!("unknown media kind: {other}"))),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    #[error("not found: {0}")]
    NotFound(MediaId),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("backend: {0}")]
    Backend(String),
}

pub type DomainResult<T> = Result<T, DomainError>;

pub trait MediaStore: Send + Sync {
    fn get(
        &self,
        id: MediaId,
    ) -> impl std::future::Future<Output = DomainResult<MediaItem>> + Send;
    fn put(
        &self,
        item: MediaItem,
    ) -> impl std::future::Future<Output = DomainResult<()>> + Send;
    fn list(&self) -> impl std::future::Future<Output = DomainResult<Vec<MediaItem>>> + Send;
}

/// Per-(user, item) state Jellyfin tracks: watched/unwatched, play
/// count, resume position, favourite flag. T33 — drives the watched
/// indicator + resume tiles in jellyfin-web.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UserItemData {
    pub played: bool,
    pub play_count: u32,
    /// Resume position in Jellyfin's 100ns ticks (10_000_000 per
    /// second). Stays 0 once the item is fully played.
    pub last_played_position_ticks: u64,
    pub is_favorite: bool,
    /// Unix-seconds timestamp of the last progress/playback event.
    /// `0` means "never played" — kept separate from `played` so a
    /// favourited-but-never-played item still reports last_played=0.
    pub last_played_at: i64,
}

pub trait UserDataStore: Send + Sync {
    fn get_user_data(
        &self,
        user: UserId,
        item: MediaId,
    ) -> impl std::future::Future<Output = DomainResult<UserItemData>> + Send;

    fn set_user_data(
        &self,
        user: UserId,
        item: MediaId,
        data: UserItemData,
    ) -> impl std::future::Future<Output = DomainResult<()>> + Send;

    /// Bulk fetch keyed by `(user, item)`. Items not in the store
    /// default to `UserItemData::default()` — callers do not need to
    /// distinguish "row missing" from "all zeros". O(1) round trip
    /// instead of N point-fetches when rendering a library list.
    fn user_data_bulk(
        &self,
        user: UserId,
        items: &[MediaId],
    ) -> impl std::future::Future<Output = DomainResult<Vec<UserItemData>>> + Send;

    /// Item ids that have a non-zero `last_played_position_ticks` and
    /// are not flagged as played — drives Jellyfin's Resume row.
    fn resumable_items(
        &self,
        user: UserId,
    ) -> impl std::future::Future<Output = DomainResult<Vec<MediaId>>> + Send;
}

pub trait Scanner: Send + Sync {
    fn scan(
        &self,
        root: &std::path::Path,
    ) -> impl std::future::Future<Output = DomainResult<Vec<MediaItem>>> + Send;
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProbeInfo {
    pub kind: MediaKind,
    pub duration_ms: Option<u64>,
    pub container: Option<String>,
}

pub trait Prober: Send + Sync {
    fn probe(
        &self,
        path: &std::path::Path,
    ) -> impl std::future::Future<Output = DomainResult<ProbeInfo>> + Send;
}

/// Future transcoding ops (T8, T9). Inherits `probe` from `Prober`.
pub trait Transcoder: Prober {}

pub trait Clock: Send + Sync {
    fn now_unix_ms(&self) -> u64;
}

/// Live-TV channel exposed to Jellyfin clients via the /LiveTv API
/// surface (T47). `stream_url` is what the channel's video pulls
/// from — pharos may either pass-through or transcode depending on
/// the client's DeviceProfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveChannel {
    /// Stable id within the backend (e.g. `tvg-id` from M3U or
    /// HDHomeRun's `GuideNumber`).
    pub id: String,
    pub number: String,
    pub name: String,
    pub logo_url: Option<String>,
    pub stream_url: String,
    pub group_title: Option<String>,
}

/// EPG entry — one upcoming program on a channel. `start_unix_ms`
/// / `end_unix_ms` are absolute timestamps; consumers convert to
/// Jellyfin's ISO-8601 wire shape at the DTO boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpgProgram {
    pub channel_id: String,
    pub title: String,
    pub description: Option<String>,
    pub start_unix_ms: u64,
    pub end_unix_ms: u64,
}

pub trait TunerBackend: Send + Sync {
    fn channels(
        &self,
    ) -> impl std::future::Future<Output = DomainResult<Vec<LiveChannel>>> + Send;

    /// EPG programmes in `[start_unix_ms, end_unix_ms)`. Backends
    /// without an EPG return an empty Vec.
    fn programs(
        &self,
        start_unix_ms: u64,
        end_unix_ms: u64,
    ) -> impl std::future::Future<Output = DomainResult<Vec<EpgProgram>>> + Send;
}

#[cfg(test)]
mod tests;
