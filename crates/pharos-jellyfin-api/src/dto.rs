//! Jellyfin response/request DTOs. PascalCase on the wire to match the
//! reference Jellyfin API (V7).

use pharos_core::{User, UserPolicy};
use serde::{Deserialize, Serialize};

/// The wire index at which THIS item's sidecar subtitle streams begin: one past
/// the highest real ffprobe stream index (video is 0). B71 — the old fixed
/// `1_000_000` base made sidecar `MediaStream.Index` a sparse sentinel; the
/// jellyfin-sdk-kotlin players (Android/Google TV) treat `Index` POSITIONALLY
/// and crash (out-of-bounds) on it, whereas jellyfin-web tolerated the gap.
/// A per-item contiguous base keeps every emitted index a REAL, small,
/// collision-free position — computed identically by the stream builder and the
/// subtitle-fetch handler so they always agree.
pub fn sidecar_base_index(probe: &pharos_core::MediaProbe) -> u32 {
    let max_audio = probe
        .audio_tracks
        .iter()
        .map(|t| t.stream_index)
        .max()
        .unwrap_or(0);
    let max_sub = probe
        .subtitle_tracks
        .iter()
        .map(|t| t.stream_index)
        .max()
        .unwrap_or(0);
    // Video is index 0; sidecars start one past the highest real index.
    max_audio.max(max_sub) + 1
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct SystemInfoDto {
    pub id: String,
    pub server_name: String,
    pub version: String,
    pub product_name: &'static str,
    pub operating_system: &'static str,
    pub local_address: String,
    pub startup_wizard_completed: bool,
    pub cast_receiver_id: &'static str,
    pub operating_system_display_name: &'static str,
    pub has_pending_restart: bool,
    pub is_shutting_down: bool,
    pub supports_library_monitor: bool,
    pub web_socket_port_number: u16,
    pub completed_installations: Vec<serde_json::Value>,
    pub can_self_restart: bool,
    pub can_launch_web_browser: bool,
    pub program_data_path: &'static str,
    pub web_path: &'static str,
    pub items_by_name_path: &'static str,
    pub cache_path: &'static str,
    pub log_path: &'static str,
    pub internal_metadata_path: &'static str,
    pub transcoding_temp_path: &'static str,
    pub has_update_available: bool,
    pub encoder_location: &'static str,
    pub system_architecture: &'static str,
}

/// A Jellyfin `MediaSegmentDto` (Skip Intro/Outro). `Id` is typed as a `UUID`
/// by the kotlin SDK — a non-UUID string (the old `"{item_id}:{idx}"`) crashes
/// the strict client during playback. `new()` derives a DETERMINISTIC uuid from
/// (item_id, key) so it's stable across requests AND always a valid UUID —
/// making the malformed-id state unrepresentable (B69).
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct MediaSegmentDto {
    pub id: String,
    pub item_id: String,
    pub start_ticks: u64,
    pub end_ticks: u64,
    #[serde(rename = "Type")]
    pub kind: String,
}

impl MediaSegmentDto {
    /// Fixed UUIDv5 namespace for deriving a segment's stable id.
    const NS: uuid::Uuid = uuid::Uuid::from_u128(0x6d65_6469_6173_6567_6d65_6e74_7635_0001);

    pub fn new(
        item_id: &str,
        key: &str,
        start_ticks: u64,
        end_ticks: u64,
        kind: impl Into<String>,
    ) -> Self {
        let id = uuid::Uuid::new_v5(&Self::NS, format!("{item_id}:{key}").as_bytes())
            .simple()
            .to_string();
        Self {
            id,
            item_id: item_id.to_string(),
            start_ticks,
            end_ticks,
            kind: kind.into(),
        }
    }
}

/// Response of `POST /ClientLog/Document` (Jellyfin `ClientLogDocumentResponseDto`).
/// The client uploads a log / crash report; the server stores it and returns the
/// filename it was written under.
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ClientLogDocumentResponseDto {
    pub file_name: String,
}

/// The UNAUTHENTICATED `/System/Info/Public` probe every client hits at boot
/// (the kotlin SDK's `PublicSystemInfo`). A strict subset of SystemInfo with no
/// server-internal paths (V9). Typed so the boot handshake can't silently drop
/// a field a client keys on.
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct PublicSystemInfoDto {
    pub id: String,
    pub server_name: String,
    pub version: String,
    pub product_name: &'static str,
    pub operating_system: &'static str,
    pub local_address: String,
    pub startup_wizard_completed: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct AuthenticateByNameRequest {
    pub username: String,
    pub pw: String,
}

// V8: manual `Debug` so an accidental `tracing::debug!(?body)` or
// `error!(?body)` never logs the cleartext password. The username is
// still visible — it isn't secret on its own.
impl std::fmt::Debug for AuthenticateByNameRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthenticateByNameRequest")
            .field("username", &self.username)
            .field("pw", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct AuthenticationResultDto {
    pub user: UserDto,
    pub session_info: SessionInfoDto,
    pub access_token: String,
    pub server_id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct UserDto {
    pub name: String,
    pub server_id: String,
    pub id: String,
    pub has_password: bool,
    pub has_configured_password: bool,
    /// Legacy, but a non-nullable bool in Jellyfin's C# UserDto — real
    /// Jellyfin always serializes it, so strict SDK clients require it.
    pub has_configured_easy_password: bool,
    pub policy: UserPolicyDto,
    pub configuration: UserConfigurationDto,
    pub primary_image_aspect_ratio: f32,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct UserConfigurationDto {
    pub audio_language_preference: String,
    pub play_default_audio_track: bool,
    pub subtitle_language_preference: String,
    pub display_missing_episodes: bool,
    pub grouped_folders: Vec<String>,
    pub subtitle_mode: String,
    pub display_collections_view: bool,
    pub enable_local_password: bool,
    pub ordered_views: Vec<String>,
    pub latest_items_excludes: Vec<String>,
    pub my_media_excludes: Vec<String>,
    pub hide_played_in_latest: bool,
    pub remember_audio_selections: bool,
    pub remember_subtitle_selections: bool,
    pub enable_next_episode_auto_play: bool,
    pub cast_receiver_id: String,
}

impl Default for UserConfigurationDto {
    fn default() -> Self {
        Self {
            audio_language_preference: String::new(),
            play_default_audio_track: true,
            subtitle_language_preference: String::new(),
            display_missing_episodes: false,
            grouped_folders: vec![],
            subtitle_mode: "Default".into(),
            display_collections_view: false,
            enable_local_password: false,
            ordered_views: vec![],
            latest_items_excludes: vec![],
            my_media_excludes: vec![],
            hide_played_in_latest: true,
            remember_audio_selections: true,
            remember_subtitle_selections: true,
            enable_next_episode_auto_play: true,
            cast_receiver_id: "F007D354".into(),
        }
    }
}

impl UserDto {
    pub fn from_domain(user: &User, server_id: &str) -> Self {
        Self {
            name: user.name.clone(),
            server_id: server_id.to_string(),
            id: user.id.0.simple().to_string(),
            has_password: user.has_password,
            has_configured_password: user.has_password,
            has_configured_easy_password: false,
            policy: UserPolicyDto::from_domain(&user.policy),
            configuration: UserConfigurationDto::default(),
            primary_image_aspect_ratio: 1.0,
        }
    }
}

/// A recurring weekly access window (Jellyfin `AccessSchedule`). `Id`/`UserId`
/// are Jellyfin bookkeeping pharos doesn't persist, so they're omitted (the
/// wire tolerates their absence — jellyfin-web reads DayOfWeek/StartHour/EndHour).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct AccessScheduleDto {
    pub day_of_week: String,
    pub start_hour: f64,
    pub end_hour: f64,
}

/// The full policy wire object. Deserialized on `POST /Users/{id}/Policy` and
/// serialized on every `UserDto`. [`Default`] is the permissive
/// [`UserPolicy::default`] projection so a partial POST body (Jellyfin treats a
/// policy write as a whole-object REPLACE) fills missing fields permissively
/// rather than zeroing them.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct UserPolicyDto {
    pub is_administrator: bool,
    pub is_hidden: bool,
    pub is_disabled: bool,
    pub enable_remote_access: bool,
    pub enable_media_playback: bool,
    pub enable_audio_playback_transcoding: bool,
    pub enable_video_playback_transcoding: bool,
    pub enable_playback_remuxing: bool,
    pub enable_content_deletion: bool,
    pub enable_content_downloading: bool,
    pub enable_sync_transcoding: bool,
    pub enable_media_conversion: bool,
    pub enable_all_devices: bool,
    pub enable_all_channels: bool,
    pub enable_all_folders: bool,
    pub enable_public_sharing: bool,
    // The remaining non-nullable value-type fields of Jellyfin's C# UserPolicy.
    // Real Jellyfin always serializes every one (ignore-null only skips
    // nullables), so strict SDK clients (kotlin) require them all.
    pub enable_collection_management: bool,
    pub enable_subtitle_management: bool,
    pub enable_lyric_management: bool,
    pub enable_user_preference_access: bool,
    pub enable_remote_control_of_other_users: bool,
    pub enable_shared_device_control: bool,
    pub enable_live_tv_management: bool,
    pub enable_live_tv_access: bool,
    pub force_remote_source_transcoding: bool,
    pub invalid_login_attempt_count: i32,
    pub login_attempts_before_lockout: i32,
    pub max_active_sessions: i32,
    pub remote_client_bitrate_limit: i32,
    /// Jellyfin `SyncPlayUserAccessType` — `CreateAndJoinGroups` | `JoinGroups`
    /// | `None`. jellyfin-web hides the group-watch (SyncPlay) UI unless this
    /// grants access, so an absent/None value makes "create a group" a no-op.
    /// `String` (not `&'static str`) because the DTO also derives `Deserialize`.
    pub sync_play_access: String,
    /// Non-null `String`s in the jellyfin-sdk-kotlin `UserPolicy` (no `?`).
    /// pharos omitted them, so the Android/Google-TV SDK threw parsing
    /// `UserDto.policy` inside the Quick Connect finalize's AuthenticationResult
    /// → "Unable to connect to server" despite a valid token. Real Jellyfin
    /// always serializes the default provider ids (B64).
    pub authentication_provider_id: String,
    pub password_reset_provider_id: String,
    // T68 — library-access + parental-control fields. jellyfin-web's
    // dashboard writes all of these; pharos persists them in `policy_json`
    // and enforces EnabledFolders + MaxParentalRating on the item-listing path.
    /// Library wire ids the user may browse when `!enable_all_folders`.
    pub enabled_folders: Vec<String>,
    /// Nullable max parental-rating score; `null` = unrestricted.
    pub max_parental_rating: Option<i32>,
    /// Item types whose unrated members are blocked (Jellyfin `UnratedItem`).
    pub block_unrated_items: Vec<String>,
    pub blocked_tags: Vec<String>,
    pub allowed_tags: Vec<String>,
    pub access_schedules: Vec<AccessScheduleDto>,
}

impl Default for UserPolicyDto {
    fn default() -> Self {
        Self::from_domain(&UserPolicy::default())
    }
}

impl UserPolicyDto {
    pub fn from_domain(p: &UserPolicy) -> Self {
        Self {
            is_administrator: p.admin,
            is_hidden: p.is_hidden,
            is_disabled: p.is_disabled,
            enable_remote_access: true,
            enable_media_playback: true,
            enable_audio_playback_transcoding: true,
            enable_video_playback_transcoding: true,
            enable_playback_remuxing: true,
            enable_content_deletion: p.admin,
            enable_content_downloading: p.enable_content_downloading,
            enable_sync_transcoding: true,
            enable_media_conversion: true,
            enable_all_devices: true,
            enable_all_channels: true,
            enable_all_folders: p.enable_all_folders,
            enable_public_sharing: false,
            enable_collection_management: p.admin,
            enable_subtitle_management: p.admin,
            enable_lyric_management: p.admin,
            enable_user_preference_access: true,
            enable_remote_control_of_other_users: p.admin,
            enable_shared_device_control: true,
            enable_live_tv_management: p.admin,
            enable_live_tv_access: p.enable_live_tv_access,
            force_remote_source_transcoding: false,
            invalid_login_attempt_count: 0,
            login_attempts_before_lockout: p.login_attempts_before_lockout,
            max_active_sessions: p.max_active_sessions,
            remote_client_bitrate_limit: p.remote_client_bitrate_limit,
            sync_play_access: p.sync_play_access.clone(),
            // Real Jellyfin's default provider ids — any non-empty string
            // satisfies the kotlin non-null contract (B64).
            authentication_provider_id:
                "Jellyfin.Server.Implementations.Users.DefaultAuthenticationProvider".to_string(),
            password_reset_provider_id:
                "Jellyfin.Server.Implementations.Users.DefaultPasswordResetProvider".to_string(),
            enabled_folders: p.enabled_folders.clone(),
            max_parental_rating: p.max_parental_rating,
            block_unrated_items: p.block_unrated_items.clone(),
            blocked_tags: p.blocked_tags.clone(),
            allowed_tags: p.allowed_tags.clone(),
            access_schedules: p
                .access_schedules
                .iter()
                .map(|s| AccessScheduleDto {
                    day_of_week: s.day_of_week.clone(),
                    start_hour: s.start_hour,
                    end_hour: s.end_hour,
                })
                .collect(),
        }
    }

    /// Project an inbound `POST /Users/{id}/Policy` body onto the domain
    /// [`UserPolicy`]. Jellyfin treats a policy write as a whole-object
    /// replace, so every modelled field is taken from the body (missing ones
    /// having been permissively filled by [`Default`]).
    pub fn to_domain(&self) -> UserPolicy {
        UserPolicy {
            admin: self.is_administrator,
            is_disabled: self.is_disabled,
            is_hidden: self.is_hidden,
            enable_all_folders: self.enable_all_folders,
            enabled_folders: self.enabled_folders.clone(),
            max_parental_rating: self.max_parental_rating,
            block_unrated_items: self.block_unrated_items.clone(),
            blocked_tags: self.blocked_tags.clone(),
            allowed_tags: self.allowed_tags.clone(),
            access_schedules: self
                .access_schedules
                .iter()
                .map(|s| pharos_core::AccessSchedule {
                    day_of_week: s.day_of_week.clone(),
                    start_hour: s.start_hour,
                    end_hour: s.end_hour,
                })
                .collect(),
            max_active_sessions: self.max_active_sessions,
            login_attempts_before_lockout: self.login_attempts_before_lockout,
            remote_client_bitrate_limit: self.remote_client_bitrate_limit,
            enable_live_tv_access: self.enable_live_tv_access,
            enable_content_downloading: self.enable_content_downloading,
            sync_play_access: self.sync_play_access.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct BaseItemDto {
    pub id: String,
    pub name: String,
    pub server_id: String,
    #[serde(rename = "Type")]
    pub kind: &'static str,
    pub media_type: &'static str,
    pub is_folder: bool,
    pub user_data: UserItemDataDto,
    pub run_time_ticks: u64,
    pub location_type: &'static str,
    pub can_play: bool,
    pub media_sources: Vec<MediaSourceLiteDto>,
    pub play_access: &'static str,
    // Array-typed fields jellyfin-web iterates over without null
    // guards (T30). Default-empty so for-of / spread / .map don't
    // throw Symbol.iterator TypeErrors during view init.
    pub artists: Vec<String>,
    pub artist_items: Vec<NameGuidPairDto>,
    pub album_artists: Vec<NameGuidPairDto>,
    pub genres: Vec<String>,
    pub genre_items: Vec<NameGuidPairDto>,
    pub tags: Vec<String>,
    pub studios: Vec<NameGuidPairDto>,
    pub people: Vec<PersonDto>,
    pub production_locations: Vec<String>,
    pub provider_ids: serde_json::Map<String, serde_json::Value>,
    pub remote_trailers: Vec<serde_json::Value>,
    pub chapters: Vec<ChapterInfoDto>,
    pub trickplay: serde_json::Map<String, serde_json::Value>,
    pub external_urls: Vec<serde_json::Value>,
    pub image_tags: serde_json::Map<String, serde_json::Value>,
    pub backdrop_image_tags: Vec<String>,
    /// Server-side first-seen timestamp as ISO-8601. None when the
    /// row predates migration 0010 (pre-T-fix-39 rescans).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date_created: Option<String>,
    /// jellyfin-web's breadcrumb back-nav requires every item to
    /// reference its parent: Episode → SeasonId, Audio in album →
    /// AlbumId, otherwise the containing library / root id. Set by
    /// `BaseItemDto::with_parent_id(...)` after construction since
    /// the library-root mapping lives in AppState.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Audio metadata: album name (None for video/no-tag files).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_id: Option<String>,
    // Series-hierarchy fields populated when this item is an Episode.
    // jellyfin-web's Shows view reads them to render the Series ▸
    // Season ▸ Episode breadcrumb.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub season_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub season_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_index_number: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_number: Option<u32>,
    pub screenshot_image_tags: Vec<String>,
    /// P13 — Jellyfin video range. "HDR" for HDR10 / HLG / DV
    /// sources; "SDR" otherwise. Clients use this to enable HDR
    /// playback paths on capable displays.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_range: Option<&'static str>,
    // LIB-C7/C8/C9 — descriptive metadata projected from
    // `MediaItem.metadata`. All `Option`/array fields are
    // `skip_serializing_if`-guarded so the wire shape is unchanged when
    // the data is absent (existing /Items golden shape stays intact).
    /// C8 — long-form synopsis.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overview: Option<String>,
    /// C8 — Jellyfin uses an array; carries the single tagline or stays
    /// empty. Emitted only when non-empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub taglines: Vec<String>,
    /// C7 — audience rating (0–10).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub community_rating: Option<f32>,
    /// C7 — critic rating (0–100).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub critic_rating: Option<f32>,
    /// C7 — parental rating string, e.g. "PG-13".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub official_rating: Option<String>,
    /// C7 — release year.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub production_year: Option<u32>,
    /// C7 — premiere/air date as ISO-8601 (from unix-secs).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub premiere_date: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct NameGuidPairDto {
    pub name: String,
    pub id: String,
}

/// A SYNTHESISED browse item — Genre / Studio / Tag(Folder) / MusicArtist /
/// MusicAlbum / BoxSet / Playlist / Person / LiveTv channel & program. pharos
/// stores none of these as real `MediaItem`s (they're aggregated from tags or
/// grouped rows), so their `BaseItemDto` was hand-rolled as a `serde_json::json!`
/// literal per endpoint. This one typed struct replaces all of them (B78/V38):
/// `Id`+`Type` are mandatory (the only kotlin-required fields — `Type` must be a
/// valid `BaseItemKind`, e.g. Tag→"Folder" per B69); every richer field is
/// optional and OMITTED when `None`, so each call site sets exactly the fields
/// its former literal emitted (empty `{}`/`[]` collections included, via
/// `Some(empty)`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct SynthItemDto {
    pub id: String,
    pub name: String,
    pub server_id: String,
    #[serde(rename = "Type")]
    pub kind: &'static str,
    pub media_type: String,
    pub is_folder: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub can_delete: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collection_type: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overview: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub production_year: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub premiere_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_number: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Empty `{}` map when `Some`; omitted when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_tags: Option<std::collections::BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backdrop_image_tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub genres: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_artists: Option<Vec<NameGuidPairDto>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist_items: Option<Vec<NameGuidPairDto>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_data: Option<UserItemDataDto>,
}

/// A library `CollectionFolder` (the `/UserViews` + `/Library/MediaFolders`
/// home rows). Embeds the B68-critical folder `UserData` (typed). `Type` is a
/// valid BaseItemKind; `CollectionType` is nullable (null for a mixed library)
/// and ALWAYS on the wire. Typed per B78/V38.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct CollectionFolderDto {
    pub id: String,
    pub name: String,
    pub server_id: String,
    #[serde(rename = "Type")]
    pub kind: &'static str,
    pub collection_type: Option<String>,
    pub media_type: &'static str,
    pub is_folder: bool,
    pub user_data: UserItemDataDto,
}

/// A synthesised `Series` or `Season` folder (pharos stores no Series/Season
/// entity — they're derived from episode `SeriesInfo`). Near-full `BaseItemDto`
/// with the empty spread-target arrays jellyfin-web expects + the B68 folder
/// `UserData`. Typed per B78/V38. Series-only fields (production_locations /
/// remote_trailers / chapters / production_year) and Season-only fields
/// (series_name / series_id / index_number) are optional-and-omitted on the
/// other kind.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct SeriesFolderDto {
    pub id: String,
    pub name: String,
    pub server_id: String,
    #[serde(rename = "Type")]
    pub kind: &'static str,
    pub media_type: &'static str,
    pub is_folder: bool,
    pub can_play: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_number: Option<u32>,
    pub user_data: UserItemDataDto,
    pub genres: Vec<String>,
    pub genre_items: Vec<NameGuidPairDto>,
    pub tags: Vec<String>,
    pub studios: Vec<NameGuidPairDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub production_locations: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_trailers: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chapters: Option<Vec<ChapterInfoDto>>,
    pub image_tags: std::collections::BTreeMap<String, String>,
    pub backdrop_image_tags: Vec<String>,
    pub provider_ids: std::collections::BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub production_year: Option<i32>,
}

impl SynthItemDto {
    /// A bare synth folder (`Id`, `Name`, `ServerId`, `Type`, `MediaType`
    /// "Unknown", `IsFolder` true) — the Genre/Studio/Tag shape. Callers add
    /// richer fields on the returned value as needed.
    pub fn folder(id: String, name: String, server_id: String, kind: &'static str) -> Self {
        Self {
            id,
            name,
            server_id,
            kind,
            media_type: "Unknown".to_string(),
            is_folder: true,
            child_count: None,
            can_delete: None,
            collection_type: None,
            overview: None,
            production_year: None,
            premiere_date: None,
            index_number: None,
            parent_id: None,
            image_tags: None,
            backdrop_image_tags: None,
            genres: None,
            tags: None,
            album_artist: None,
            album_artists: None,
            artist_items: None,
            user_data: None,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct PersonDto {
    pub name: String,
    pub id: String,
    pub role: String,
    #[serde(rename = "Type")]
    pub kind: &'static str,
    pub primary_image_tag: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct MediaSourceLiteDto {
    pub id: String,
    pub container: String,
    #[serde(rename = "Type")]
    pub kind: &'static str,
    pub is_remote: bool,
    pub supports_direct_play: bool,
    pub supports_direct_stream: bool,
    pub supports_transcoding: bool,
    // Remaining non-nullable value-type fields of the C# MediaSourceInfo —
    // real Jellyfin always serializes them; strict SDK clients require them
    // on the item-embedded MediaSources too, not just PlaybackInfo (B13).
    pub read_at_native_framerate: bool,
    pub ignore_dts: bool,
    pub ignore_index: bool,
    pub gen_pts_input: bool,
    pub is_infinite_stream: bool,
    pub has_segments: bool,
    pub requires_opening: bool,
    pub requires_closing: bool,
    pub requires_looping: bool,
    pub supports_probing: bool,
    /// Non-nullable MediaStreamProtocol enum in the SDKs — "http" when not
    /// transcoding (matches real Jellyfin).
    pub transcoding_sub_protocol: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_time_ticks: Option<u64>,
    pub protocol: &'static str,
    pub media_streams: Vec<MediaStreamDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bitrate: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    pub name: String,
    /// `None` when the source has no audio track. Hard-coding an index
    /// for silent-video fixtures made jellyfin-web's player attempt
    /// to select a track that doesn't exist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_audio_stream_index: Option<u32>,
    pub video_type: &'static str,
    pub e_tag: String,
}

/// The FULL `MediaSourceInfo` a `/Items/{id}/PlaybackInfo` response carries —
/// the exact object B70 traced the native Android/Google-TV app CRASHING while
/// parsing. Superset of [`MediaSourceLiteDto`] (item-embedded) with the
/// transcode-negotiation + playback-tuning fields. Typed (not a
/// `serde_json::json!` literal) per B78/V38 so the kotlin-required field set
/// (B13 — the non-null value fields + the non-null `TranscodingSubProtocol`
/// enum) can't silently regress. Nullable fields serialize as `null` (matching
/// the previous literal's exact wire shape).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct MediaSourceInfoDto {
    pub id: String,
    #[serde(rename = "Type")]
    pub kind: &'static str,
    pub container: String,
    pub is_remote: bool,
    #[serde(rename = "ETag")]
    pub e_tag: String,
    pub run_time_ticks: Option<u64>,
    pub size: Option<u64>,
    pub name: String,
    pub protocol: &'static str,
    pub supports_direct_play: bool,
    pub supports_direct_stream: bool,
    pub supports_transcoding: bool,
    pub transcoding_url: Option<String>,
    /// Non-nullable `MediaStreamProtocol` enum in the SDKs — "http"/"hls",
    /// NEVER null (a null fails the whole native deserialization, B13).
    pub transcoding_sub_protocol: &'static str,
    pub requires_opening: bool,
    pub requires_closing: bool,
    pub requires_looping: bool,
    pub supports_probing: bool,
    pub read_at_native_framerate: bool,
    pub ignore_dts: bool,
    pub ignore_index: bool,
    pub gen_pts_input: bool,
    pub is_infinite_stream: bool,
    pub has_segments: bool,
    pub media_streams: Vec<MediaStreamDto>,
    pub media_attachments: Vec<MediaAttachmentDto>,
    pub bitrate: Option<u64>,
    pub video_type: &'static str,
    pub default_audio_stream_index: Option<u32>,
    pub default_subtitle_stream_index: Option<u32>,
    pub buffer_ms: u32,
    pub analyze_duration_ms: u32,
    pub transcoding_max_audio_channels: u32,
    pub start_position_ticks: u64,
}

impl Default for MediaSourceInfoDto {
    /// The constant / never-varying fields real Jellyfin emits for a plain
    /// library file; call sites override only the item-specific ones via
    /// `..Default::default()`.
    fn default() -> Self {
        Self {
            id: String::new(),
            kind: "Default",
            container: String::new(),
            is_remote: false,
            e_tag: String::new(),
            run_time_ticks: None,
            size: None,
            name: String::new(),
            protocol: "File",
            supports_direct_play: false,
            supports_direct_stream: false,
            supports_transcoding: true,
            transcoding_url: None,
            transcoding_sub_protocol: "http",
            requires_opening: false,
            requires_closing: false,
            requires_looping: false,
            supports_probing: true,
            read_at_native_framerate: false,
            ignore_dts: false,
            ignore_index: false,
            gen_pts_input: false,
            is_infinite_stream: false,
            has_segments: false,
            media_streams: Vec::new(),
            media_attachments: Vec::new(),
            bitrate: None,
            video_type: "VideoFile",
            default_audio_stream_index: None,
            default_subtitle_stream_index: None,
            buffer_ms: 3000,
            analyze_duration_ms: 2_000_000,
            transcoding_max_audio_channels: 2,
            start_position_ticks: 0,
        }
    }
}

/// `/Items/{id}/PlaybackInfo` response envelope. Typed per B78/V38.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct PlaybackInfoResponseDto {
    pub media_sources: Vec<MediaSourceInfoDto>,
    pub play_session_id: String,
    pub start_position_ticks: u64,
}

/// Jellyfin `QuickConnectResult` — returned by `/QuickConnect/Initiate` and the
/// `/QuickConnect/Connect` poll. The kotlin SDK (Android/Google TV) requires
/// the non-null `DeviceName`/`AppName`/`AppVersion` strings and a real ISO-8601
/// `DateAdded` DateTime; a missing field or empty date fails the 5 s poll and
/// the TV silently greys out / drops the code before the user can type it
/// (B61). Typed per B78/V38. `Secret` MUST be echoed — jellyfin-web's login
/// loop finalizes with THIS response's `Secret`, not the one it kept.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct QuickConnectResultDto {
    pub code: String,
    pub secret: String,
    pub device_id: String,
    pub device_name: String,
    pub app_name: String,
    pub app_version: String,
    pub authenticated: bool,
    pub date_added: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct MediaStreamDto {
    #[serde(rename = "Type")]
    pub kind: &'static str,
    pub index: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    pub is_default: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channels: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bit_rate: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aspect_ratio: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub real_frame_rate: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub average_frame_rate: Option<f32>,
    /// ISO-639 language tag on Audio + Subtitle tracks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Human-readable title on Subtitle tracks (e.g. "English [SDH]").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// `IsExternal` flags sidecar vs embedded tracks (subtitle picker UI).
    /// ALWAYS serialized: jellyfin-sdk-kotlin's `MediaStream` model marks
    /// this (and the other `Is*` booleans below, plus `IsInterlaced` /
    /// `IsOriginal`) as REQUIRED — kotlinx.serialization throws on a missing
    /// field, so omitting any of them on a Video/Audio row makes the native
    /// Android/TV apps fail the whole PlaybackInfo/Items response ("Unable
    /// to resolve playback info"). jellyfin-web is lenient; the SDKs aren't.
    pub is_external: bool,
    pub is_forced: bool,
    /// P35 — `true` when the track's `disposition.hearing_impaired` flag is
    /// set (SDH / CC). jellyfin-web labels the picker entry with it and an
    /// accessibility filter on `/Items` reuses the field.
    pub is_hearing_impaired: bool,
    /// Required by the kotlin SDK; pharos never emits interlaced streams'
    /// disposition from the probe today, so `false` (real Jellyfin defaults
    /// false for the overwhelmingly common progressive case).
    pub is_interlaced: bool,
    /// Required by the kotlin SDK (10.9+ "original language track" flag).
    pub is_original: bool,
    /// Jellyfin's URL the player fetches the rendered .vtt from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery_url: Option<String>,
    /// Subtitle-only. How the client obtains the track: `"External"` (fetch the
    /// `DeliveryUrl` .vtt + render client-side — text subs) or `"Encode"` (burn
    /// into the transcoded video — image subs like PGS/VOBSUB that can't be
    /// VTT). jellyfin-web keys off this; WITHOUT it the subtitle picker can't
    /// deliver the track and nothing renders.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery_method: Option<&'static str>,
    /// `true` for text subs (subrip/ass/vtt/…), `false` for image subs
    /// (PGS/VOBSUB) and every non-subtitle stream. jellyfin-web's player picks
    /// its render path off it (native `<track>` / SubtitlesOctopus / custom cue
    /// renderer); the kotlin SDK requires the field on EVERY stream row.
    #[serde(rename = "IsTextSubtitleStream")]
    pub is_text_subtitle_stream: bool,
    /// `true` when the client can fetch the track out-of-band via
    /// `DeliveryUrl` (all text subs here). jellyfin-web gates the "download /
    /// external" render path on it; the kotlin SDK requires it on every row.
    #[serde(rename = "SupportsExternalStream")]
    pub supports_external_stream: bool,
    /// P37 — Audio-only. `{ "TrackGain": …, "AlbumGain": … }` in dB.
    /// Finamp reads this and applies loudness normalisation; absent
    /// keys leave the player at unity gain.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replay_gain: Option<ReplayGainDto>,
    /// Human-readable label jellyfin-web's audio/subtitle pickers render.
    /// WITHOUT this every entry shows "undefined". Composed from language +
    /// title + codec + channel layout (see `stream_display_title`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_title: Option<String>,
}

/// Map a 2/3-letter ISO-639 language code to an English display name, falling
/// back to the upper-cased code. Covers the languages common in a home library.
fn language_display_name(code: &str) -> String {
    match code.trim().to_ascii_lowercase().as_str() {
        "en" | "eng" => "English",
        "ja" | "jpn" => "Japanese",
        "es" | "spa" => "Spanish",
        "fr" | "fre" | "fra" => "French",
        "de" | "ger" | "deu" => "German",
        "it" | "ita" => "Italian",
        "pt" | "por" => "Portuguese",
        "ru" | "rus" => "Russian",
        "zh" | "chi" | "zho" => "Chinese",
        "ko" | "kor" => "Korean",
        "ar" | "ara" => "Arabic",
        "nl" | "dut" | "nld" => "Dutch",
        "pl" | "pol" => "Polish",
        "sv" | "swe" => "Swedish",
        "tr" | "tur" => "Turkish",
        "hi" | "hin" => "Hindi",
        "und" => return "Undetermined".to_string(),
        other => return other.to_ascii_uppercase(),
    }
    .to_string()
}

/// True when a subtitle codec is a TEXT format (extractable to WebVTT and
/// served as an `External` track the client renders), vs an IMAGE format
/// (PGS/VOBSUB/DVB — bitmap, can't be VTT, so it must be burned into the
/// transcode via `Encode`).
pub fn is_text_subtitle_codec(codec: Option<&str>) -> bool {
    matches!(
        codec.unwrap_or("").to_ascii_lowercase().as_str(),
        "subrip"
            | "srt"
            | "ass"
            | "ssa"
            | "webvtt"
            | "vtt"
            | "mov_text"
            | "text"
            | "subviewer"
            | "subviewer1"
            | "microdvd"
            | "stl"
            | "pjs"
            | "vplayer"
    )
}

/// True for ASS/SSA subtitle codecs — jellyfin-web renders these via
/// SubtitlesOctopus and must receive the RAW ASS at the DeliveryUrl.
fn is_ass_subtitle_codec(codec: Option<&str>) -> bool {
    matches!(
        codec.unwrap_or("").to_ascii_lowercase().as_str(),
        "ass" | "ssa" | "advanced substation alpha"
    )
}

/// Channel-count → common layout label (Jellyfin picker convention).
fn channel_layout_label(ch: u32) -> &'static str {
    match ch {
        1 => "Mono",
        2 => "Stereo",
        6 => "5.1",
        7 => "6.1",
        8 => "7.1",
        _ => "",
    }
}

/// Compose a jellyfin-web picker label: "Language - Title" (or, when there's no
/// track title, "Language - CODEC Layout"). Any part may be absent; returns
/// `None` only when nothing is known.
fn stream_display_title(
    language: Option<&str>,
    title: Option<&str>,
    codec: Option<&str>,
    channels: Option<u32>,
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(l) = language.map(str::trim).filter(|l| !l.is_empty()) {
        parts.push(language_display_name(l));
    }
    if let Some(t) = title.map(str::trim).filter(|t| !t.is_empty()) {
        parts.push(t.to_string());
    } else {
        // No embedded track title → describe by codec + channel layout.
        let mut desc = String::new();
        if let Some(c) = codec.map(str::trim).filter(|c| !c.is_empty()) {
            desc.push_str(&c.to_ascii_uppercase());
        }
        if let Some(layout) = channels.map(channel_layout_label).filter(|s| !s.is_empty()) {
            if !desc.is_empty() {
                desc.push(' ');
            }
            desc.push_str(layout);
        }
        if !desc.is_empty() {
            parts.push(desc);
        }
    }
    (!parts.is_empty()).then(|| parts.join(" - "))
}

/// Fill in `display_title` for every stream that lacks one. Called once after a
/// stream list is built so each construction site stays terse.
fn fill_display_titles(streams: &mut [MediaStreamDto]) {
    for s in streams.iter_mut() {
        if s.display_title.is_some() || s.kind == "Video" {
            continue;
        }
        s.display_title = stream_display_title(
            s.language.as_deref(),
            s.title.as_deref(),
            s.codec.as_deref(),
            s.channels,
        );
    }
}

/// P37 — wire shape for `MediaStream.ReplayGain`. Floats are in dB
/// (centidecibels divided by 100). Both fields skip when absent so
/// clients fall back to unity gain rather than -infinity.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ReplayGainDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_gain: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_gain: Option<f32>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct UserItemDataDto {
    /// Non-nullable Guid in the C# DTO — always on the wire from real
    /// Jellyfin; strict SDK clients require it. Same id string the item DTOs
    /// use.
    pub item_id: String,
    pub played: bool,
    pub play_count: u32,
    /// Resume position in Jellyfin's 100ns ticks.
    pub playback_position_ticks: u64,
    pub played_percentage: f32,
    pub is_favorite: bool,
    /// `IsFolder` field is unrelated to UserData but jellyfin-web's
    /// item-data renderer reads `Likes` / `Rating` on this object —
    /// `null` would crash the optional chain in older builds.
    pub likes: Option<bool>,
    pub rating: Option<f32>,
    pub key: String,
    pub last_played_date: Option<String>,
}

impl UserItemDataDto {
    pub fn from_domain(item_id: pharos_core::MediaId, data: pharos_core::UserItemData) -> Self {
        Self::from_domain_with_runtime(item_id, data, 0)
    }

    /// Like [`Self::from_domain`] but computes `PlayedPercentage` from the
    /// item's runtime (100ns ticks) when known — jellyfin-web draws card
    /// resume bars from this field, so a hardcoded 0 blanks them (B36).
    /// `runtime_ticks == 0` (unknown) keeps the old behaviour.
    pub fn from_domain_with_runtime(
        item_id: pharos_core::MediaId,
        data: pharos_core::UserItemData,
        runtime_ticks: u64,
    ) -> Self {
        let played_percentage = if runtime_ticks > 0 {
            ((data.last_played_position_ticks as f64 / runtime_ticks as f64) * 100.0)
                .clamp(0.0, 100.0) as f32
        } else {
            0.0
        };
        Self {
            item_id: wire_item_id(item_id),
            played: data.played,
            play_count: data.play_count,
            playback_position_ticks: data.last_played_position_ticks,
            played_percentage,
            is_favorite: data.is_favorite,
            likes: None,
            rating: None,
            key: item_id.to_string(),
            last_played_date: if data.last_played_at > 0 {
                Some(format_iso8601(data.last_played_at))
            } else {
                None
            },
        }
    }

    /// UserData for a SYNTHETIC folder / stub whose id is a wire STRING (not a
    /// numeric [`pharos_core::MediaId`]) — a library `CollectionFolder`, a
    /// Series/Season, an album aggregate. Emits the full kotlin-required field
    /// set (B68) with a folder's zeroed resume position. Typed (not a
    /// `serde_json::json!` literal) so the required set can't silently drift
    /// and re-crash a strict native client (B78/V38).
    pub fn folder(item_id: &str, played: bool, play_count: u32, is_favorite: bool) -> Self {
        Self {
            item_id: item_id.to_string(),
            played,
            play_count,
            playback_position_ticks: 0,
            played_percentage: 0.0,
            is_favorite,
            likes: None,
            rating: None,
            key: item_id.to_string(),
            last_played_date: None,
        }
    }
}

/// Minimal ISO-8601 (Z) formatter for the `LastPlayedDate` field —
/// avoids pulling in `chrono` just for one render path. T58 phase 3
/// reuses it from the admin module for `/Auth/Keys` DateCreated.
/// Canonical wire form of a library item id: the u64 zero-padded into a
/// dashless 32-hex GUID (`00000000000000000218…`). Jellyfin item ids ARE
/// Guids, and the native apps parse every id with `toUUIDOrNull()` —
/// jellyfin-android's WebView→native bridge silently DROPS non-UUID ids, so
/// a decimal id string empties the play queue and the native player fails
/// with "Unable to resolve playback info" (B15). Matches the 32-hex shape
/// [`series_id_for_key`] already uses for synthetic series ids.
pub fn wire_item_id(id: pharos_core::MediaId) -> String {
    format!("{id:032x}")
}

/// A parsed inbound wire id, with the real-vs-synthetic distinction made
/// at the PARSE boundary instead of by ad-hoc high-half masking at use
/// sites ("make incorrect states unrepresentable").
///
/// The two namespaces are disjoint by construction:
/// - **Real** library item ids serialize as the u64 zero-padded to 32 hex
///   (high half all zeros — see [`wire_item_id`]).
/// - **Synth** group ids (series/season/artist/album/genre/tag/collection)
///   are a 63-bit xxh3 hash emitted DUPLICATED (`{h:016x}{h:016x}`, see
///   [`series_id_for_key`] et al.), so their high half is non-zero and
///   equals the low half.
///
/// A handler that can only serve real media takes the `Real` arm and gets
/// a type-checked guarantee no synth id reaches the store; a browse
/// handler that groups by synth ids matches both arms EXPLICITLY — the
/// silent parse-fail-to-empty behaviour of funnelling everything through
/// [`parse_item_id`] is what made synth-id routes return empty instead of
/// resolving (B32-class bugs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WireId {
    /// A library media item backed by a store row.
    Real(pharos_core::MediaId),
    /// A synthesised group id (the 63-bit hash, i.e. the repeated half).
    /// Never a store key — resolve by re-hashing candidate names/folders.
    Synth(u64),
}

impl WireId {
    /// Parse an incoming id in ANY of the shapes clients send back:
    /// - the canonical dashless 32-hex GUID
    /// - the same GUID with dashes (some SDKs re-serialize UUIDs dashed)
    /// - the legacy plain-decimal form (pre-B15 clients / open sessions;
    ///   always a REAL id — synth ids never had a decimal form)
    ///
    /// Returns `None` for foreign GUIDs in neither namespace (e.g. a user
    /// id, or another server's item id pasted into a URL).
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if s.len() == 36 && s.bytes().filter(|b| *b == b'-').count() == 4 {
            let undashed: String = s.chars().filter(|c| *c != '-').collect();
            return Self::parse(&undashed);
        }
        if s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
            let (hi, lo) = s.split_at(16);
            if hi.bytes().all(|b| b == b'0') {
                return pharos_core::MediaId::from_str_radix(lo, 16)
                    .ok()
                    .map(Self::Real);
            }
            // Synth namespace: the 63-bit hash duplicated into both halves.
            let hi = u64::from_str_radix(hi, 16).ok()?;
            let lo = u64::from_str_radix(lo, 16).ok()?;
            if hi == lo {
                return Some(Self::Synth(lo));
            }
            // GUID-shaped but in neither id namespace.
            return None;
        }
        s.parse::<pharos_core::MediaId>().ok().map(Self::Real)
    }

    /// The real media id, if this is one. The type-checked replacement for
    /// "parse then hope it wasn't synthetic".
    pub fn real(self) -> Option<pharos_core::MediaId> {
        match self {
            Self::Real(id) => Some(id),
            Self::Synth(_) => None,
        }
    }
}

impl std::fmt::Display for WireId {
    /// Canonical wire form — round-trips through [`WireId::parse`].
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Real(id) => write!(f, "{id:032x}"),
            Self::Synth(h) => write!(f, "{h:016x}{h:016x}"),
        }
    }
}

/// Parse an incoming item id in ANY of the shapes clients send back:
/// - the canonical dashless 32-hex GUID (leading 16 zeros → pharos u64)
/// - the same GUID with dashes (some SDKs re-serialize UUIDs dashed)
/// - the legacy plain-decimal form (pre-B15 clients / open sessions)
///
/// REAL ids only — a synthetic (series/artist/…) id returns `None`.
/// Handlers that must also accept synth ids match on [`WireId::parse`]
/// instead, which makes the two namespaces explicit.
pub fn parse_item_id(s: &str) -> Option<pharos_core::MediaId> {
    WireId::parse(s).and_then(WireId::real)
}

pub fn format_iso8601(unix_secs: i64) -> String {
    // Constants: days/month etc. Use the same algorithm as
    // chrono::NaiveDateTime::from_timestamp — straightforward Gregorian
    // calendar arithmetic. Good enough for "last played" display.
    let secs_per_day: i64 = 86_400;
    let mut days = unix_secs.div_euclid(secs_per_day);
    let mut secs_of_day = unix_secs.rem_euclid(secs_per_day);
    let hh = secs_of_day / 3600;
    secs_of_day %= 3600;
    let mm = secs_of_day / 60;
    let ss = secs_of_day % 60;
    // Days since 1970-01-01 → Gregorian Y-M-D.
    let mut year: i64 = 1970;
    loop {
        let dy: i64 = if is_leap(year) { 366 } else { 365 };
        if days < dy {
            break;
        }
        days -= dy;
        year += 1;
    }
    let months: [i64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut month = 0;
    while month < 12 {
        let dm = if month == 1 && is_leap(year) {
            29
        } else {
            months[month]
        };
        if days < dm {
            break;
        }
        days -= dm;
        month += 1;
    }
    let day = days + 1;
    format!(
        "{year:04}-{:02}-{:02}T{:02}:{:02}:{:02}.0000000Z",
        month as i32 + 1,
        day,
        hh,
        mm,
        ss
    )
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Millisecond-precision ISO-8601 (Z) formatter — `YYYY-MM-DDTHH:MM:SS.mmmZ`.
/// SyncPlay clock sync needs sub-second `When`/`EmittedAt`/`GetUtcTime`
/// timestamps: whole-second precision (as [`format_iso8601`] emits, with a
/// zeroed `.0000000` fraction) leaves ±1 s of clock error, enough to desync a
/// group. The client parses this with `new Date(...)`, which accepts a 3-digit
/// fractional part.
pub fn format_iso8601_ms(unix_ms: i64) -> String {
    let secs = unix_ms.div_euclid(1000);
    let millis = unix_ms.rem_euclid(1000);
    let secs_per_day: i64 = 86_400;
    let mut days = secs.div_euclid(secs_per_day);
    let mut secs_of_day = secs.rem_euclid(secs_per_day);
    let hh = secs_of_day / 3600;
    secs_of_day %= 3600;
    let mm = secs_of_day / 60;
    let ss = secs_of_day % 60;
    let mut year: i64 = 1970;
    loop {
        let dy: i64 = if is_leap(year) { 366 } else { 365 };
        if days < dy {
            break;
        }
        days -= dy;
        year += 1;
    }
    let months: [i64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut month = 0;
    while month < 12 {
        let dm = if month == 1 && is_leap(year) {
            29
        } else {
            months[month]
        };
        if days < dm {
            break;
        }
        days -= dm;
        month += 1;
    }
    let day = days + 1;
    format!(
        "{year:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        month as i32 + 1,
        day,
        hh,
        mm,
        ss,
        millis
    )
}

/// Jellyfin `BaseItemDtoQueryResult` — the paged-list envelope every browse /
/// search / list endpoint returns. All three fields are no-default (required)
/// in the kotlin SDK (`Items: List<BaseItemDto>`, `TotalRecordCount: Int`,
/// `StartIndex: Int`); a native client fails the whole page on any omission,
/// so the wire form must always carry all three (B78/V38).
///
/// Generic over the element type (default `BaseItemDto`) so synth pages whose
/// rows are `SynthItemDto` — or already-serialized `serde_json::Value` — reuse
/// the same envelope instead of a hand-built `json!` literal. `T` is inferred
/// from `items` at every call site; the default only names the common case.
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ItemsResultDto<T = BaseItemDto> {
    pub items: Vec<T>,
    pub total_record_count: u32,
    pub start_index: u32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct VirtualFolderInfoDto {
    pub name: String,
    pub locations: Vec<String>,
    pub collection_type: &'static str,
    pub item_id: String,
    pub library_options: VirtualFolderOptionsDto,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct VirtualFolderOptionsDto {
    pub enable_photos: bool,
    pub enable_realtime_monitor: bool,
}

impl BaseItemDto {
    pub fn from_domain(item: &pharos_core::MediaItem, server_id: &str) -> Self {
        Self::from_domain_with_user_data(item, server_id, pharos_core::UserItemData::default())
    }

    /// Populate `Trickplay` with the layout map for the configured
    /// widths. No-op when widths is empty or the probe lacks the
    /// duration / dimensions required to compute a layout.
    pub fn with_trickplay(
        mut self,
        probe: &pharos_core::MediaProbe,
        widths: &[u32],
        interval_ms: u32,
    ) -> Self {
        if widths.is_empty() {
            return self;
        }
        let inner = build_dto_layout_map(probe, widths, interval_ms);
        if inner.is_empty() {
            return self;
        }
        // Jellyfin's `Trickplay` is a double-nested map keyed first by
        // media-source id, then by width. jellyfin-web looks up
        // `item.Trickplay[mediaSourceId]` and requests no tiles when that
        // key is missing, so a flat `{ width → TileInfo }` map (which is
        // what `build_dto_layout_map` returns) silently disables previews.
        // For pharos single-file items the media-source id is the item id
        // string (see `build`: `media_sources[0].id == item.id.to_string()`).
        let mut outer = serde_json::Map::new();
        outer.insert(self.id.clone(), serde_json::Value::Object(inner));
        self.trickplay = outer;
        self
    }

    /// LIB-D5 — advertise additional `ImageTags` for roles that have a
    /// recorded local sidecar (D4 `artwork` rows). `roles` are the
    /// `ArtworkRole::as_str` tokens (`"Logo"` / `"Banner"` / `"Disc"` /
    /// `"Art"`, and also `"Primary"` / `"Backdrop"` / `"Thumb"` when a
    /// sidecar overrides the frame-extract default). Purely additive:
    /// the frame-extract Primary/Backdrop/Thumb tags `image_tags_for`
    /// already set stay in place; this fills the upload-only roles a
    /// client otherwise wouldn't request. Backdrop, being a list role,
    /// also gets its tag appended to `backdrop_image_tags` if absent.
    pub fn with_local_artwork_tags(mut self, id: u64, roles: &[String]) -> Self {
        for role in roles {
            if role.eq_ignore_ascii_case("backdrop") {
                let tag = image_tag_for(id, "backdrop");
                if !self.backdrop_image_tags.contains(&tag) {
                    self.backdrop_image_tags.push(tag);
                }
            }
            self.image_tags.entry(role.clone()).or_insert_with(|| {
                serde_json::Value::String(image_tag_for(id, &role.to_ascii_lowercase()))
            });
        }
        self
    }

    /// LIB-C2 — project an item's resolved cast/crew (from the
    /// `item_people` join, in NFO order) onto `People`. Each entry's `Id`
    /// is the person's `wire_id` (= [`person_id_for`] of the name), so a
    /// client click routes to `/Items?ParentId=<wire id>`; `Role` carries
    /// the played character for cast (falling back to the free-form role
    /// string for crew), and `Type` is the Jellyfin `PersonType` token.
    /// Replaces the hardcoded empty `people` `build` set.
    pub fn with_people(mut self, people: &[pharos_core::ItemPerson]) -> Self {
        self.people = people
            .iter()
            .map(|p| {
                // Jellyfin's `Role` on a cast credit is the character;
                // crew (no character) fall back to the free-form role.
                let role = p
                    .character
                    .clone()
                    .filter(|c| !c.is_empty())
                    .or_else(|| p.role.clone().filter(|r| !r.is_empty()))
                    .unwrap_or_default();
                // T79 — advertise a PrimaryImageTag ONLY when the person has a
                // servable portrait (an `http(s)` thumb_url resolved by T81).
                // A NULL / legacy-local-path thumb → no tag, so jellyfin-web
                // never requests a photo that would 404. The tag is derived
                // from the url so it changes when the portrait does (cache-bust);
                // pharos's image route serves by wire id and ignores its value.
                let primary_image_tag = p
                    .thumb_url
                    .as_deref()
                    .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
                    .map(person_image_tag_for);
                PersonDto {
                    name: p.name.clone(),
                    id: p.wire_id.clone(),
                    role,
                    kind: p.kind.as_str(),
                    primary_image_tag,
                }
            })
            .collect();
        self
    }

    /// LIB-C3 — project an item's resolved studios (from the `item_studios`
    /// join, name-ordered) onto `Studios`. Each pair's `Id` is the studio's
    /// `wire_id` (= [`studio_id_for`] of the name), so a client click routes
    /// to `/Items?ParentId=<wire id>`. Replaces the hardcoded empty
    /// `studios` `build` set.
    pub fn with_studios(mut self, studios: &[pharos_core::Studio]) -> Self {
        self.studios = studios
            .iter()
            .map(|s| NameGuidPairDto {
                name: s.name.clone(),
                id: s.wire_id.clone(),
            })
            .collect();
        self
    }

    /// LIB-C6 — project an item's resolved tags (from the `item_tags`
    /// join, name-ordered) onto `Tags`. Jellyfin's `Tags` is a flat
    /// `Vec<String>` of label names (no wire id; a tag's
    /// `/Items?ParentId=<tag id>` link comes from the /Tags list, not the
    /// item DTO), so we emit the bare names. Replaces the hardcoded empty
    /// `tags` `build` set.
    pub fn with_tags(mut self, tags: &[pharos_core::Tag]) -> Self {
        self.tags = tags.iter().map(|t| t.name.clone()).collect();
        self
    }

    pub fn from_domain_with_user_data(
        item: &pharos_core::MediaItem,
        server_id: &str,
        user_data: pharos_core::UserItemData,
    ) -> Self {
        let mut dto = Self::build(item, server_id);
        dto.user_data = UserItemDataDto::from_domain(item.id, user_data);
        dto
    }

    fn build(item: &pharos_core::MediaItem, server_id: &str) -> Self {
        let kind = match item.kind {
            pharos_core::MediaKind::Movie => "Movie",
            pharos_core::MediaKind::Episode => "Episode",
            pharos_core::MediaKind::Audio => "Audio",
        };
        let media_type = match item.kind {
            pharos_core::MediaKind::Audio => "Audio",
            _ => "Video",
        };
        let is_video = !matches!(item.kind, pharos_core::MediaKind::Audio);
        let probe = &item.probe;
        let container = container_for(probe, is_video);
        let run_time_ticks = probe.run_time_ticks().unwrap_or(0);

        let media_streams = build_media_streams(probe, is_video);
        let default_audio_stream_index: Option<u32> = media_streams
            .iter()
            .find(|s| s.kind == "Audio")
            .map(|s| s.index);

        Self {
            id: wire_item_id(item.id),
            name: item.title.clone(),
            server_id: server_id.to_string(),
            kind,
            media_type,
            is_folder: false,
            user_data: UserItemDataDto {
                item_id: wire_item_id(item.id),
                ..Default::default()
            },
            run_time_ticks,
            location_type: "FileSystem",
            can_play: true,
            play_access: "Full",
            media_sources: vec![MediaSourceLiteDto {
                id: wire_item_id(item.id),
                container,
                kind: "Default",
                is_remote: false,
                supports_direct_play: true,
                supports_direct_stream: true,
                supports_transcoding: true,
                read_at_native_framerate: false,
                ignore_dts: false,
                ignore_index: false,
                gen_pts_input: false,
                is_infinite_stream: false,
                has_segments: false,
                requires_opening: false,
                requires_closing: false,
                requires_looping: false,
                supports_probing: true,
                transcoding_sub_protocol: "http",
                run_time_ticks: probe.run_time_ticks(),
                protocol: "File",
                media_streams,
                bitrate: probe.bitrate_bps,
                size: probe.size_bytes,
                name: item.title.clone(),
                default_audio_stream_index,
                video_type: "VideoFile",
                e_tag: "0".into(),
            }],
            artists: item.probe.artist.iter().cloned().collect(),
            artist_items: item
                .probe
                .artist
                .as_ref()
                .map(|name| {
                    vec![NameGuidPairDto {
                        name: name.clone(),
                        id: artist_id_for(name),
                    }]
                })
                .unwrap_or_default(),
            album_artists: item
                .probe
                .album_artist
                .as_ref()
                .map(|name| {
                    vec![NameGuidPairDto {
                        name: name.clone(),
                        id: artist_id_for(name),
                    }]
                })
                .unwrap_or_default(),
            genres: item.probe.genre.iter().cloned().collect(),
            genre_items: item
                .probe
                .genre
                .as_ref()
                .map(|name| {
                    vec![NameGuidPairDto {
                        name: name.clone(),
                        id: genre_id_for(name),
                    }]
                })
                .unwrap_or_default(),
            tags: vec![],
            studios: vec![],
            people: vec![],
            production_locations: item.metadata.production_locations.clone(),
            provider_ids: provider_ids_map(&item.metadata.provider_ids),
            remote_trailers: remote_trailers(item),
            chapters: item
                .probe
                .chapters
                .iter()
                .map(|c| ChapterInfoDto {
                    name: c.title.clone(),
                    // `StartPositionTicks` is Jellyfin's 100-ns unit
                    // (10_000 ticks / ms).
                    start_position_ticks: c.start_ms.saturating_mul(10_000),
                    image_date_modified: "0001-01-01T00:00:00.0000000Z",
                })
                .collect(),
            trickplay: serde_json::Map::new(),
            external_urls: external_urls(item),
            image_tags: image_tags_for(item),
            // Audio items have no video frames, so the image handler 404s a
            // Backdrop request for them (api::jellyfin::images). Keep this list
            // consistent with `image_tags_for` (which already omits Backdrop
            // for Audio) — otherwise jellyfin-web requests a backdrop that can
            // only 404, tripping the strict-console compat spec.
            backdrop_image_tags: if matches!(item.kind, pharos_core::MediaKind::Audio) {
                vec![]
            } else {
                vec![image_tag_for(item.id, "backdrop")]
            },
            screenshot_image_tags: vec![],
            date_created: item.created_at.map(format_iso8601),
            // ParentId default: SeasonId for episodes, AlbumId for
            // audio with album tag, else None (handler fills with the
            // library root id; we can't compute it without
            // media_roots).
            // LIB-C11 — series/season ids key on the show FOLDER (via
            // `series_key()`), so same-name shows don't merge. Falls back
            // to the bare name for legacy rows lacking a folder.
            parent_id: item
                .series
                .as_ref()
                .and_then(|s| {
                    s.season_number
                        .map(|n| season_id_for_key(s.series_folder.as_deref(), &s.series_name, n))
                })
                .or_else(|| item.probe.album.as_deref().map(album_id_for)),
            album: item.probe.album.clone(),
            album_id: item.probe.album.as_deref().map(album_id_for),
            series_name: item.series.as_ref().map(|s| s.series_name.clone()),
            series_id: item
                .series
                .as_ref()
                .map(|s| series_id_for_key(s.series_folder.as_deref(), &s.series_name)),
            season_id: item.series.as_ref().and_then(|s| {
                s.season_number
                    .map(|n| season_id_for_key(s.series_folder.as_deref(), &s.series_name, n))
            }),
            season_name: item
                .series
                .as_ref()
                .and_then(|s| s.season_number.map(season_display_name)),
            // Episodes: season/episode. Audio tracks: disc/track (drives
            // jellyfin-web's album track list numbering + ordering).
            parent_index_number: item
                .series
                .as_ref()
                .and_then(|s| s.season_number)
                .or(item.probe.disc_number),
            index_number: item
                .series
                .as_ref()
                .and_then(|s| s.episode_number)
                .or(item.probe.track_number),
            // P13 — VideoRange is HDR-only when the probe says so.
            // Audio items + SDR videos skip the field entirely.
            video_range: if is_video && item.probe.is_hdr() {
                Some("HDR")
            } else {
                None
            },
            // LIB-C7/C8/C9 — descriptive metadata projection. Absent
            // values stay None / empty so the wire shape is unchanged
            // for un-enriched items.
            overview: item.metadata.overview.clone(),
            taglines: item.metadata.tagline.iter().cloned().collect(),
            community_rating: item.metadata.community_rating,
            critic_rating: item.metadata.critic_rating,
            official_rating: item.metadata.official_rating.clone(),
            // LIB-C11 — fall back to the series folder's parsed year so
            // same-name shows (Cosmos 1980 vs 2014) are distinguishable
            // even before per-episode metadata enrichment (EPIC D).
            production_year: item
                .metadata
                .production_year
                .or_else(|| item.series.as_ref().and_then(|s| s.series_year))
                // Audio: the release-year tag (album sort + track detail).
                .or(item.probe.year),
            premiere_date: item.metadata.premiere_date.map(format_iso8601),
        }
    }
}

/// T67 — build `BaseItemDto.ExternalUrls` (the "IMDb / TheMovieDb" links
/// jellyfin-web renders on the detail page) from the item's provider ids. Pure
/// derivation from data already on the item — no store lookup — so it populates
/// list rows and detail alike. TMDb links are type-scoped (`/movie` vs `/tv`).
fn external_urls(item: &pharos_core::MediaItem) -> Vec<serde_json::Value> {
    let ids = &item.metadata.provider_ids;
    let mut out = Vec::new();
    let mut push = |name: &str, url: String| {
        out.push(serde_json::json!({ "Name": name, "Url": url }));
    };
    if let Some(imdb) = &ids.imdb {
        push("IMDb", format!("https://www.imdb.com/title/{imdb}/"));
    }
    if let Some(tmdb) = &ids.tmdb {
        let seg = if matches!(item.kind, pharos_core::MediaKind::Movie) {
            "movie"
        } else {
            "tv"
        };
        push(
            "TheMovieDb",
            format!("https://www.themoviedb.org/{seg}/{tmdb}"),
        );
    }
    if let Some(tvdb) = &ids.tvdb {
        push(
            "TheTVDB",
            format!("https://thetvdb.com/?tab=series&id={tvdb}"),
        );
    }
    if let Some(mbid) = &ids.mbid {
        push(
            "MusicBrainz",
            format!("https://musicbrainz.org/recording/{mbid}"),
        );
    }
    out
}

/// T67 — build `BaseItemDto.RemoteTrailers` (jellyfin-web's "Trailer" button)
/// from the item's trailer URLs (Kodi NFO `<trailer>`). Jellyfin's `MediaUrl`
/// shape is `{Url, Name}`; a single generic "Trailer" name matches what real
/// Jellyfin emits when the source carries no per-trailer title.
fn remote_trailers(item: &pharos_core::MediaItem) -> Vec<serde_json::Value> {
    item.metadata
        .trailers
        .iter()
        .map(|url| serde_json::json!({ "Url": url, "Name": "Trailer" }))
        .collect()
}

/// LIB-C9 — project core [`ProviderIds`](pharos_core::ProviderIds) into
/// the Jellyfin `ProviderIds` map under canonical provider keys. Absent
/// ids are omitted so the map is empty when nothing is known.
fn provider_ids_map(ids: &pharos_core::ProviderIds) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    let mut insert = |key: &str, val: &Option<String>| {
        if let Some(v) = val {
            map.insert(key.to_string(), serde_json::Value::String(v.clone()));
        }
    };
    insert("Tmdb", &ids.tmdb);
    insert("Tvdb", &ids.tvdb);
    insert("Imdb", &ids.imdb);
    insert("MusicBrainzTrack", &ids.mbid);
    map
}

/// Advertise the image roles ImageCache can produce for this item.
/// Tag value is a stable per-(item, role) hash — jellyfin-web uses
/// it as the `?tag=` cache-buster on the image URL. We don't have
/// real ETags yet (re-extraction is deterministic per item) so the
/// hash IS the version.
pub fn image_tags_for(item: &pharos_core::MediaItem) -> serde_json::Map<String, serde_json::Value> {
    let mut m = serde_json::Map::new();
    // Every item gets a Primary tag — Audio uses cover-art if embedded,
    // Video uses a frame extracted at seek_seconds. Backdrop/Thumb only
    // make sense for video.
    m.insert(
        "Primary".into(),
        serde_json::Value::String(image_tag_for(item.id, "primary")),
    );
    if !matches!(item.kind, pharos_core::MediaKind::Audio) {
        m.insert(
            "Backdrop".into(),
            serde_json::Value::String(image_tag_for(item.id, "backdrop")),
        );
        m.insert(
            "Thumb".into(),
            serde_json::Value::String(image_tag_for(item.id, "thumb")),
        );
    }
    m
}

/// Per-(item, role) stable hex tag (16 chars — Jellyfin's `?tag=` is
/// usually a hex string and the length doesn't matter to the client).
pub fn image_tag_for(item_id: u64, role: &str) -> String {
    use xxhash_rust::xxh3::xxh3_64;
    let h = xxh3_64(format!("img:{item_id}:{role}").as_bytes()) & 0x7FFFFFFFFFFFFFFF;
    format!("{h:016x}")
}

/// T79 — a stable hex tag for a person's portrait, derived from its resolved
/// url so it rotates when the portrait changes (cache-busting) yet stays
/// stable across renders of the same url. pharos's `/Items/{personWireId}/
/// Images/Primary` route serves by wire id and ignores the tag value — the
/// tag's only job is to be present + stable so jellyfin-web requests the photo.
pub fn person_image_tag_for(thumb_url: &str) -> String {
    use xxhash_rust::xxh3::xxh3_64;
    let h = xxh3_64(thumb_url.as_bytes()) & 0x7FFFFFFFFFFFFFFF;
    format!("{h:016x}")
}

/// Stable per-name 32-hex ids for the synthesised Artist / Album /
/// Genre / Studio aggregate items. Drives /Items?ParentId=… joins
/// + the NameGuidPair links jellyfin-web renders in track tiles.
pub fn artist_id_for(name: &str) -> String {
    name_aggregate_id_for("artist", name)
}
pub fn album_id_for(name: &str) -> String {
    name_aggregate_id_for("album", name)
}
pub fn genre_id_for(name: &str) -> String {
    // LIB-C4 — delegate to core so the id matches the `genres.wire_id`
    // the store stamps at upsert (single source of truth for the hash).
    pharos_core::genre_wire_id(name)
}
/// LIB-C2 — stable 32-hex id for a synthesised Person item. Delegates to
/// core's `person_wire_id` so the `Id` a Person DTO emits is byte-
/// identical to the `people.wire_id` the store stamps at upsert (single
/// source of truth) — `/Items?ParentId=<person id>` resolves by that
/// wire_id index.
pub fn person_id_for(name: &str) -> String {
    pharos_core::person_wire_id(name)
}
/// LIB-C3 — stable 32-hex id for a synthesised Studio item. Delegates to
/// core's `studio_wire_id` so the `Id` a Studio DTO emits is byte-
/// identical to the `studios.wire_id` the store stamps at upsert (single
/// source of truth) — `/Items?ParentId=<studio id>` resolves by that
/// wire_id index.
pub fn studio_id_for(name: &str) -> String {
    pharos_core::studio_wire_id(name)
}
/// LIB-C5 — stable 32-hex id for a synthesised Collection / BoxSet item.
/// Delegates to core's `collection_wire_id` so the `Id` a BoxSet DTO
/// emits is byte-identical to the `collections.wire_id` the store stamps
/// at upsert (single source of truth) — `/Items/{id}` resolves the
/// BoxSet and `/Items?ParentId=<collection id>` lists its members by
/// that wire_id index.
pub fn collection_id_for(name: &str) -> String {
    pharos_core::collection_wire_id(name)
}
/// LIB-C6 — stable 32-hex id for a synthesised Tag item. Delegates to
/// core's `tag_wire_id` so the `Id` a Tag DTO emits is byte-identical to
/// the `tags.wire_id` the store stamps at upsert (single source of truth)
/// — `/Items?ParentId=<tag id>` resolves by that wire_id index.
pub fn tag_id_for(name: &str) -> String {
    pharos_core::tag_wire_id(name)
}
fn name_aggregate_id_for(kind: &str, name: &str) -> String {
    use xxhash_rust::xxh3::xxh3_64;
    let h = xxh3_64(format!("{kind}:{name}").as_bytes()) & 0x7FFFFFFFFFFFFFFF;
    format!("{h:016x}{h:016x}")
}

/// Stable 32-hex id for the synthesised Series item, keyed on the bare
/// series NAME.
///
/// LIB-C11 — this name-only variant is the *legacy* identity. Two shows
/// that share a name collapse to one id here, interleaving their
/// episodes. Prefer [`series_id_for_key`] (folder-keyed) for newly
/// scanned items; this stays as the fallback for rows lacking a folder
/// so existing client URLs survive the migration.
pub fn series_id_for(name: &str) -> String {
    use xxhash_rust::xxh3::xxh3_64;
    let h = xxh3_64(format!("series:{name}").as_bytes()) & 0x7FFFFFFFFFFFFFFF;
    format!("{h:016x}{h:016x}")
}

/// LIB-C11 — stable 32-hex Series id keyed on the show's FOLDER path when
/// known, falling back to the bare NAME otherwise. Folder-keyed ids make
/// same-name shows (`Cosmos (1980)` vs `Cosmos (2014)`) distinct so their
/// episodes don't interleave. Passing `None` is byte-identical to
/// [`series_id_for`], so legacy/backfilled rows keep their existing wire
/// id and client URLs keep resolving.
pub fn series_id_for_key(folder: Option<&str>, name: &str) -> String {
    match folder {
        Some(f) => {
            use xxhash_rust::xxh3::xxh3_64;
            let h = xxh3_64(format!("series-folder:{f}").as_bytes()) & 0x7FFFFFFFFFFFFFFF;
            format!("{h:016x}{h:016x}")
        }
        None => series_id_for(name),
    }
}

/// Stable 32-hex id for the synthesised Season item, keyed on the bare
/// series NAME + season number. LIB-C11 legacy variant — see
/// [`season_id_for_key`].
pub fn season_id_for(series_name: &str, season_number: u32) -> String {
    use xxhash_rust::xxh3::xxh3_64;
    let h =
        xxh3_64(format!("season:{series_name}:{season_number}").as_bytes()) & 0x7FFFFFFFFFFFFFFF;
    format!("{h:016x}{h:016x}")
}

/// LIB-C11 — stable 32-hex Season id keyed on the show's FOLDER path (+
/// season number) when known, falling back to the bare NAME otherwise.
/// Keeps a season under the right folder-keyed series. `None` folder is
/// byte-identical to [`season_id_for`] so legacy rows are unaffected.
pub fn season_id_for_key(folder: Option<&str>, series_name: &str, season_number: u32) -> String {
    match folder {
        Some(f) => {
            use xxhash_rust::xxh3::xxh3_64;
            let h = xxh3_64(format!("season-folder:{f}:{season_number}").as_bytes())
                & 0x7FFFFFFFFFFFFFFF;
            format!("{h:016x}{h:016x}")
        }
        None => season_id_for(series_name, season_number),
    }
}

/// Human-readable season name. "Specials" for 0, "Season N" otherwise.
pub fn season_display_name(n: u32) -> String {
    if n == 0 {
        "Specials".into()
    } else {
        format!("Season {n}")
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct SessionInfoDto {
    pub id: String,
    pub user_id: String,
    pub user_name: String,
    pub device_id: String,
    pub device_name: String,
    pub client: String,
    pub application_version: String,
    pub server_id: String,
    // Non-nullable value-type fields of Jellyfin's C# SessionInfoDto — always
    // on the wire from real Jellyfin, required by strict SDK clients.
    pub last_activity_date: String,
    pub last_playback_check_in: String,
    pub is_active: bool,
    pub supports_media_control: bool,
    pub supports_remote_control: bool,
    pub has_custom_device_name: bool,
    /// NON-nullable lists in the jellyfin-sdk-kotlin `SessionInfoDto`
    /// (`playableMediaTypes: List<MediaType>`, `supportedCommands:
    /// List<GeneralCommandType>` — no `?`). Omitting them made the Android/
    /// Google-TV SDK throw while parsing the AuthenticationResult of the Quick
    /// Connect finalize, surfaced on the TV as "Unable to connect to server"
    /// even though the server issued the token fine. Empty lists satisfy the
    /// non-null contract (B63); jellyfin-web ignored their absence.
    pub playable_media_types: Vec<String>,
    pub supported_commands: Vec<String>,
}

/// Pick a container string for the wire response. ffprobe reports
/// `format_name` as a comma-joined alias list (e.g. `"matroska,webm"`).
/// Jellyfin's DirectPlayProfile matches single tokens, so we have to
/// pick one. Prefer the more specific alias when it matches a profile
/// pharos commonly emits (webm > matroska, m4v > mp4), since the file
/// actually IS the more specific one — the `mkv` muxer accepts webm,
/// but a `.webm` file's clients pick the webm profile, not matroska.
/// Falls back to a kind-default when no probe ran, because an empty
/// Container makes jellyfin-web pick Transcode with no TranscodingUrl
/// → "Playback Error" dialog (caught in dev).
pub fn container_for(probe: &pharos_core::MediaProbe, is_video: bool) -> String {
    const PREFERRED: &[&str] = &["webm", "m4v", "mp4", "mp3", "flac", "ogg", "opus", "aac"];
    if let Some(c) = probe.container.as_deref() {
        let aliases: Vec<&str> = c
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        for pref in PREFERRED {
            if aliases.iter().any(|a| a.eq_ignore_ascii_case(pref)) {
                return (*pref).to_string();
            }
        }
        if let Some(first) = aliases.first() {
            return first.to_string();
        }
    }
    if is_video {
        "mp4".to_string()
    } else {
        "mp3".to_string()
    }
}

/// Per-item subtitle source. `item_id` is the MediaItem the stream
/// belongs to. `delivery_url` carries the URL the client fetches the
/// rendered .vtt from — built per call so callers can override the
/// host prefix (PlaybackInfo + Items use a relative URL).
pub struct SubtitleStreamCtx {
    pub item_id: pharos_core::MediaId,
    pub sidecar_count: u32,
    /// Per-sidecar language (index = offset), parsed from the filename by the
    /// caller. `None` where the filename carried no language token. Labels each
    /// external track with its language instead of a bare "External N".
    pub sidecar_langs: Vec<Option<String>>,
}

impl SubtitleStreamCtx {
    pub fn new(item_id: pharos_core::MediaId) -> Self {
        Self {
            item_id,
            sidecar_count: 0,
            sidecar_langs: Vec::new(),
        }
    }
}

pub fn build_media_streams(probe: &pharos_core::MediaProbe, is_video: bool) -> Vec<MediaStreamDto> {
    build_media_streams_with_subtitles(probe, is_video, None)
}

/// P37 — convert an AudioTrack's stored centidecibel ReplayGain
/// fields back to dB. Both fields stay independently `Option<f32>`
/// so a track with only album-level RG (FLAC pressings + a single
/// tag run) doesn't emit a zero TrackGain that would silence the
/// player.
fn build_replay_gain(t: &pharos_core::AudioTrack) -> Option<ReplayGainDto> {
    let track = t.replaygain_track_centidb.map(|c| c as f32 / 100.0);
    let album = t.replaygain_album_centidb.map(|c| c as f32 / 100.0);
    if track.is_none() && album.is_none() {
        return None;
    }
    Some(ReplayGainDto {
        track_gain: track,
        album_gain: album,
    })
}

pub fn build_media_streams_with_subtitles(
    probe: &pharos_core::MediaProbe,
    is_video: bool,
    subtitle_ctx: Option<&SubtitleStreamCtx>,
) -> Vec<MediaStreamDto> {
    let mut streams = Vec::with_capacity(if is_video { 2 } else { 1 });
    if is_video {
        let aspect_ratio = match (probe.width, probe.height) {
            (Some(w), Some(h)) if h != 0 => Some(format!("{w}:{h}")),
            _ => None,
        };
        let fps = probe.frame_rate_f32();
        streams.push(MediaStreamDto {
            kind: "Video",
            index: 0,
            codec: probe.video_codec.clone(),
            is_default: true,
            width: probe.width,
            height: probe.height,
            channels: None,
            sample_rate: None,
            bit_rate: probe.bitrate_bps,
            aspect_ratio,
            real_frame_rate: fps,
            average_frame_rate: fps,
            language: None,
            title: None,
            is_external: false,
            is_forced: false,
            is_hearing_impaired: false,
            is_interlaced: false,
            is_original: false,
            delivery_url: None,
            delivery_method: None,
            is_text_subtitle_stream: false,
            supports_external_stream: false,
            replay_gain: None,
            display_title: None,
        });
        // P16 — multi-track audio when the probe carries the new
        // audio_tracks Vec. Falls through to the scalar
        // single-stream path when the Vec is empty (rows from
        // pre-P16 probe passes). Each track surfaces its real
        // language + title so jellyfin-web's player renders an
        // audio picker.
        if !probe.audio_tracks.is_empty() {
            for t in &probe.audio_tracks {
                streams.push(MediaStreamDto {
                    kind: "Audio",
                    index: t.stream_index,
                    codec: t.codec.clone(),
                    is_default: t.is_default,
                    width: None,
                    height: None,
                    channels: t.channels,
                    sample_rate: t.sample_rate,
                    bit_rate: None,
                    aspect_ratio: None,
                    real_frame_rate: None,
                    average_frame_rate: None,
                    language: t.language.clone(),
                    title: t.title.clone(),
                    is_external: false,
                    is_forced: false,
                    is_hearing_impaired: false,
                    is_interlaced: false,
                    is_original: false,
                    delivery_url: None,
                    delivery_method: None,
                    is_text_subtitle_stream: false,
                    supports_external_stream: false,
                    replay_gain: build_replay_gain(t),
                    display_title: None,
                });
            }
        } else if let Some(codec) = probe.audio_codec.clone() {
            // Back-compat scalar path — older rows have audio_codec
            // populated but audio_tracks empty. Some test fixtures
            // (BBB WebM corpus) are video-only; only emit when probe
            // actually saw audio.
            streams.push(MediaStreamDto {
                kind: "Audio",
                index: 1,
                codec: Some(codec),
                is_default: true,
                width: None,
                height: None,
                channels: probe.audio_channels,
                sample_rate: probe.sample_rate,
                bit_rate: None,
                aspect_ratio: None,
                real_frame_rate: None,
                average_frame_rate: None,
                language: None,
                title: None,
                is_external: false,
                is_forced: false,
                is_hearing_impaired: false,
                is_interlaced: false,
                is_original: false,
                delivery_url: None,
                delivery_method: None,
                is_text_subtitle_stream: false,
                supports_external_stream: false,
                replay_gain: None,
                display_title: None,
            });
        }
        // Subtitle tracks — embedded first, then sidecars.
        if let Some(ctx) = subtitle_ctx {
            for t in &probe.subtitle_tracks {
                // P30 — append `(forced)` so jellyfin-web's track
                // picker shows the disposition without the user
                // having to memorise indices.
                let base = t.title.clone().or_else(|| t.language.clone());
                // P30 forced suffix + P35 SDH suffix. Real Jellyfin's
                // picker shows both — order matches what jellyfin-web
                // renders so the strings sort-stable across reloads.
                let title = {
                    let mut s = base.clone().unwrap_or_default();
                    if t.is_hearing_impaired {
                        if s.is_empty() {
                            s = "SDH".to_string();
                        } else {
                            s.push_str(" [SDH]");
                        }
                    }
                    if t.is_forced {
                        if s.is_empty() {
                            s = "Forced".to_string();
                        } else {
                            s.push_str(" (forced)");
                        }
                    }
                    if s.is_empty() {
                        base
                    } else {
                        Some(s)
                    }
                };
                // Text subs deliver External (client-side render); image subs
                // (PGS/VOBSUB) can't be text, so they burn into the transcode
                // (Encode). ASS/SSA render via jellyfin-web's SubtitlesOctopus,
                // which routes purely on Codec=="ass"/"ssa" and fetches the
                // DeliveryUrl byte-for-byte — so it must be the RAW .ass, NOT
                // a converted .vtt (libass can't parse a VTT body → "failed to
                // start a track"). subrip/other text stay .vtt for a native
                // <track>.
                let is_text = is_text_subtitle_codec(t.codec.as_deref());
                let is_ass = is_ass_subtitle_codec(t.codec.as_deref());
                let ext = if is_ass { "ass" } else { "vtt" };
                streams.push(MediaStreamDto {
                    kind: "Subtitle",
                    index: t.stream_index,
                    codec: t.codec.clone(),
                    is_default: t.is_default,
                    width: None,
                    height: None,
                    channels: None,
                    sample_rate: None,
                    bit_rate: None,
                    aspect_ratio: None,
                    real_frame_rate: None,
                    average_frame_rate: None,
                    language: t.language.clone(),
                    title,
                    is_external: false,
                    is_forced: t.is_forced,
                    is_hearing_impaired: t.is_hearing_impaired,
                    is_interlaced: false,
                    is_original: false,
                    delivery_url: is_text.then(|| {
                        format!(
                            "/Videos/{id}/{id}/Subtitles/{idx}/Stream.{ext}",
                            id = wire_item_id(ctx.item_id),
                            idx = t.stream_index,
                        )
                    }),
                    delivery_method: Some(if is_text { "External" } else { "Encode" }),
                    is_text_subtitle_stream: is_text,
                    supports_external_stream: is_text,
                    replay_gain: None,
                    display_title: None,
                });
            }
            // Sidecars: contiguous, one past the highest real ffprobe index
            // (B71 — never the old sparse 1_000_000 sentinel).
            let sidecar_base = sidecar_base_index(probe);
            for offset in 0..ctx.sidecar_count {
                let idx = sidecar_base + offset;
                let lang = ctx
                    .sidecar_langs
                    .get(offset as usize)
                    .and_then(|l| l.clone());
                // Label by language when the filename carried one; else the
                // positional "External N" fallback.
                let title = match lang.as_deref() {
                    Some(l) => Some(language_display_name(l)),
                    None => Some(format!("External {}", offset + 1)),
                };
                streams.push(MediaStreamDto {
                    kind: "Subtitle",
                    index: idx,
                    codec: Some("webvtt".into()),
                    is_default: false,
                    width: None,
                    height: None,
                    channels: None,
                    sample_rate: None,
                    bit_rate: None,
                    aspect_ratio: None,
                    real_frame_rate: None,
                    average_frame_rate: None,
                    language: lang.clone(),
                    title,
                    is_external: true,
                    is_forced: false,
                    is_hearing_impaired: false,
                    is_interlaced: false,
                    is_original: false,
                    delivery_url: Some(format!(
                        "/Videos/{id}/{id}/Subtitles/{idx}/Stream.vtt",
                        id = wire_item_id(ctx.item_id),
                    )),
                    // Sidecars are already WebVTT — always External text.
                    delivery_method: Some("External"),
                    is_text_subtitle_stream: true,
                    supports_external_stream: true,
                    replay_gain: None,
                    display_title: None,
                });
            }
        }
    } else {
        streams.push(MediaStreamDto {
            kind: "Audio",
            index: 0,
            codec: probe.audio_codec.clone(),
            is_default: true,
            width: None,
            height: None,
            channels: probe.audio_channels,
            sample_rate: probe.sample_rate,
            bit_rate: probe.bitrate_bps,
            aspect_ratio: None,
            real_frame_rate: None,
            average_frame_rate: None,
            language: None,
            title: None,
            is_external: false,
            is_forced: false,
            is_hearing_impaired: false,
            is_interlaced: false,
            is_original: false,
            delivery_url: None,
            delivery_method: None,
            is_text_subtitle_stream: false,
            supports_external_stream: false,
            replay_gain: None,
            display_title: None,
        });
    }
    fill_display_titles(&mut streams);
    streams
}

/// Build a MediaSource's `MediaAttachments` array — one entry per embedded
/// attachment (fonts). Each carries a `DeliveryUrl` jellyfin-web fetches and
/// hands to SubtitlesOctopus so ASS/SSA subtitles render with the right fonts.
/// Jellyfin `ChapterInfo` embedded in a `BaseItemDto`. `ImageDateModified` is a
/// non-nullable DateTime in C# (DateTime.MinValue when no chapter image);
/// strict SDKs require it. Typed per B78/V38.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ChapterInfoDto {
    pub name: String,
    pub start_position_ticks: u64,
    pub image_date_modified: &'static str,
}

/// Jellyfin `MediaAttachment` (embedded font / cover in a `MediaSourceInfo`).
/// Typed per B78/V38. jellyfin-web's ASS renderer + native players fetch the
/// `DeliveryUrl` to pull embedded subtitle fonts.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct MediaAttachmentDto {
    pub index: u32,
    pub file_name: Option<String>,
    pub mime_type: Option<String>,
    pub codec: Option<String>,
    pub delivery_url: String,
}

pub fn build_media_attachments(
    item_id: pharos_core::MediaId,
    attachments: &[pharos_core::MediaAttachment],
) -> Vec<MediaAttachmentDto> {
    attachments
        .iter()
        .map(|a| MediaAttachmentDto {
            index: a.stream_index,
            file_name: a.filename.clone(),
            mime_type: a.mime_type.clone(),
            codec: a.codec.clone(),
            delivery_url: format!(
                "/Videos/{id}/{id}/Attachments/{idx}",
                id = wire_item_id(item_id),
                idx = a.stream_index,
            ),
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use pharos_core::MediaProbe;

    #[test]
    fn media_segment_id_is_a_deterministic_uuid() {
        // B69 — the kotlin SDK types MediaSegmentDto.Id as UUID; a non-UUID
        // string crashes the strict client. new() must always yield a valid,
        // stable uuid.
        let a = MediaSegmentDto::new("00000000000000000000000000000abc", "0", 0, 10, "Intro");
        let b = MediaSegmentDto::new("00000000000000000000000000000abc", "0", 0, 10, "Intro");
        assert_eq!(a.id, b.id, "deterministic across calls");
        assert!(
            uuid::Uuid::parse_str(&a.id).is_ok(),
            "Id must be a valid UUID, got {}",
            a.id
        );
        let c = MediaSegmentDto::new("00000000000000000000000000000abc", "d1", 0, 10, "Outro");
        assert_ne!(a.id, c.id, "distinct key → distinct id");
    }

    #[test]
    fn wire_item_ids_are_guid_shaped_and_round_trip() {
        // jellyfin-android parses every id with toUUIDOrNull() and silently
        // drops non-UUID ids — a decimal id string empties the native play
        // queue ("Unable to resolve playback info", B15).
        let id: pharos_core::MediaId = 152_979_617_944_103_156;
        let wire = wire_item_id(id);
        assert_eq!(wire.len(), 32);
        assert!(wire.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(
            uuid::Uuid::parse_str(&wire).is_ok(),
            "must be UUID-parseable"
        );
        assert_eq!(parse_item_id(&wire), Some(id), "canonical round-trip");
        // Dashed UUID form (some SDKs re-serialize dashed).
        let dashed = uuid::Uuid::parse_str(&wire)
            .unwrap()
            .hyphenated()
            .to_string();
        assert_eq!(parse_item_id(&dashed), Some(id));
        // Legacy decimal still accepted (pre-B15 clients / open sessions).
        assert_eq!(parse_item_id("152979617944103156"), Some(id));
        // Synthetic series ids (non-zero high half) are NOT item ids.
        assert_eq!(parse_item_id("11112222333344441111222233334444"), None);
    }

    #[test]
    fn wire_id_separates_the_real_and_synth_namespaces_at_parse() {
        // Real: canonical + dashed + legacy decimal all land in Real.
        let id: pharos_core::MediaId = 152_979_617_944_103_156;
        let wire = wire_item_id(id);
        assert_eq!(WireId::parse(&wire), Some(WireId::Real(id)));
        assert_eq!(
            WireId::parse("152979617944103156"),
            Some(WireId::Real(id)),
            "legacy decimal is always a real id"
        );

        // Synth: every synth emitter uses the duplicated-63-bit-hash shape;
        // parse must classify it as Synth with the hash recovered, for ANY
        // of the synth families.
        for synth in [
            series_id_for("Cosmos"),
            season_id_for("Cosmos", 2),
            artist_id_for("Limp Bizkit"),
            album_id_for("Significant Other"),
        ] {
            let parsed = WireId::parse(&synth);
            let Some(WireId::Synth(h)) = parsed else {
                panic!("synth id {synth} must parse as Synth, got {parsed:?}");
            };
            // Display round-trips byte-identically.
            assert_eq!(WireId::Synth(h).to_string(), synth);
            // And the real-only funnel keeps rejecting it.
            assert_eq!(parse_item_id(&synth), None);
        }

        // Foreign GUID (neither namespace: high half ≠ 0 and ≠ low half) —
        // e.g. a user id — is neither Real nor Synth.
        assert_eq!(WireId::parse("7f0030cf2cf7436787ddbded65123a89"), None);

        // Real ids round-trip through Display too.
        assert_eq!(WireId::Real(id).to_string(), wire);
    }

    #[test]
    fn format_iso8601_ms_has_three_fraction_digits() {
        // Epoch → 1970-01-01T00:00:00.000Z.
        assert_eq!(format_iso8601_ms(0), "1970-01-01T00:00:00.000Z");
        // 1.234s past a known instant keeps the milliseconds.
        // 1_700_000_000_000 ms = 2023-11-14T22:13:20.000Z.
        assert_eq!(
            format_iso8601_ms(1_700_000_000_123),
            "2023-11-14T22:13:20.123Z"
        );
        // Must match `…THH:MM:SS.mmmZ` (exactly 3 fraction digits).
        let s = format_iso8601_ms(1_700_000_000_045);
        let frac = &s[s.len() - 4..];
        assert_eq!(frac, "045Z", "expected 3-digit ms fraction, got {s}");
    }

    #[test]
    fn media_attachments_emit_font_delivery_urls() {
        // ASS/SSA fonts must surface as MediaAttachments with a DeliveryUrl so
        // jellyfin-web hands them to SubtitlesOctopus.
        let atts = vec![
            pharos_core::MediaAttachment {
                stream_index: 7,
                filename: Some("Arial.ttf".into()),
                mime_type: Some("application/x-truetype-font".into()),
                codec: Some("ttf".into()),
            },
            pharos_core::MediaAttachment {
                stream_index: 8,
                filename: Some("Bold.otf".into()),
                mime_type: Some("application/vnd.ms-opentype".into()),
                codec: Some("otf".into()),
            },
        ];
        let out = build_media_attachments(42, &atts);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].index, 7);
        assert_eq!(out[0].file_name.as_deref(), Some("Arial.ttf"));
        assert_eq!(
            out[0].mime_type.as_deref(),
            Some("application/x-truetype-font")
        );
        assert_eq!(out[0].codec.as_deref(), Some("ttf"));
        assert_eq!(out[0].delivery_url, "/Videos/0000000000000000000000000000002a/0000000000000000000000000000002a/Attachments/7");
        assert_eq!(out[1].index, 8);
        assert_eq!(out[1].delivery_url, "/Videos/0000000000000000000000000000002a/0000000000000000000000000000002a/Attachments/8");
    }

    #[test]
    fn stream_display_title_composes_language_title_codec_channels() {
        // Track with a title → "Language - Title" (the Code Geass case that
        // rendered "undefined").
        assert_eq!(
            stream_display_title(Some("jpn"), Some("TrueHD 5.1"), Some("truehd"), Some(6))
                .as_deref(),
            Some("Japanese - TrueHD 5.1")
        );
        assert_eq!(
            stream_display_title(Some("eng"), Some("CBM (5.1 Surround)"), Some("ass"), None)
                .as_deref(),
            Some("English - CBM (5.1 Surround)")
        );
        // No title → "Language - CODEC Layout".
        assert_eq!(
            stream_display_title(Some("eng"), None, Some("aac"), Some(2)).as_deref(),
            Some("English - AAC Stereo")
        );
        // No language → codec/layout only.
        assert_eq!(
            stream_display_title(None, None, Some("flac"), Some(6)).as_deref(),
            Some("FLAC 5.1")
        );
        // Nothing known → None (jellyfin-web omits the entry rather than
        // showing "undefined").
        assert_eq!(stream_display_title(None, None, None, None), None);
    }

    #[test]
    fn build_streams_fills_display_title_for_audio_and_subtitle() {
        let mut probe = MediaProbe {
            video_codec: Some("hevc".into()),
            audio_tracks: vec![pharos_core::AudioTrack {
                stream_index: 1,
                codec: Some("aac".into()),
                language: Some("eng".into()),
                title: None,
                channels: Some(2),
                ..Default::default()
            }],
            ..Default::default()
        };
        probe.subtitle_tracks = vec![pharos_core::SubtitleTrack {
            stream_index: 2,
            codec: Some("ass".into()),
            language: Some("eng".into()),
            title: Some("Signs/Songs".into()),
            ..Default::default()
        }];
        let ctx = SubtitleStreamCtx::new(1);
        let streams = build_media_streams_with_subtitles(&probe, true, Some(&ctx));
        let audio = streams.iter().find(|s| s.kind == "Audio").unwrap();
        assert_eq!(audio.display_title.as_deref(), Some("English - AAC Stereo"));
        let sub = streams.iter().find(|s| s.kind == "Subtitle").unwrap();
        assert_eq!(sub.display_title.as_deref(), Some("English - Signs/Songs"));
        // Video never gets a DisplayTitle.
        let video = streams.iter().find(|s| s.kind == "Video").unwrap();
        assert!(video.display_title.is_none());
    }

    #[test]
    fn subtitle_delivery_method_by_codec() {
        // Text subs (subrip) → External + a .vtt DeliveryUrl the client fetches;
        // image subs (PGS) → Encode (burn into the transcode) with no URL.
        // Without a DeliveryMethod jellyfin-web can't render subs at all.
        let probe = MediaProbe {
            video_codec: Some("h264".into()),
            subtitle_tracks: vec![
                pharos_core::SubtitleTrack {
                    stream_index: 2,
                    codec: Some("subrip".into()),
                    language: Some("eng".into()),
                    ..Default::default()
                },
                pharos_core::SubtitleTrack {
                    stream_index: 3,
                    codec: Some("hdmv_pgs_subtitle".into()),
                    language: Some("eng".into()),
                    ..Default::default()
                },
                pharos_core::SubtitleTrack {
                    stream_index: 4,
                    codec: Some("ass".into()),
                    language: Some("eng".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let ctx = SubtitleStreamCtx::new(42);
        let streams = build_media_streams_with_subtitles(&probe, true, Some(&ctx));
        // subrip → External VTT (native <track>).
        let text = streams.iter().find(|s| s.index == 2).unwrap();
        assert_eq!(text.delivery_method, Some("External"));
        assert_eq!(
            text.delivery_url.as_deref(),
            Some("/Videos/0000000000000000000000000000002a/0000000000000000000000000000002a/Subtitles/2/Stream.vtt")
        );
        // jellyfin-web's subtitle engine gates on these two flags: it only
        // fetches an external track (Stream.js / Stream.vtt) when
        // IsTextSubtitleStream && SupportsExternalStream. Omitting them (the
        // pre-fix wire shape) made the client skip text subs entirely.
        assert!(text.is_text_subtitle_stream);
        assert!(text.supports_external_stream);
        // PGS → Encode (burn), no URL.
        let image = streams.iter().find(|s| s.index == 3).unwrap();
        assert_eq!(image.delivery_method, Some("Encode"));
        assert!(
            image.delivery_url.is_none(),
            "image subs burn in, no .vtt URL"
        );
        // Image subs are neither text nor externally deliverable.
        assert!(!image.is_text_subtitle_stream);
        assert!(!image.supports_external_stream);
        // ASS → External but RAW .ass URL (SubtitlesOctopus, not a VTT track).
        let ass = streams.iter().find(|s| s.index == 4).unwrap();
        assert_eq!(ass.delivery_method, Some("External"));
        assert_eq!(
            ass.delivery_url.as_deref(),
            Some("/Videos/0000000000000000000000000000002a/0000000000000000000000000000002a/Subtitles/4/Stream.ass")
        );
        assert!(ass.is_text_subtitle_stream);
        assert!(ass.supports_external_stream);
    }

    #[test]
    fn sidecar_subtitle_indices_are_contiguous_not_a_sentinel() {
        // B71 — sidecar subtitle wire indices must be REAL, small, contiguous
        // positions (one past the highest real ffprobe index), never the old
        // 1_000_000 sentinel: the kotlin Android/TV players treat MediaStream
        // .Index positionally and crash out-of-bounds on a sparse index.
        let probe = MediaProbe {
            video_codec: Some("h264".into()),
            audio_tracks: vec![pharos_core::AudioTrack {
                stream_index: 1,
                ..Default::default()
            }],
            // No embedded subs → all subtitles are sidecars.
            subtitle_tracks: vec![],
            ..Default::default()
        };
        assert_eq!(sidecar_base_index(&probe), 2, "one past audio index 1");
        let ctx = SubtitleStreamCtx {
            item_id: 0x2a,
            sidecar_count: 3,
            sidecar_langs: vec![Some("eng".into()), Some("spa".into()), None],
        };
        let streams = build_media_streams_with_subtitles(&probe, true, Some(&ctx));
        let subs: Vec<u32> = streams
            .iter()
            .filter(|s| s.kind == "Subtitle")
            .map(|s| s.index)
            .collect();
        assert_eq!(subs, vec![2, 3, 4], "contiguous after the real streams");
        assert!(
            streams.iter().all(|s| s.index < 1000),
            "no stream carries the old 1_000_000 sentinel index"
        );
    }

    #[test]
    fn metadata_fields_project_into_dto_when_set() {
        // LIB-C7/C8/C9 — populated descriptive metadata must surface on
        // the wire under the Jellyfin field names.
        use pharos_core::{MediaItem, MediaKind, MediaMetadata, ProviderIds};
        let item = MediaItem {
            id: 1,
            path: "/m/a.mkv".into(),
            title: "A".into(),
            kind: MediaKind::Movie,
            metadata: MediaMetadata {
                community_rating: Some(8.7),
                critic_rating: Some(83.0),
                official_rating: Some("R".into()),
                production_year: Some(1999),
                premiere_date: Some(922_060_800),
                overview: Some("synopsis".into()),
                tagline: Some("a tagline".into()),
                provider_ids: ProviderIds {
                    tmdb: Some("603".into()),
                    imdb: Some("tt0133093".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let dto = BaseItemDto::from_domain(&item, "srv");
        let v = serde_json::to_value(&dto).unwrap();
        assert_eq!(v["Overview"], "synopsis");
        assert_eq!(v["Taglines"], serde_json::json!(["a tagline"]));
        // f32 → JSON widens to f64; compare with tolerance.
        assert!((v["CommunityRating"].as_f64().unwrap() - 8.7).abs() < 1e-4);
        assert!((v["CriticRating"].as_f64().unwrap() - 83.0).abs() < 1e-4);
        assert_eq!(v["OfficialRating"], "R");
        assert_eq!(v["ProductionYear"], 1999);
        assert!(v["PremiereDate"].as_str().unwrap().starts_with("1999-03-"));
        assert_eq!(v["ProviderIds"]["Tmdb"], "603");
        assert_eq!(v["ProviderIds"]["Imdb"], "tt0133093");
        assert!(v["ProviderIds"].get("Tvdb").is_none());
        // T67 — ExternalUrls derived from the provider ids. A Movie's TMDb link
        // is type-scoped to `/movie`; the IMDb link points at `/title`.
        let urls: Vec<(String, String)> = v["ExternalUrls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e["Name"].as_str().unwrap().to_string(),
                    e["Url"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert!(urls.contains(&(
            "IMDb".into(),
            "https://www.imdb.com/title/tt0133093/".into()
        )));
        assert!(urls.contains(&(
            "TheMovieDb".into(),
            "https://www.themoviedb.org/movie/603".into()
        )));
    }

    #[test]
    fn metadata_fields_absent_when_none() {
        // The un-enriched path: every C7/C8 field is omitted from the
        // wire (preserving the existing /Items golden shape). ProviderIds
        // stays an empty object.
        use pharos_core::{MediaItem, MediaKind};
        let item = MediaItem {
            id: 2,
            path: "/m/b.mkv".into(),
            title: "B".into(),
            kind: MediaKind::Movie,
            ..Default::default()
        };
        let dto = BaseItemDto::from_domain(&item, "srv");
        let v = serde_json::to_value(&dto).unwrap();
        for key in [
            "Overview",
            "Taglines",
            "CommunityRating",
            "CriticRating",
            "OfficialRating",
            "ProductionYear",
            "PremiereDate",
        ] {
            assert!(v.get(key).is_none(), "{key} must be absent when None");
        }
        assert_eq!(v["ProviderIds"], serde_json::json!({}));
    }

    #[test]
    fn same_name_shows_in_distinct_folders_get_distinct_wire_ids() {
        // LIB-C11 — two episodes of same-name shows in different folders
        // must project to DIFFERENT SeriesId/SeasonId so jellyfin-web
        // doesn't interleave them, and each carries its own year.
        use pharos_core::{MediaItem, MediaKind, SeriesInfo};
        let mk = |folder: &str, year: u32| MediaItem {
            id: 1,
            path: format!("{folder}/Season 01/S01E01.mkv").into(),
            title: "Cosmos".into(),
            kind: MediaKind::Episode,
            series: Some(SeriesInfo {
                series_name: "Cosmos".into(),
                season_number: Some(1),
                episode_number: Some(1),
                series_folder: Some(folder.into()),
                series_year: Some(year),
            }),
            ..Default::default()
        };
        let a = BaseItemDto::from_domain(&mk("/tv/Cosmos (1980)", 1980), "srv");
        let b = BaseItemDto::from_domain(&mk("/tv/Cosmos (2014)", 2014), "srv");

        // Same display name…
        assert_eq!(a.series_name, b.series_name);
        // …distinct folder-keyed Series/Season ids.
        assert!(a.series_id.is_some());
        assert_ne!(a.series_id, b.series_id);
        assert_ne!(a.season_id, b.season_id);
        assert_ne!(a.parent_id, b.parent_id); // ParentId = SeasonId here.

        // The folder-keyed id equals the standalone helper output…
        assert_eq!(
            a.series_id.as_deref(),
            Some(series_id_for_key(Some("/tv/Cosmos (1980)"), "Cosmos").as_str())
        );
        // …and differs from the legacy name-only id (proving the fix).
        assert_ne!(
            a.series_id.as_deref(),
            Some(series_id_for("Cosmos").as_str())
        );

        // Year flows into ProductionYear so clients can disambiguate.
        let av = serde_json::to_value(&a).unwrap();
        let bv = serde_json::to_value(&b).unwrap();
        assert_eq!(av["ProductionYear"], 1980);
        assert_eq!(bv["ProductionYear"], 2014);
    }

    #[test]
    fn legacy_series_id_helpers_unchanged_with_no_folder() {
        // LIB-C11 — passing None must be byte-identical to the legacy
        // name-only helpers so pre-backfill client URLs keep resolving.
        assert_eq!(series_id_for_key(None, "Cosmos"), series_id_for("Cosmos"));
        assert_eq!(
            season_id_for_key(None, "Cosmos", 1),
            season_id_for("Cosmos", 1)
        );
    }

    #[test]
    fn container_for_prefers_webm_alias_over_matroska() {
        // ffprobe reports `format_name = "matroska,webm"` for both .mkv
        // and .webm files; jellyfin-web's DirectPlayProfile expects
        // `webm` for vp9 video. Picking "matroska" forces transcode →
        // "Playback Error" because no TranscodingUrl is wired.
        let probe = MediaProbe {
            container: Some("matroska,webm".into()),
            ..Default::default()
        };
        assert_eq!(container_for(&probe, true), "webm");
    }

    #[test]
    fn container_for_falls_back_when_no_preferred_alias() {
        let probe = MediaProbe {
            container: Some("avi".into()),
            ..Default::default()
        };
        assert_eq!(container_for(&probe, true), "avi");
    }

    #[test]
    fn container_for_kind_default_when_probe_empty() {
        let probe = MediaProbe::default();
        assert_eq!(container_for(&probe, true), "mp4");
        assert_eq!(container_for(&probe, false), "mp3");
    }

    #[test]
    fn build_media_streams_omits_audio_for_silent_video() {
        // BBB test corpus is video-only. Advertising a fabricated audio
        // stream there breaks playback — the client tries to decode
        // bytes that aren't AAC.
        let probe = MediaProbe {
            video_codec: Some("vp9".into()),
            width: Some(1920),
            height: Some(1080),
            ..Default::default()
        };
        let streams = build_media_streams(&probe, true);
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].kind, "Video");
        assert_eq!(streams[0].codec.as_deref(), Some("vp9"));
    }

    #[test]
    fn build_media_streams_emits_audio_when_probe_has_codec() {
        let probe = MediaProbe {
            video_codec: Some("vp9".into()),
            audio_codec: Some("opus".into()),
            audio_channels: Some(2),
            sample_rate: Some(48_000),
            ..Default::default()
        };
        let streams = build_media_streams(&probe, true);
        assert_eq!(streams.len(), 2);
        assert_eq!(streams[1].kind, "Audio");
        assert_eq!(streams[1].codec.as_deref(), Some("opus"));
        assert_eq!(streams[1].channels, Some(2));
    }
}

// ---------------------------------------------------------------------
// Trickplay DTO helpers — lifted from the server-side `trickplay`
// handler in Phase A.2 so `BaseItemDto::with_trickplay` is self-
// contained on this side of the crate boundary. The handler now
// imports these from `pharos_jellyfin_api::dto`.

use pharos_cache::trickplay_cache::{Layout, TILE_GRID};

/// Compose a `Layout` for the requested width when probe has the
/// data we need. Returns `None` when duration_ms / dimensions are
/// missing — callers (HTTP routes) 404 rather than 500.
pub fn build_layout(
    probe: &pharos_core::MediaProbe,
    width: u32,
    interval_ms: u32,
) -> Option<Layout> {
    let duration_ms = probe.duration_ms?;
    let src_w = probe.width?;
    let src_h = probe.height?;
    Layout::compute(duration_ms, src_w, src_h, width, interval_ms)
}

/// Render `BaseItemDto.Trickplay` for a video item. Returns the
/// `{ width_str → TileInfo }` map; empty when no width yields a
/// valid layout (no probe data, audio-only item, or widths
/// unconfigured).
///
/// Wire shape per width:
/// ```json
/// "320": {
///   "Width": 320, "Height": 180,
///   "TileWidth": 10, "TileHeight": 10,
///   "ThumbnailCount": 89, "Interval": 10000, "Bandwidth": 0
/// }
/// ```
pub fn build_dto_layout_map(
    probe: &pharos_core::MediaProbe,
    widths: &[u32],
    interval_ms: u32,
) -> serde_json::Map<String, serde_json::Value> {
    let mut out = serde_json::Map::new();
    for &w in widths {
        if let Some(layout) = build_layout(probe, w, interval_ms) {
            out.insert(
                w.to_string(),
                serde_json::json!({
                    "Width": layout.width,
                    "Height": layout.height,
                    "TileWidth": TILE_GRID,
                    "TileHeight": TILE_GRID,
                    "ThumbnailCount": layout.thumb_count,
                    "Interval": layout.interval_ms,
                    "Bandwidth": 0u64,
                }),
            );
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod trickplay_helper_tests {
    use super::{build_dto_layout_map, build_layout};
    use pharos_core::MediaProbe;

    fn probe_1080p_10min() -> MediaProbe {
        MediaProbe {
            duration_ms: Some(10 * 60 * 1000),
            width: Some(1920),
            height: Some(1080),
            ..Default::default()
        }
    }

    #[test]
    fn dto_layout_map_emits_one_entry_per_configured_width() {
        let probe = probe_1080p_10min();
        let map = build_dto_layout_map(&probe, &[320, 640], 10_000);
        assert_eq!(map.len(), 2);
        let v320 = map.get("320").unwrap();
        assert_eq!(v320.get("Width").unwrap().as_u64().unwrap(), 320);
        assert_eq!(v320.get("Height").unwrap().as_u64().unwrap(), 180);
        assert_eq!(v320.get("Interval").unwrap().as_u64().unwrap(), 10_000);
        assert_eq!(v320.get("TileWidth").unwrap().as_u64().unwrap(), 10);
        // 10 min @ 10s = 60 thumbs.
        assert_eq!(v320.get("ThumbnailCount").unwrap().as_u64().unwrap(), 60);
    }

    #[test]
    fn dto_layout_map_empty_when_probe_lacks_dimensions() {
        let probe = MediaProbe {
            duration_ms: Some(60_000),
            width: None,
            height: None,
            ..Default::default()
        };
        let map = build_dto_layout_map(&probe, &[320], 10_000);
        assert!(map.is_empty());
    }

    #[test]
    fn dto_layout_map_empty_when_no_widths_configured() {
        let probe = probe_1080p_10min();
        let map = build_dto_layout_map(&probe, &[], 10_000);
        assert!(map.is_empty());
    }

    #[test]
    fn with_trickplay_nests_layout_under_media_source_id() {
        // Jellyfin's BaseItemDto.Trickplay is a *double-nested* map:
        // `{ "<mediaSourceId>": { "<width>": TileInfo } }`. jellyfin-web
        // looks up `item.Trickplay[mediaSourceId]` and never requests tiles
        // if that key is absent. For pharos single-file items the media
        // source id equals the item id string.
        use pharos_core::{MediaItem, MediaKind, MediaProbe};
        let item = MediaItem {
            id: 42,
            path: "/m/a.mkv".into(),
            title: "A".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(10 * 60 * 1000),
                width: Some(1920),
                height: Some(1080),
                ..Default::default()
            },
            ..Default::default()
        };
        let dto = super::BaseItemDto::from_domain(&item, "srv").with_trickplay(
            &item.probe,
            &[320, 640],
            10_000,
        );
        let v = serde_json::to_value(&dto).unwrap();
        // Outer key is the media-source id (the item id in canonical 32-hex
        // GUID wire form — B15); inner keyed by width.
        assert_eq!(
            v["Trickplay"]["0000000000000000000000000000002a"]["320"]["Width"]
                .as_u64()
                .unwrap(),
            320
        );
        assert_eq!(
            v["Trickplay"]["0000000000000000000000000000002a"]["640"]["Width"]
                .as_u64()
                .unwrap(),
            640
        );
        // The old (wrong) flat shape put the width at the top level.
        assert!(
            v["Trickplay"].get("320").is_none(),
            "width must not sit at the top level: {}",
            v["Trickplay"]
        );
    }

    #[test]
    fn build_layout_returns_none_when_duration_missing() {
        let probe = MediaProbe {
            duration_ms: None,
            width: Some(1920),
            height: Some(1080),
            ..Default::default()
        };
        assert!(build_layout(&probe, 320, 10_000).is_none());
    }

    #[test]
    fn local_artwork_tags_add_upload_only_roles_additively() {
        use pharos_core::{MediaItem, MediaKind};
        let item = MediaItem {
            id: 5,
            path: "/m/a.mkv".into(),
            title: "A".into(),
            kind: MediaKind::Movie,
            ..Default::default()
        };
        let dto = super::BaseItemDto::from_domain(&item, "srv")
            .with_local_artwork_tags(5, &["Logo".into(), "Backdrop".into()]);
        // Existing frame-extract Primary tag stays.
        assert!(dto.image_tags.contains_key("Primary"));
        // Upload-only Logo now advertised.
        assert!(dto.image_tags.contains_key("Logo"));
        // Backdrop list role appended once.
        assert_eq!(dto.backdrop_image_tags.len(), 1);
        assert_eq!(
            dto.backdrop_image_tags[0],
            super::image_tag_for(5, "backdrop")
        );
    }

    #[test]
    fn audio_item_advertises_no_backdrop() {
        use pharos_core::{MediaItem, MediaKind};
        // The image handler 404s Backdrop/Thumb for Audio (no video frames);
        // the DTO must not advertise one, or jellyfin-web requests a backdrop
        // that only 404s (tripped the strict-console compat spec on a music
        // item). Both the ImageTags map AND the BackdropImageTags list must
        // omit it.
        let audio = MediaItem {
            id: 4,
            path: "/m/a.webm".into(),
            title: "A".into(),
            kind: MediaKind::Audio,
            ..Default::default()
        };
        let dto = super::BaseItemDto::from_domain(&audio, "srv");
        assert!(
            dto.backdrop_image_tags.is_empty(),
            "audio: no backdrop list"
        );
        assert!(
            !dto.image_tags.contains_key("Backdrop"),
            "audio: no Backdrop tag"
        );
        assert!(!dto.image_tags.contains_key("Thumb"), "audio: no Thumb tag");
        assert!(
            dto.image_tags.contains_key("Primary"),
            "audio keeps Primary (cover art)"
        );

        // A video item still advertises a backdrop.
        let movie = MediaItem {
            id: 1,
            kind: MediaKind::Movie,
            ..audio.clone()
        };
        assert_eq!(
            super::BaseItemDto::from_domain(&movie, "srv").backdrop_image_tags,
            vec![super::image_tag_for(1, "backdrop")]
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod person_image_tag_tests {
    use super::BaseItemDto;
    use pharos_core::{ItemPerson, MediaItem, MediaKind, PersonKind};

    fn person(name: &str, thumb: Option<&str>) -> ItemPerson {
        ItemPerson {
            name: name.to_string(),
            wire_id: pharos_core::person_wire_id(name),
            role: None,
            character: Some("Self".to_string()),
            kind: PersonKind::Actor,
            sort_order: Some(0),
            thumb_url: thumb.map(str::to_string),
        }
    }

    fn movie() -> MediaItem {
        MediaItem {
            id: 1,
            kind: MediaKind::Movie,
            ..Default::default()
        }
    }

    #[test]
    fn primary_image_tag_only_for_http_thumb() {
        let people = [
            person("Has Photo", Some("https://image.tmdb.org/t/p/w300/x.jpg")),
            person("Legacy Path", Some("/config/metadata/People/l.jpg")),
            person("No Thumb", None),
        ];
        let dto = BaseItemDto::from_domain(&movie(), "srv").with_people(&people);

        assert!(
            dto.people[0].primary_image_tag.is_some(),
            "http(s) portrait advertises a tag"
        );
        assert_eq!(
            dto.people[1].primary_image_tag, None,
            "legacy local path advertises no tag (would 404)"
        );
        assert_eq!(
            dto.people[2].primary_image_tag, None,
            "portrait-less cast advertises no tag"
        );
    }

    #[test]
    fn tag_is_stable_per_url_and_differs_across_urls() {
        let a1 = super::person_image_tag_for("https://cdn/a.jpg");
        let a2 = super::person_image_tag_for("https://cdn/a.jpg");
        let b = super::person_image_tag_for("https://cdn/b.jpg");
        assert_eq!(a1, a2, "same url → same tag (stable across renders)");
        assert_ne!(a1, b, "different url → different tag (cache-bust)");
    }
}
