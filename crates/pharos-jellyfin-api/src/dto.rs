//! Jellyfin response/request DTOs. PascalCase on the wire to match the
//! reference Jellyfin API (V7).

use pharos_core::{User, UserPolicy};
use serde::{Deserialize, Serialize};

/// Sidecar subtitle streams numbered starting here so their indices
/// never collide with real ffprobe stream indices (which start at 0
/// and are typically single-digit). Lifted here in Phase A.2 so the
/// DTO crate is self-contained.
pub const SIDECAR_BASE_INDEX: u32 = 1_000_000;

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
            policy: UserPolicyDto::from_domain(&user.policy),
            configuration: UserConfigurationDto::default(),
            primary_image_aspect_ratio: 1.0,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
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
}

impl UserPolicyDto {
    pub fn from_domain(p: &UserPolicy) -> Self {
        Self {
            is_administrator: p.admin,
            is_hidden: false,
            is_disabled: false,
            enable_remote_access: true,
            enable_media_playback: true,
            enable_audio_playback_transcoding: true,
            enable_video_playback_transcoding: true,
            enable_playback_remuxing: true,
            enable_content_deletion: p.admin,
            enable_content_downloading: true,
            enable_sync_transcoding: true,
            enable_media_conversion: true,
            enable_all_devices: true,
            enable_all_channels: true,
            enable_all_folders: true,
            enable_public_sharing: false,
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
    pub chapters: Vec<serde_json::Value>,
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

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct NameGuidPairDto {
    pub name: String,
    pub id: String,
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

#[derive(Debug, Serialize)]
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
    /// Subtitle-only: jellyfin-web uses `IsExternal` to flag sidecar
    /// vs embedded tracks in the picker UI.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_external: Option<bool>,
    /// Subtitle-only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_forced: Option<bool>,
    /// P35 — Subtitle-only. `true` when the track's
    /// `disposition.hearing_impaired` flag is set (SDH / CC).
    /// jellyfin-web reads this to label the picker entry and an
    /// accessibility filter on `/Items` reuses the field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_hearing_impaired: Option<bool>,
    /// Jellyfin's URL the player fetches the rendered .vtt from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery_url: Option<String>,
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
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ReplayGainDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_gain: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_gain: Option<f32>,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct UserItemDataDto {
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
        Self {
            played: data.played,
            play_count: data.play_count,
            playback_position_ticks: data.last_played_position_ticks,
            played_percentage: 0.0,
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
}

/// Minimal ISO-8601 (Z) formatter for the `LastPlayedDate` field —
/// avoids pulling in `chrono` just for one render path. T58 phase 3
/// reuses it from the admin module for `/Auth/Keys` DateCreated.
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

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ItemsResultDto {
    pub items: Vec<BaseItemDto>,
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
        self.trickplay = build_dto_layout_map(probe, widths, interval_ms);
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
                PersonDto {
                    name: p.name.clone(),
                    id: p.wire_id.clone(),
                    role,
                    kind: p.kind.as_str(),
                    primary_image_tag: None,
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
            id: item.id.to_string(),
            name: item.title.clone(),
            server_id: server_id.to_string(),
            kind,
            media_type,
            is_folder: false,
            user_data: UserItemDataDto::default(),
            run_time_ticks,
            location_type: "FileSystem",
            can_play: true,
            play_access: "Full",
            media_sources: vec![MediaSourceLiteDto {
                id: item.id.to_string(),
                container,
                kind: "Default",
                is_remote: false,
                supports_direct_play: true,
                supports_direct_stream: true,
                supports_transcoding: true,
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
            production_locations: vec![],
            provider_ids: provider_ids_map(&item.metadata.provider_ids),
            remote_trailers: vec![],
            chapters: item
                .probe
                .chapters
                .iter()
                .map(|c| {
                    // `StartPositionTicks` is Jellyfin's 100-ns unit
                    // (10_000 ticks / ms).
                    let ticks = c.start_ms.saturating_mul(10_000);
                    serde_json::json!({
                        "Name": c.title,
                        "StartPositionTicks": ticks,
                    })
                })
                .collect(),
            trickplay: serde_json::Map::new(),
            external_urls: vec![],
            image_tags: image_tags_for(item),
            backdrop_image_tags: vec![image_tag_for(item.id, "backdrop")],
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
            parent_index_number: item.series.as_ref().and_then(|s| s.season_number),
            index_number: item.series.as_ref().and_then(|s| s.episode_number),
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
                .or_else(|| item.series.as_ref().and_then(|s| s.series_year)),
            premiere_date: item.metadata.premiere_date.map(format_iso8601),
        }
    }
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
}

impl SubtitleStreamCtx {
    pub fn new(item_id: pharos_core::MediaId) -> Self {
        Self {
            item_id,
            sidecar_count: 0,
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
            is_external: None,
            is_forced: None,
            is_hearing_impaired: None,
            delivery_url: None,
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
                    is_external: None,
                    is_forced: None,
                    is_hearing_impaired: None,
                    delivery_url: None,
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
                is_external: None,
                is_forced: None,
                is_hearing_impaired: None,
                delivery_url: None,
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
                    is_external: Some(false),
                    is_forced: Some(t.is_forced),
                    is_hearing_impaired: Some(t.is_hearing_impaired),
                    delivery_url: Some(format!(
                        "/Videos/{id}/{id}/Subtitles/{idx}/Stream.vtt",
                        id = ctx.item_id,
                        idx = t.stream_index,
                    )),
                    replay_gain: None,
                    display_title: None,
                });
            }
            // Sidecars: stream_index = SIDECAR_BASE + offset.
            for offset in 0..ctx.sidecar_count {
                let idx = SIDECAR_BASE_INDEX + offset;
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
                    language: None,
                    title: Some(format!("External {}", offset + 1)),
                    is_external: Some(true),
                    is_forced: Some(false),
                    is_hearing_impaired: Some(false),
                    delivery_url: Some(format!(
                        "/Videos/{id}/{id}/Subtitles/{idx}/Stream.vtt",
                        id = ctx.item_id,
                    )),
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
            is_external: None,
            is_forced: None,
            is_hearing_impaired: None,
            delivery_url: None,
            replay_gain: None,
            display_title: None,
        });
    }
    fill_display_titles(&mut streams);
    streams
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use pharos_core::MediaProbe;

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
}
