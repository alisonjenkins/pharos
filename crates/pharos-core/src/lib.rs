//! pharos-core: domain traits at IO boundary (V12).
//! No IO impls here. Servers/adapters live in pharos-server and friends.

pub mod auth;
pub mod secret;

pub use auth::{
    AuthBackend, AuthError, AuthResult, AuthToken, TokenRecord, TokenStore, User, UserId,
    UserPolicy, UserRecord, UserStore,
};
pub use secret::SecretString;

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub type MediaId = u64;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MediaItem {
    pub id: MediaId,
    pub path: PathBuf,
    pub title: String,
    pub kind: MediaKind,
    /// Probed file/stream metadata persisted alongside the item.
    /// All fields optional — a probe failure or pre-ffprobe scan still
    /// yields a row, just with `MediaProbe::default()`. Jellyfin DTOs
    /// omit fields whose value is `None` so clients negotiate against
    /// reality, not a stub.
    pub probe: MediaProbe,
    /// Show-hierarchy metadata when kind == Episode. None for
    /// Movie / Audio. Synthesised Series + Season DTOs derive their
    /// stable ids from `series_name` + `(series_name, season_number)`
    /// respectively (via `series_id_for` / `season_id_for`).
    pub series: Option<SeriesInfo>,
    /// Unix-seconds timestamp of the first time pharos saw this
    /// item. Set on initial INSERT; preserved by `ON CONFLICT` so
    /// rescans don't reset "added on" dates. `None` for rows
    /// imported before migration 0010.
    pub created_at: Option<i64>,
}

/// Parent-show / season / episode metadata for items the scanner
/// promoted to `MediaKind::Episode`. `season_number` + `episode_number`
/// fall back to None when the path didn't yield them but the
/// containing dir still flagged as a season layout.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeriesInfo {
    pub series_name: String,
    pub season_number: Option<u32>,
    pub episode_number: Option<u32>,
}

/// Stream/format metadata pulled by `Prober::probe` (today: ffprobe).
/// Persisted on `MediaItem` so the API surface (PlaybackInfo, BaseItemDto)
/// reports real codec / container / size / runtime per file.
///
/// `frame_rate_mille` stores frames-per-second × 1000 to keep MediaProbe
/// `Eq` without leaking floats into the domain layer. Conversion helpers
/// (`frame_rate_f32`) live in the DTO boundary.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaProbe {
    pub size_bytes: Option<u64>,
    pub duration_ms: Option<u64>,
    pub container: Option<String>,
    pub bitrate_bps: Option<u64>,
    pub video_codec: Option<String>,
    /// Canonical H.264/HEVC/VP9 profile name as ffprobe reports
    /// (`"High"`, `"Main"`, `"Main 10"`, `"Profile 0"`). Used to
    /// build RFC 6381 CODECS strings for HLS playlists.
    pub video_profile: Option<String>,
    /// Codec level × 10 (e.g. 40 = level 4.0, 51 = level 5.1). Wire
    /// format for the trailing two hex digits of `avc1.…` /
    /// `hvc1.…L<level>` codec tokens.
    pub video_level: Option<u32>,
    /// P13 — ffprobe `pix_fmt` token (e.g. `"yuv420p"`,
    /// `"yuv420p10le"`). Distinguishes 8-bit vs 10-bit pipelines so
    /// HDR-capable clients pick the right decoder path.
    pub pixel_format: Option<String>,
    /// ffprobe `color_primaries` (`"bt709"`, `"bt2020"`).
    pub color_primaries: Option<String>,
    /// ffprobe `color_transfer` (`"bt709"`, `"smpte2084"` = HDR10,
    /// `"arib-std-b67"` = HLG). Primary HDR discriminator.
    pub color_transfer: Option<String>,
    /// ffprobe `color_space` (`"bt709"`, `"bt2020nc"`).
    pub color_space: Option<String>,
    pub audio_codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub frame_rate_mille: Option<u32>,
    pub audio_channels: Option<u32>,
    pub sample_rate: Option<u32>,
    /// Embedded subtitle tracks discovered by the prober. Stored
    /// JSON-serialised in the `subtitle_tracks` column.
    pub subtitle_tracks: Vec<SubtitleTrack>,
    /// P16 — every audio stream the source carries. The scalar
    /// `audio_codec` / `audio_channels` / `sample_rate` above stay
    /// populated from the first stream for back-compat with rows that
    /// pre-date the multi-track migration. Empty Vec = no audio
    /// streams in source.
    pub audio_tracks: Vec<AudioTrack>,
    /// Common audio-file format tags (`title` / `artist` / `album` /
    /// `album_artist` / `genre`). Populated by FfmpegProber from
    /// ffprobe's `format.tags`. None when the file lacks the tag.
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub genre: Option<String>,
    /// Embedded chapter markers extracted by ffprobe `-show_chapters`.
    /// Each entry's `start_ms` lands on Jellyfin's `Chapters[].StartPositionTicks`.
    pub chapters: Vec<MediaChapter>,
    /// P34 — alternate playable versions of the same logical item
    /// (theatrical / director's cut / extended / alternate dubs).
    /// PlaybackInfo emits one MediaSource per entry in addition to
    /// the primary version this struct describes. Empty Vec leaves
    /// PlaybackInfo single-source. A future scanner enrichment pass
    /// populates this from sibling-file convention or NFO metadata.
    pub alternate_sources: Vec<AlternateMediaSource>,
}

/// P34 — minimal MediaSource shape carried alongside the primary
/// probe so PlaybackInfo can advertise multiple editions of the same
/// item. Path is stored so the segment + direct-play handlers know
/// which file to mux. Fields not listed here fall back to the primary
/// probe at PlaybackInfo build time (saves duplicating the entire
/// codec stack for every edition).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlternateMediaSource {
    /// Stable id suffix appended to the parent item id when forming
    /// the wire `MediaSourceInfo.Id`. Real Jellyfin uses a free-form
    /// string here so existing client URLs survive a re-scan.
    pub id: String,
    /// Filesystem path to the alternate-edition source file. Same
    /// shape as `MediaItem.path`; the request-path handlers honour
    /// it instead of the primary path when the wire MediaSourceId
    /// selects this entry.
    pub path: std::path::PathBuf,
    pub container: Option<String>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub bitrate_bps: Option<u64>,
    pub size_bytes: Option<u64>,
    pub duration_ms: Option<u64>,
    /// Human-readable edition tag (`"Director's Cut"`, `"Extended"`,
    /// `"Theatrical"`). Surfaces as `MediaSourceInfo.Name` so the
    /// jellyfin-web edition picker labels rows correctly.
    pub name: Option<String>,
}

/// One chapter marker. `title` defaults to `Chapter {N}` when ffprobe
/// reports no name (most BluRay rips).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaChapter {
    pub start_ms: u64,
    pub end_ms: u64,
    pub title: String,
}

/// P16 — one embedded audio stream from the source file. Multi-track
/// containers (TV episodes with eng + jpn dubs, movies with director
/// commentary) emit one entry per stream so the PlaybackInfo wire
/// shape surfaces a track picker.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioTrack {
    pub stream_index: u32,
    pub codec: Option<String>,
    pub channels: Option<u32>,
    pub sample_rate: Option<u32>,
    pub language: Option<String>,
    pub title: Option<String>,
    pub is_default: bool,
    /// P37 — track-level ReplayGain in centidecibels (× 100). ffprobe
    /// reports `tags.replaygain_track_gain` as `"-7.34 dB"`; the
    /// scanner parses the leading float and rounds to centidecibels.
    /// `Option<i16>` keeps the Eq derive (Option<f32> would break it)
    /// and the range easily fits all realistic gain values.
    pub replaygain_track_centidb: Option<i16>,
    /// P37 — album-level ReplayGain, same encoding as the track field.
    pub replaygain_album_centidb: Option<i16>,
}

/// One embedded subtitle stream from the source file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubtitleTrack {
    /// ffprobe stream index — what we pass `ffmpeg -map 0:s:<n>`.
    pub stream_index: u32,
    /// ISO-639 language tag when ffprobe emitted one.
    pub language: Option<String>,
    /// Codec name (`subrip`, `webvtt`, `ass`, ...) used to pick the
    /// right extraction pipeline.
    pub codec: Option<String>,
    /// Optional human-readable title.
    pub title: Option<String>,
    /// `true` when the stream's `disposition.default` flag is set.
    pub is_default: bool,
    /// `true` when the stream's `disposition.forced` flag is set.
    pub is_forced: bool,
    /// P35 — `true` when ffprobe reports `disposition.hearing_impaired`
    /// (the SDH / CC flag). Surfaces in MediaStream as
    /// `IsHearingImpaired` so jellyfin-web's subtitle picker can
    /// label the track and accessibility filtering works.
    pub is_hearing_impaired: bool,
}

impl MediaProbe {
    /// Convenience accessor — fps as f32, rounded back from the
    /// `× 1000` integer storage. Returns `None` if absent.
    pub fn frame_rate_f32(&self) -> Option<f32> {
        self.frame_rate_mille.map(|m| m as f32 / 1000.0)
    }

    /// Convert duration_ms → Jellyfin's 100-ns ticks (10_000 ticks / ms).
    pub fn run_time_ticks(&self) -> Option<u64> {
        self.duration_ms.map(|ms| ms.saturating_mul(10_000))
    }

    /// P13 — derive the Jellyfin `VideoRange` discriminator (`"HDR"`
    /// vs `"SDR"`) from probe color metadata. HDR10 uses
    /// `smpte2084`; HLG broadcast uses `arib-std-b67`; Dolby Vision
    /// ffprobe also reports `smpte2084` for the base layer.
    pub fn video_range(&self) -> &'static str {
        match self.color_transfer.as_deref() {
            Some("smpte2084") | Some("arib-std-b67") => "HDR",
            _ => "SDR",
        }
    }

    /// True when the probe carries HDR transfer characteristics.
    pub fn is_hdr(&self) -> bool {
        matches!(self.video_range(), "HDR")
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

/// LIB-A1 — per-row scan-state signature used for incremental rescans.
/// `file_mtime` / `file_size` are the filesystem stat values seen on the
/// last scan (distinct from `MediaProbe::size_bytes`, which is the
/// ffprobe-reported format size). The A2 skip-unchanged path compares a
/// fresh stat against this signature to decide whether re-probing is
/// needed; `last_seen_scan_id` ties the row to the most recent scan run
/// that observed it (the A3 mark-and-sweep token).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanState {
    /// Unix-seconds timestamp of the scan that last touched this row.
    pub last_scanned: i64,
    /// Filesystem mtime (unix seconds) recorded at last scan.
    pub file_mtime: i64,
    /// Filesystem size in bytes recorded at last scan.
    pub file_size: u64,
    /// Id of the most recent `scan_runs` entry that saw this row.
    pub last_seen_scan_id: i64,
}

pub trait MediaStore: Send + Sync {
    fn get(&self, id: MediaId)
        -> impl std::future::Future<Output = DomainResult<MediaItem>> + Send;
    fn put(&self, item: MediaItem) -> impl std::future::Future<Output = DomainResult<()>> + Send;
    fn list(&self) -> impl std::future::Future<Output = DomainResult<Vec<MediaItem>>> + Send;

    /// LIB-A1 — read the stored fs-stat signature for one item, or
    /// `None` when the row is absent or predates migration 0016 (no
    /// signature recorded yet, so the caller must re-probe).
    fn scan_state(
        &self,
        id: MediaId,
    ) -> impl std::future::Future<Output = DomainResult<Option<ScanState>>> + Send;

    /// LIB-A1 — open a scan run against `root`, recording the start
    /// time. Returns the new `scan_runs.id` used as the mark-and-sweep
    /// token for `mark_seen` / `sweep_unseen` / `finish_scan`.
    fn begin_scan(
        &self,
        root: &std::path::Path,
    ) -> impl std::future::Future<Output = DomainResult<i64>> + Send;

    /// LIB-A1 — stamp `id` as seen by scan run `scan_id`, persisting the
    /// freshly-stat'd `mtime` / `size`. No-op (zero rows) when the id is
    /// absent — the caller `put`s before marking on a fresh insert.
    fn mark_seen(
        &self,
        id: MediaId,
        scan_id: i64,
        mtime: i64,
        size: u64,
    ) -> impl std::future::Future<Output = DomainResult<()>> + Send;

    /// LIB-A1 — root-scoped mark-and-sweep delete. Removes
    /// `media_items` rows under `root_prefix` whose `last_seen_scan_id`
    /// is NULL or != `scan_id` (i.e. not observed by the current run),
    /// returning the deleted ids. Root-scoped so sweeping one root never
    /// deletes another root's items (V10: a single atomic DELETE).
    fn sweep_unseen(
        &self,
        scan_id: i64,
        root_prefix: &str,
    ) -> impl std::future::Future<Output = DomainResult<Vec<MediaId>>> + Send;

    /// LIB-A1 — close the scan run, recording the finish time and the
    /// seen/swept counts for observability.
    fn finish_scan(
        &self,
        scan_id: i64,
        items_seen: i64,
        items_swept: i64,
    ) -> impl std::future::Future<Output = DomainResult<()>> + Send;
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

/// Per-user free-form preferences (UserConfiguration + display
/// preferences). Stored as JSON strings — the schema lives in
/// jellyfin-web's UserConfigurationDto and varies by version, so
/// the storage layer treats them as opaque payloads.
pub trait PreferenceStore: Send + Sync {
    fn get_user_configuration(
        &self,
        user: UserId,
    ) -> impl std::future::Future<Output = DomainResult<Option<String>>> + Send;

    fn set_user_configuration(
        &self,
        user: UserId,
        json: &str,
    ) -> impl std::future::Future<Output = DomainResult<()>> + Send;

    fn get_display_preferences(
        &self,
        user: UserId,
        dp_id: &str,
        client: &str,
    ) -> impl std::future::Future<Output = DomainResult<Option<String>>> + Send;

    fn set_display_preferences(
        &self,
        user: UserId,
        dp_id: &str,
        client: &str,
        json: &str,
    ) -> impl std::future::Future<Output = DomainResult<()>> + Send;
}

pub trait Scanner: Send + Sync {
    fn scan(
        &self,
        root: &std::path::Path,
    ) -> impl std::future::Future<Output = DomainResult<Vec<MediaItem>>> + Send;
}

/// Result of a single probe call. `kind` informs MediaItem classification;
/// `probe` carries the full metadata block persisted on the item.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeInfo {
    pub kind: MediaKind,
    pub probe: MediaProbe,
}

impl ProbeInfo {
    /// Backwards-compat shortcut for old callers that only checked
    /// `duration_ms`. Reads through to the inner probe block.
    pub fn duration_ms(&self) -> Option<u64> {
        self.probe.duration_ms
    }

    pub fn container(&self) -> Option<&str> {
        self.probe.container.as_deref()
    }
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
    fn channels(&self) -> impl std::future::Future<Output = DomainResult<Vec<LiveChannel>>> + Send;

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
