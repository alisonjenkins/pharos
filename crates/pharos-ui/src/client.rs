//! Server client. Two layers:
//!
//! - **Parse helpers** (always compiled, host-testable). Map Jellyfin-shaped
//!   JSON bytes into `LoggedInUser` / `Vec<LibraryItem>`.
//! - **Transport** (gated by `web` feature, WASM-only). Wraps the parse
//!   helpers around `gloo_net::http::Request`.
//!
//! V16: only the public Jellyfin-compat surface is consumed. No backdoor.

use crate::api_types::{ItemKind, LibraryItem, LoggedInUser};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("http: {0}")]
    Http(String),
    #[error("parse: {0}")]
    Parse(String),
    #[error("status {0}")]
    Status(u16),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AuthResponseDto {
    user: AuthUserDto,
    server_id: String,
    access_token: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AuthUserDto {
    id: String,
    name: String,
    policy: AuthPolicyDto,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AuthPolicyDto {
    is_administrator: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ItemsResponseDto {
    items: Vec<ItemDto>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ItemDto {
    id: String,
    name: String,
    #[serde(rename = "Type")]
    kind: String,
}

pub fn parse_auth_response(bytes: &[u8]) -> Result<LoggedInUser, ClientError> {
    let parsed: AuthResponseDto =
        serde_json::from_slice(bytes).map_err(|e| ClientError::Parse(e.to_string()))?;
    Ok(LoggedInUser {
        id: parsed.user.id,
        name: parsed.user.name,
        server_id: parsed.server_id,
        access_token: parsed.access_token,
        is_admin: parsed.user.policy.is_administrator,
    })
}

pub fn parse_items_response(bytes: &[u8]) -> Result<Vec<LibraryItem>, ClientError> {
    let parsed: ItemsResponseDto =
        serde_json::from_slice(bytes).map_err(|e| ClientError::Parse(e.to_string()))?;
    Ok(parsed
        .items
        .into_iter()
        .map(|i| LibraryItem {
            id: i.id,
            name: i.name,
            kind: ItemKind::from_jellyfin_type(&i.kind),
        })
        .collect())
}

/// T50 — admin user-list parser. The Jellyfin `/Users` endpoint
/// returns a bare array of `UserDto` (NOT wrapped in `ItemsResult`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminUser {
    pub id: String,
    pub name: String,
    pub is_admin: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AdminUserDto {
    id: String,
    name: String,
    policy: AdminUserPolicyDto,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AdminUserPolicyDto {
    is_administrator: bool,
}

/// T53 — `/Search/Hints` result shape. `MediaType` distinguishes
/// `Audio` from `Video`; `Type` (the kind) tracks the
/// Jellyfin-side discriminator (Movie / Episode / Audio / ...).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHint {
    pub id: String,
    pub name: String,
    pub kind: ItemKind,
    pub matched_term: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SearchHintDto {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(rename = "Type", default)]
    kind: String,
    #[serde(default)]
    matched_term: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SearchHintsResultDto {
    #[serde(default)]
    search_hints: Vec<SearchHintDto>,
}

/// T54 — single-item detail (`GET /Items/{id}` shape, projection).
///
/// Phase 2 adds episode + audio hierarchy + image-presence so the
/// detail view can render S/E breadcrumbs, artist/album lines, and
/// the Primary backdrop without re-fetching. Phase 3 picks up
/// `Overview`, `People`, and chapter scaffolding.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ItemDetail {
    pub id: String,
    pub name: String,
    pub kind: ItemKind,
    pub run_time_ticks: u64,
    pub played: bool,
    pub play_count: u32,
    pub is_favorite: bool,
    pub playback_position_ticks: u64,
    /// Episode-only: parent series display name.
    pub series_name: Option<String>,
    /// Episode-only: season number (jellyfin's ParentIndexNumber).
    pub season_index: Option<u32>,
    /// Episode-only: episode number within season (IndexNumber).
    pub episode_index: Option<u32>,
    /// Audio-only: track artists, in display order.
    pub artists: Vec<String>,
    /// Audio-only: album name.
    pub album: Option<String>,
    /// Audio-only: album-level artists (often == artists, sometimes "Various Artists").
    pub album_artists: Vec<String>,
    /// True when ImageTags contains a "Primary" entry — caller composes
    /// the `/Items/{id}/Images/Primary` URL.
    pub has_primary_image: bool,
    /// True when BackdropImageTags is non-empty.
    pub has_backdrop_image: bool,
    /// T54 phase 3: cast + crew. Server emits `People` as an array of
    /// PersonDto; the UI flattens to a stable display projection.
    pub people: Vec<ItemPerson>,
    /// T54 phase 3: long-form description.
    pub overview: Option<String>,
    /// T54 phase 3: genre tags. Server emits as a flat string array.
    pub genres: Vec<String>,
    /// T54 phase 4 / T57 phase 2: chapter markers parsed from
    /// `BaseItemDto.Chapters[].{Name,StartPositionTicks}`.
    pub chapters: Vec<ItemChapter>,
}

/// T54 phase 3 — cast / crew display projection.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ItemPerson {
    pub id: String,
    pub name: String,
    /// e.g. "Director", "Actor", "Writer".
    pub kind: String,
    /// Role label (Actor's character name); empty for crew.
    pub role: String,
    /// True when ImageTag is non-null — caller composes
    /// `/Items/{person_id}/Images/Primary`.
    pub has_image: bool,
}

/// T54 phase 4 / T57 phase 2 — chapter marker projection.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ItemChapter {
    pub name: String,
    /// Jellyfin's 100-ns ticks (10_000 ticks / ms).
    pub start_position_ticks: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ItemDetailDto {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(rename = "Type", default)]
    kind: String,
    #[serde(default)]
    run_time_ticks: u64,
    #[serde(default)]
    user_data: ItemDetailUserDataDto,
    #[serde(default)]
    series_name: Option<String>,
    #[serde(default)]
    parent_index_number: Option<u32>,
    #[serde(default)]
    index_number: Option<u32>,
    #[serde(default)]
    artists: Vec<String>,
    #[serde(default)]
    album: Option<String>,
    #[serde(default)]
    album_artists: Vec<NameGuidPairDto>,
    #[serde(default)]
    image_tags: serde_json::Value,
    #[serde(default)]
    backdrop_image_tags: Vec<serde_json::Value>,
    #[serde(default)]
    people: Vec<ItemPersonDto>,
    #[serde(default)]
    overview: Option<String>,
    #[serde(default)]
    genres: Vec<String>,
    #[serde(default)]
    chapters: Vec<ItemChapterDto>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct ItemChapterDto {
    name: String,
    start_position_ticks: u64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct ItemPersonDto {
    id: String,
    name: String,
    role: String,
    #[serde(rename = "Type")]
    kind: String,
    primary_image_tag: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct ItemDetailUserDataDto {
    played: bool,
    play_count: u32,
    is_favorite: bool,
    playback_position_ticks: u64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct NameGuidPairDto {
    name: String,
}

pub fn parse_item_detail_response(bytes: &[u8]) -> Result<ItemDetail, ClientError> {
    let parsed: ItemDetailDto =
        serde_json::from_slice(bytes).map_err(|e| ClientError::Parse(e.to_string()))?;
    let has_primary_image = parsed
        .image_tags
        .as_object()
        .map(|m| m.contains_key("Primary"))
        .unwrap_or(false);
    let has_backdrop_image = !parsed.backdrop_image_tags.is_empty();
    let people = parsed
        .people
        .into_iter()
        .map(|p| ItemPerson {
            id: p.id,
            name: p.name,
            kind: p.kind,
            role: p.role,
            has_image: p.primary_image_tag.is_some(),
        })
        .collect();
    Ok(ItemDetail {
        id: parsed.id,
        name: parsed.name,
        kind: ItemKind::from_jellyfin_type(&parsed.kind),
        run_time_ticks: parsed.run_time_ticks,
        played: parsed.user_data.played,
        play_count: parsed.user_data.play_count,
        is_favorite: parsed.user_data.is_favorite,
        playback_position_ticks: parsed.user_data.playback_position_ticks,
        series_name: parsed.series_name,
        season_index: parsed.parent_index_number,
        episode_index: parsed.index_number,
        artists: parsed.artists,
        album: parsed.album,
        album_artists: parsed.album_artists.into_iter().map(|p| p.name).collect(),
        has_primary_image,
        has_backdrop_image,
        people,
        overview: parsed.overview,
        genres: parsed.genres,
        chapters: parsed
            .chapters
            .into_iter()
            .map(|c| ItemChapter {
                name: c.name,
                start_position_ticks: c.start_position_ticks,
            })
            .collect(),
    })
}

pub fn parse_search_hints_response(bytes: &[u8]) -> Result<Vec<SearchHint>, ClientError> {
    let parsed: SearchHintsResultDto =
        serde_json::from_slice(bytes).map_err(|e| ClientError::Parse(e.to_string()))?;
    Ok(parsed
        .search_hints
        .into_iter()
        .map(|h| SearchHint {
            id: h.id,
            name: h.name,
            kind: ItemKind::from_jellyfin_type(&h.kind),
            matched_term: h.matched_term,
        })
        .collect())
}

/// T55 — UserConfiguration projection. Subset of jellyfin-web's
/// `Configuration` block — fields the preferences UI flips. Strings
/// kept as `String` so the form can round-trip arbitrary values.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UserConfiguration {
    pub audio_language_preference: String,
    pub subtitle_language_preference: String,
    pub subtitle_mode: String,
    pub play_default_audio_track: bool,
    pub display_missing_episodes: bool,
    pub display_collections_view: bool,
    pub hide_played_in_latest: bool,
    pub remember_audio_selections: bool,
    pub remember_subtitle_selections: bool,
    pub enable_next_episode_auto_play: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
struct UserConfigurationDto {
    audio_language_preference: String,
    subtitle_language_preference: String,
    subtitle_mode: String,
    play_default_audio_track: bool,
    display_missing_episodes: bool,
    display_collections_view: bool,
    hide_played_in_latest: bool,
    remember_audio_selections: bool,
    remember_subtitle_selections: bool,
    enable_next_episode_auto_play: bool,
}

impl Default for UserConfigurationDto {
    fn default() -> Self {
        Self {
            audio_language_preference: String::new(),
            subtitle_language_preference: String::new(),
            subtitle_mode: "Default".into(),
            play_default_audio_track: true,
            display_missing_episodes: false,
            display_collections_view: false,
            hide_played_in_latest: true,
            remember_audio_selections: true,
            remember_subtitle_selections: true,
            enable_next_episode_auto_play: true,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct UserMeDto {
    #[serde(default)]
    configuration: UserConfigurationDto,
}

pub fn parse_user_configuration_response(bytes: &[u8]) -> Result<UserConfiguration, ClientError> {
    let parsed: UserMeDto =
        serde_json::from_slice(bytes).map_err(|e| ClientError::Parse(e.to_string()))?;
    let c = parsed.configuration;
    Ok(UserConfiguration {
        audio_language_preference: c.audio_language_preference,
        subtitle_language_preference: c.subtitle_language_preference,
        subtitle_mode: c.subtitle_mode,
        play_default_audio_track: c.play_default_audio_track,
        display_missing_episodes: c.display_missing_episodes,
        display_collections_view: c.display_collections_view,
        hide_played_in_latest: c.hide_played_in_latest,
        remember_audio_selections: c.remember_audio_selections,
        remember_subtitle_selections: c.remember_subtitle_selections,
        enable_next_episode_auto_play: c.enable_next_episode_auto_play,
    })
}

pub fn user_configuration_to_dto_json(c: &UserConfiguration) -> serde_json::Value {
    serde_json::json!({
        "AudioLanguagePreference": c.audio_language_preference,
        "SubtitleLanguagePreference": c.subtitle_language_preference,
        "SubtitleMode": c.subtitle_mode,
        "PlayDefaultAudioTrack": c.play_default_audio_track,
        "DisplayMissingEpisodes": c.display_missing_episodes,
        "DisplayCollectionsView": c.display_collections_view,
        "HidePlayedInLatest": c.hide_played_in_latest,
        "RememberAudioSelections": c.remember_audio_selections,
        "RememberSubtitleSelections": c.remember_subtitle_selections,
        "EnableNextEpisodeAutoPlay": c.enable_next_episode_auto_play,
    })
}

/// T56 — Live TV channel projection. `id` is the upstream tvg-id.
/// `logo_url` is the public `/livetv/channels/{id}/images/primary`
/// redirect path so the UI can render `<img src=...>` without
/// composing the upstream URL itself.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LiveChannel {
    pub id: String,
    pub name: String,
    pub number: String,
    pub group: Option<String>,
    pub has_logo: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LiveProgram {
    pub id: String,
    pub channel_id: String,
    pub title: String,
    pub overview: Option<String>,
    pub start_iso: String,
    pub end_iso: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct LiveChannelDto {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    channel_number: String,
    #[serde(default)]
    channel_group_name: Option<String>,
    #[serde(default)]
    image_tags: serde_json::Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct LiveChannelsResultDto {
    #[serde(default)]
    items: Vec<LiveChannelDto>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct LiveProgramDto {
    #[serde(default)]
    id: String,
    #[serde(default)]
    channel_id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    overview: Option<String>,
    #[serde(default)]
    start_date: String,
    #[serde(default)]
    end_date: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct LiveProgramsResultDto {
    #[serde(default)]
    items: Vec<LiveProgramDto>,
}

pub fn parse_live_channels_response(bytes: &[u8]) -> Result<Vec<LiveChannel>, ClientError> {
    let parsed: LiveChannelsResultDto =
        serde_json::from_slice(bytes).map_err(|e| ClientError::Parse(e.to_string()))?;
    Ok(parsed
        .items
        .into_iter()
        .map(|c| {
            let has_logo = c
                .image_tags
                .as_object()
                .map(|m| m.contains_key("Primary"))
                .unwrap_or(false);
            LiveChannel {
                id: c.id,
                name: c.name,
                number: c.channel_number,
                group: c.channel_group_name,
                has_logo,
            }
        })
        .collect())
}

pub fn parse_live_programs_response(bytes: &[u8]) -> Result<Vec<LiveProgram>, ClientError> {
    let parsed: LiveProgramsResultDto =
        serde_json::from_slice(bytes).map_err(|e| ClientError::Parse(e.to_string()))?;
    Ok(parsed
        .items
        .into_iter()
        .map(|p| LiveProgram {
            id: p.id,
            channel_id: p.channel_id,
            title: p.name,
            overview: p.overview,
            start_iso: p.start_date,
            end_iso: p.end_date,
        })
        .collect())
}

/// T60 — `SessionInfo` projection from `/Sessions` (snapshot of the
/// SessionRegistry actor). Includes the now-playing fields the remote-
/// control UI needs to show a per-session status line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RemoteSession {
    pub id: String,
    pub user_id: String,
    pub user_name: String,
    pub device_name: String,
    pub client: String,
    pub now_playing_item_id: Option<String>,
    pub position_ticks: u64,
    pub is_paused: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RemoteSessionDto {
    #[serde(default)]
    id: String,
    #[serde(default)]
    user_id: String,
    #[serde(default)]
    user_name: String,
    #[serde(default)]
    device_name: String,
    #[serde(default)]
    client: String,
    #[serde(default)]
    now_playing_item_id: Option<String>,
    #[serde(default)]
    position_ticks: u64,
    #[serde(default)]
    is_paused: bool,
}

pub fn parse_sessions_response(bytes: &[u8]) -> Result<Vec<RemoteSession>, ClientError> {
    let parsed: Vec<RemoteSessionDto> =
        serde_json::from_slice(bytes).map_err(|e| ClientError::Parse(e.to_string()))?;
    Ok(parsed
        .into_iter()
        .map(|s| RemoteSession {
            id: s.id,
            user_id: s.user_id,
            user_name: s.user_name,
            device_name: s.device_name,
            client: s.client,
            now_playing_item_id: s.now_playing_item_id,
            position_ticks: s.position_ticks,
            is_paused: s.is_paused,
        })
        .collect())
}

/// T58 phase 2 — `/ScheduledTasks` row. Server emits an empty array
/// today; the projection types so the UI renders the table chrome.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScheduledTask {
    pub id: String,
    pub name: String,
    pub category: String,
    pub state: String,
    pub last_execution_iso: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ScheduledTaskDto {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    last_execution_result: serde_json::Value,
}

pub fn parse_scheduled_tasks_response(
    bytes: &[u8],
) -> Result<Vec<ScheduledTask>, ClientError> {
    let parsed: Vec<ScheduledTaskDto> =
        serde_json::from_slice(bytes).map_err(|e| ClientError::Parse(e.to_string()))?;
    Ok(parsed
        .into_iter()
        .map(|t| ScheduledTask {
            id: t.id,
            name: t.name,
            category: t.category,
            state: if t.state.is_empty() {
                "Idle".to_string()
            } else {
                t.state
            },
            last_execution_iso: t
                .last_execution_result
                .get("EndTimeUtc")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        })
        .collect())
}

/// T58 phase 2 — `/Plugins` row.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PluginEntry {
    pub id: String,
    pub name: String,
    pub version: String,
    pub status: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PluginDto {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    status: String,
}

pub fn parse_plugins_response(bytes: &[u8]) -> Result<Vec<PluginEntry>, ClientError> {
    let parsed: Vec<PluginDto> =
        serde_json::from_slice(bytes).map_err(|e| ClientError::Parse(e.to_string()))?;
    Ok(parsed
        .into_iter()
        .map(|p| PluginEntry {
            id: p.id,
            name: p.name,
            version: p.version,
            status: p.status,
        })
        .collect())
}

/// T58 phase 3 — `/Auth/Keys` row.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ApiKey {
    pub id: String,
    pub app_name: String,
    pub date_created_iso: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ApiKeyDto {
    #[serde(default)]
    id: String,
    #[serde(default)]
    app_name: String,
    #[serde(default)]
    date_created: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ApiKeysResultDto {
    #[serde(default)]
    items: Vec<ApiKeyDto>,
}

pub fn parse_api_keys_response(bytes: &[u8]) -> Result<Vec<ApiKey>, ClientError> {
    let parsed: ApiKeysResultDto =
        serde_json::from_slice(bytes).map_err(|e| ClientError::Parse(e.to_string()))?;
    Ok(parsed
        .items
        .into_iter()
        .map(|k| ApiKey {
            id: k.id,
            app_name: k.app_name,
            date_created_iso: k.date_created,
        })
        .collect())
}

/// T58 phase 3 — `/Auth/Keys` POST result. The raw `access_token` is
/// returned ONCE; the UI surfaces it then drops it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NewApiKey {
    pub id: String,
    pub app_name: String,
    pub access_token: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct NewApiKeyDto {
    #[serde(default)]
    id: String,
    #[serde(default)]
    app_name: String,
    #[serde(default)]
    access_token: String,
}

pub fn parse_new_api_key_response(bytes: &[u8]) -> Result<NewApiKey, ClientError> {
    let parsed: NewApiKeyDto =
        serde_json::from_slice(bytes).map_err(|e| ClientError::Parse(e.to_string()))?;
    Ok(NewApiKey {
        id: parsed.id,
        app_name: parsed.app_name,
        access_token: parsed.access_token,
    })
}

/// T58 phase 2 — `/System/Logs` row.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LogEntry {
    pub name: String,
    pub size_bytes: u64,
    pub date_modified_iso: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct LogEntryDto {
    #[serde(default)]
    name: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    date_modified: String,
}

pub fn parse_logs_response(bytes: &[u8]) -> Result<Vec<LogEntry>, ClientError> {
    let parsed: Vec<LogEntryDto> =
        serde_json::from_slice(bytes).map_err(|e| ClientError::Parse(e.to_string()))?;
    Ok(parsed
        .into_iter()
        .map(|e| LogEntry {
            name: e.name,
            size_bytes: e.size,
            date_modified_iso: e.date_modified,
        })
        .collect())
}

/// T58 — VirtualFolder ("library") entry from `/Library/VirtualFolders`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LibraryFolder {
    pub item_id: String,
    pub name: String,
    pub collection_type: String,
    pub locations: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct VirtualFolderDto {
    #[serde(default)]
    item_id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    collection_type: String,
    #[serde(default)]
    locations: Vec<String>,
}

pub fn parse_virtual_folders_response(bytes: &[u8]) -> Result<Vec<LibraryFolder>, ClientError> {
    let parsed: Vec<VirtualFolderDto> =
        serde_json::from_slice(bytes).map_err(|e| ClientError::Parse(e.to_string()))?;
    Ok(parsed
        .into_iter()
        .map(|d| LibraryFolder {
            item_id: d.item_id,
            name: d.name,
            collection_type: d.collection_type,
            locations: d.locations,
        })
        .collect())
}

/// T58 — `/Devices` entry. jellyfin-web's dashboard reads this to render
/// the connected-clients list.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeviceEntry {
    pub id: String,
    pub name: String,
    pub app_name: String,
    pub last_user_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DeviceDto {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    app_name: String,
    #[serde(default)]
    last_user_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DevicesResultDto {
    #[serde(default)]
    items: Vec<DeviceDto>,
}

pub fn parse_devices_response(bytes: &[u8]) -> Result<Vec<DeviceEntry>, ClientError> {
    // /Devices returns the same wrapper jellyfin uses for most lists.
    let parsed: DevicesResultDto =
        serde_json::from_slice(bytes).map_err(|e| ClientError::Parse(e.to_string()))?;
    Ok(parsed
        .items
        .into_iter()
        .map(|d| DeviceEntry {
            id: d.id,
            name: d.name,
            app_name: d.app_name,
            last_user_name: d.last_user_name,
        })
        .collect())
}

/// T58 — `/System/ActivityLog/Entries` row (jellyfin-web's Activity Log
/// dashboard). Pharos returns an empty list today; the projection
/// types so the UI is in place when entries land.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ActivityEntry {
    pub id: String,
    pub name: String,
    pub short_overview: String,
    pub date_iso: String,
    pub severity: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ActivityEntryDto {
    #[serde(default)]
    id: serde_json::Value,
    #[serde(default)]
    name: String,
    #[serde(default)]
    short_overview: String,
    #[serde(default)]
    date: String,
    #[serde(default)]
    severity: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ActivityEntriesResultDto {
    #[serde(default)]
    items: Vec<ActivityEntryDto>,
}

pub fn parse_activity_entries_response(bytes: &[u8]) -> Result<Vec<ActivityEntry>, ClientError> {
    let parsed: ActivityEntriesResultDto =
        serde_json::from_slice(bytes).map_err(|e| ClientError::Parse(e.to_string()))?;
    Ok(parsed
        .items
        .into_iter()
        .map(|d| ActivityEntry {
            id: id_to_string(&d.id),
            name: d.name,
            short_overview: d.short_overview,
            date_iso: d.date,
            severity: d.severity,
        })
        .collect())
}

fn id_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

pub fn parse_admin_users_response(bytes: &[u8]) -> Result<Vec<AdminUser>, ClientError> {
    let parsed: Vec<AdminUserDto> =
        serde_json::from_slice(bytes).map_err(|e| ClientError::Parse(e.to_string()))?;
    Ok(parsed
        .into_iter()
        .map(|u| AdminUser {
            id: u.id,
            name: u.name,
            is_admin: u.policy.is_administrator,
        })
        .collect())
}

#[cfg(feature = "web")]
pub mod web {
    //! gloo-net HTTP wrappers. Browser-only. Each call composes the
    //! relevant parse helper above so unit tests of the parse layer
    //! cover the JSON contract.

    use super::*;
    use gloo_net::http::Request;

    #[derive(Serialize)]
    #[serde(rename_all = "PascalCase")]
    struct AuthRequestBody<'a> {
        username: &'a str,
        pw: &'a str,
    }

    pub async fn authenticate(
        base: &str,
        username: &str,
        password: &str,
    ) -> Result<LoggedInUser, ClientError> {
        let body = serde_json::to_string(&AuthRequestBody {
            username,
            pw: password,
        })
        .map_err(|e| ClientError::Parse(e.to_string()))?;
        let resp = Request::post(&format!("{base}/Users/AuthenticateByName"))
            .header("Content-Type", "application/json")
            .body(body)
            .map_err(|e| ClientError::Http(e.to_string()))?
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            return Err(ClientError::Status(resp.status()));
        }
        let bytes = resp
            .binary()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        parse_auth_response(&bytes)
    }

    pub async fn list_items(base: &str, token: &str) -> Result<Vec<LibraryItem>, ClientError> {
        let resp = Request::get(&format!("{base}/Items"))
            .header("X-Emby-Token", token)
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            return Err(ClientError::Status(resp.status()));
        }
        let bytes = resp
            .binary()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        parse_items_response(&bytes)
    }

    #[derive(Serialize)]
    #[serde(rename_all = "PascalCase")]
    struct CreateUserBody<'a> {
        name: &'a str,
        password: &'a str,
    }

    pub async fn admin_list_users(base: &str, token: &str) -> Result<Vec<AdminUser>, ClientError> {
        let resp = Request::get(&format!("{base}/Users"))
            .header("X-Emby-Token", token)
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            return Err(ClientError::Status(resp.status()));
        }
        let bytes = resp
            .binary()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        parse_admin_users_response(&bytes)
    }

    pub async fn admin_create_user(
        base: &str,
        token: &str,
        name: &str,
        password: &str,
    ) -> Result<(), ClientError> {
        let body = serde_json::to_string(&CreateUserBody { name, password })
            .map_err(|e| ClientError::Parse(e.to_string()))?;
        let resp = Request::post(&format!("{base}/Users/New"))
            .header("X-Emby-Token", token)
            .header("Content-Type", "application/json")
            .body(body)
            .map_err(|e| ClientError::Http(e.to_string()))?
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            return Err(ClientError::Status(resp.status()));
        }
        Ok(())
    }

    pub async fn admin_set_user_policy(
        base: &str,
        token: &str,
        user_id: &str,
        is_admin: bool,
    ) -> Result<(), ClientError> {
        let body = serde_json::json!({ "IsAdministrator": is_admin }).to_string();
        let resp = Request::post(&format!("{base}/Users/{user_id}/Policy"))
            .header("X-Emby-Token", token)
            .header("Content-Type", "application/json")
            .body(body)
            .map_err(|e| ClientError::Http(e.to_string()))?
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            return Err(ClientError::Status(resp.status()));
        }
        Ok(())
    }

    /// T50 phase 2 — admin password reset for another user. Self-reset
    /// requires CurrentPw, so admins reset only other users this way;
    /// the caller-side check sits in `AdminUserRow`.
    pub async fn admin_reset_user_password(
        base: &str,
        token: &str,
        user_id: &str,
        new_password: &str,
    ) -> Result<(), ClientError> {
        let body = serde_json::json!({
            "NewPw": new_password,
            "ResetPassword": false,
        })
        .to_string();
        let resp = Request::post(&format!("{base}/Users/{user_id}/Password"))
            .header("X-Emby-Token", token)
            .header("Content-Type", "application/json")
            .body(body)
            .map_err(|e| ClientError::Http(e.to_string()))?
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            return Err(ClientError::Status(resp.status()));
        }
        Ok(())
    }

    pub async fn admin_delete_user(
        base: &str,
        token: &str,
        user_id: &str,
    ) -> Result<(), ClientError> {
        let resp = Request::delete(&format!("{base}/Users/{user_id}"))
            .header("X-Emby-Token", token)
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            return Err(ClientError::Status(resp.status()));
        }
        Ok(())
    }

    pub async fn fetch_item_detail(
        base: &str,
        token: &str,
        id: &str,
    ) -> Result<ItemDetail, ClientError> {
        let resp = Request::get(&format!("{base}/Items/{id}"))
            .header("X-Emby-Token", token)
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            return Err(ClientError::Status(resp.status()));
        }
        let bytes = resp
            .binary()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        parse_item_detail_response(&bytes)
    }

    pub async fn mark_played(
        base: &str,
        token: &str,
        user_id: &str,
        item_id: &str,
        played: bool,
    ) -> Result<(), ClientError> {
        let method = if played { "POST" } else { "DELETE" };
        let req = match method {
            "POST" => Request::post(&format!("{base}/Users/{user_id}/PlayedItems/{item_id}")),
            _ => Request::delete(&format!("{base}/Users/{user_id}/PlayedItems/{item_id}")),
        };
        let resp = req
            .header("X-Emby-Token", token)
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            return Err(ClientError::Status(resp.status()));
        }
        Ok(())
    }

    pub async fn mark_favorite(
        base: &str,
        token: &str,
        user_id: &str,
        item_id: &str,
        favorite: bool,
    ) -> Result<(), ClientError> {
        let req = if favorite {
            Request::post(&format!("{base}/Users/{user_id}/FavoriteItems/{item_id}"))
        } else {
            Request::delete(&format!("{base}/Users/{user_id}/FavoriteItems/{item_id}"))
        };
        let resp = req
            .header("X-Emby-Token", token)
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            return Err(ClientError::Status(resp.status()));
        }
        Ok(())
    }

    pub async fn search_hints(
        base: &str,
        token: &str,
        term: &str,
    ) -> Result<Vec<SearchHint>, ClientError> {
        let qs = if term.is_empty() {
            String::new()
        } else {
            format!("?searchTerm={}", urlencode(term))
        };
        let resp = Request::get(&format!("{base}/Search/Hints{qs}"))
            .header("X-Emby-Token", token)
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            return Err(ClientError::Status(resp.status()));
        }
        let bytes = resp
            .binary()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        parse_search_hints_response(&bytes)
    }

    /// Minimal urlencode for the search term — covers space + `&` +
    /// `?` which is all jellyfin-web's search input emits. Pulls in
    /// no extra crate.
    fn urlencode(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            match c {
                'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => out.push(c),
                _ => {
                    for b in c.to_string().bytes() {
                        out.push_str(&format!("%{b:02X}"));
                    }
                }
            }
        }
        out
    }

    pub async fn live_channels(base: &str, token: &str) -> Result<Vec<LiveChannel>, ClientError> {
        let resp = Request::get(&format!("{base}/LiveTv/Channels"))
            .header("X-Emby-Token", token)
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            return Err(ClientError::Status(resp.status()));
        }
        let bytes = resp
            .binary()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        parse_live_channels_response(&bytes)
    }

    pub async fn live_programs(
        base: &str,
        token: &str,
        window_hours: u32,
    ) -> Result<Vec<LiveProgram>, ClientError> {
        let resp = Request::get(&format!(
            "{base}/LiveTv/Programs?windowHours={window_hours}"
        ))
        .header("X-Emby-Token", token)
        .send()
        .await
        .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            return Err(ClientError::Status(resp.status()));
        }
        let bytes = resp
            .binary()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        parse_live_programs_response(&bytes)
    }

    pub async fn list_sessions(base: &str, token: &str) -> Result<Vec<RemoteSession>, ClientError> {
        let resp = Request::get(&format!("{base}/Sessions"))
            .header("X-Emby-Token", token)
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            return Err(ClientError::Status(resp.status()));
        }
        let bytes = resp
            .binary()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        parse_sessions_response(&bytes)
    }

    pub async fn send_session_playstate(
        base: &str,
        token: &str,
        session_id: &str,
        command: &str,
        arg: serde_json::Value,
    ) -> Result<(), ClientError> {
        let body = arg.to_string();
        let resp = Request::post(&format!("{base}/Sessions/{session_id}/Playing/{command}"))
            .header("X-Emby-Token", token)
            .header("Content-Type", "application/json")
            .body(body)
            .map_err(|e| ClientError::Http(e.to_string()))?
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            return Err(ClientError::Status(resp.status()));
        }
        Ok(())
    }

    pub async fn send_session_general(
        base: &str,
        token: &str,
        session_id: &str,
        command: &str,
        arg: serde_json::Value,
    ) -> Result<(), ClientError> {
        let body = arg.to_string();
        let resp = Request::post(&format!("{base}/Sessions/{session_id}/Command/{command}"))
            .header("X-Emby-Token", token)
            .header("Content-Type", "application/json")
            .body(body)
            .map_err(|e| ClientError::Http(e.to_string()))?
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            return Err(ClientError::Status(resp.status()));
        }
        Ok(())
    }

    pub async fn list_api_keys(
        base: &str,
        token: &str,
    ) -> Result<Vec<ApiKey>, ClientError> {
        let resp = Request::get(&format!("{base}/Auth/Keys"))
            .header("X-Emby-Token", token)
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            return Err(ClientError::Status(resp.status()));
        }
        let bytes = resp
            .binary()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        parse_api_keys_response(&bytes)
    }

    pub async fn create_api_key(
        base: &str,
        token: &str,
        app_name: &str,
    ) -> Result<NewApiKey, ClientError> {
        let url = format!("{base}/Auth/Keys?App={}", urlencode(app_name));
        let resp = Request::post(&url)
            .header("X-Emby-Token", token)
            .body("")
            .map_err(|e| ClientError::Http(e.to_string()))?
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            return Err(ClientError::Status(resp.status()));
        }
        let bytes = resp
            .binary()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        parse_new_api_key_response(&bytes)
    }

    pub async fn revoke_api_key(
        base: &str,
        token: &str,
        key_id: &str,
    ) -> Result<(), ClientError> {
        let resp = Request::delete(&format!("{base}/Auth/Keys/{}", urlencode(key_id)))
            .header("X-Emby-Token", token)
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            return Err(ClientError::Status(resp.status()));
        }
        Ok(())
    }

    pub async fn list_scheduled_tasks(
        base: &str,
        token: &str,
    ) -> Result<Vec<ScheduledTask>, ClientError> {
        let resp = Request::get(&format!("{base}/ScheduledTasks"))
            .header("X-Emby-Token", token)
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            return Err(ClientError::Status(resp.status()));
        }
        let bytes = resp
            .binary()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        parse_scheduled_tasks_response(&bytes)
    }

    pub async fn list_plugins(
        base: &str,
        token: &str,
    ) -> Result<Vec<PluginEntry>, ClientError> {
        let resp = Request::get(&format!("{base}/Plugins"))
            .header("X-Emby-Token", token)
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            return Err(ClientError::Status(resp.status()));
        }
        let bytes = resp
            .binary()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        parse_plugins_response(&bytes)
    }

    pub async fn list_logs(
        base: &str,
        token: &str,
    ) -> Result<Vec<LogEntry>, ClientError> {
        let resp = Request::get(&format!("{base}/System/Logs"))
            .header("X-Emby-Token", token)
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            return Err(ClientError::Status(resp.status()));
        }
        let bytes = resp
            .binary()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        parse_logs_response(&bytes)
    }

    pub async fn list_virtual_folders(
        base: &str,
        token: &str,
    ) -> Result<Vec<LibraryFolder>, ClientError> {
        let resp = Request::get(&format!("{base}/Library/VirtualFolders"))
            .header("X-Emby-Token", token)
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            return Err(ClientError::Status(resp.status()));
        }
        let bytes = resp
            .binary()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        parse_virtual_folders_response(&bytes)
    }

    pub async fn list_devices(base: &str, token: &str) -> Result<Vec<DeviceEntry>, ClientError> {
        let resp = Request::get(&format!("{base}/Devices"))
            .header("X-Emby-Token", token)
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            return Err(ClientError::Status(resp.status()));
        }
        let bytes = resp
            .binary()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        parse_devices_response(&bytes)
    }

    pub async fn list_activity_entries(
        base: &str,
        token: &str,
    ) -> Result<Vec<ActivityEntry>, ClientError> {
        let resp = Request::get(&format!("{base}/System/ActivityLog/Entries"))
            .header("X-Emby-Token", token)
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            return Err(ClientError::Status(resp.status()));
        }
        let bytes = resp
            .binary()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        parse_activity_entries_response(&bytes)
    }

    pub async fn fetch_user_configuration(
        base: &str,
        token: &str,
    ) -> Result<UserConfiguration, ClientError> {
        let resp = Request::get(&format!("{base}/Users/Me"))
            .header("X-Emby-Token", token)
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            return Err(ClientError::Status(resp.status()));
        }
        let bytes = resp
            .binary()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        parse_user_configuration_response(&bytes)
    }

    pub async fn save_user_configuration(
        base: &str,
        token: &str,
        user_id: &str,
        cfg: &UserConfiguration,
    ) -> Result<(), ClientError> {
        let body = user_configuration_to_dto_json(cfg).to_string();
        let resp = Request::post(&format!("{base}/Users/{user_id}/Configuration"))
            .header("X-Emby-Token", token)
            .header("Content-Type", "application/json")
            .body(body)
            .map_err(|e| ClientError::Http(e.to_string()))?
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            return Err(ClientError::Status(resp.status()));
        }
        Ok(())
    }

    pub async fn admin_library_refresh(base: &str, token: &str) -> Result<(), ClientError> {
        let resp = Request::post(&format!("{base}/Library/Refresh"))
            .header("X-Emby-Token", token)
            .body("")
            .map_err(|e| ClientError::Http(e.to_string()))?
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            return Err(ClientError::Status(resp.status()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const AUTH_BODY: &[u8] = br#"{
        "User": {
            "Name": "ali",
            "Id": "abc123",
            "Policy": {
                "IsAdministrator": true,
                "IsHidden": false,
                "IsDisabled": false,
                "EnableMediaPlayback": true,
                "EnableAllChannels": true,
                "EnableAllFolders": true,
                "EnableAllDevices": true,
                "EnablePublicSharing": false,
                "EnableRemoteAccess": true,
                "EnableContentDeletion": true,
                "EnableContentDownloading": true,
                "EnableSyncTranscoding": true,
                "EnableMediaConversion": true,
                "EnableAudioPlaybackTranscoding": true,
                "EnableVideoPlaybackTranscoding": true,
                "EnablePlaybackRemuxing": true
            }
        },
        "SessionInfo": {},
        "AccessToken": "tok-xyz",
        "ServerId": "srv-1"
    }"#;

    const ITEMS_BODY: &[u8] = br#"{
        "Items": [
            {"Id":"1","Name":"Movie One","Type":"Movie","ServerId":"srv-1","MediaType":"Video","IsFolder":false,"UserData":{"Played":false,"PlayCount":0}},
            {"Id":"2","Name":"Song","Type":"Audio","ServerId":"srv-1","MediaType":"Audio","IsFolder":false,"UserData":{"Played":false,"PlayCount":0}}
        ],
        "TotalRecordCount": 2,
        "StartIndex": 0
    }"#;

    #[test]
    fn parse_auth_extracts_token_and_user() {
        let u = parse_auth_response(AUTH_BODY).unwrap();
        assert_eq!(u.id, "abc123");
        assert_eq!(u.name, "ali");
        assert_eq!(u.access_token, "tok-xyz");
        assert_eq!(u.server_id, "srv-1");
        assert!(u.is_admin);
    }

    #[test]
    fn parse_items_maps_kind_and_drops_unknown_fields() {
        let items = parse_items_response(ITEMS_BODY).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, "1");
        assert_eq!(items[0].kind, ItemKind::Movie);
        assert_eq!(items[1].kind, ItemKind::Audio);
    }

    #[test]
    fn parse_auth_garbage_is_parse_err() {
        let r = parse_auth_response(b"not json");
        assert!(matches!(r, Err(ClientError::Parse(_))));
    }

    #[test]
    fn parse_items_empty_array_returns_empty_vec() {
        let body = br#"{"Items":[],"TotalRecordCount":0,"StartIndex":0}"#;
        let items = parse_items_response(body).unwrap();
        assert!(items.is_empty());
    }

    #[test]
    fn parse_item_detail_episode_extracts_series_and_indices() {
        let body = br#"{
            "Id":"e1","Name":"Pilot","Type":"Episode","RunTimeTicks":24000000000,
            "SeriesName":"Andor","ParentIndexNumber":1,"IndexNumber":3,
            "ImageTags":{"Primary":"abc"},"BackdropImageTags":[],
            "UserData":{"Played":false,"PlayCount":0,"IsFavorite":false,"PlaybackPositionTicks":0}
        }"#;
        let d = parse_item_detail_response(body).unwrap();
        assert_eq!(d.series_name.as_deref(), Some("Andor"));
        assert_eq!(d.season_index, Some(1));
        assert_eq!(d.episode_index, Some(3));
        assert!(d.has_primary_image);
        assert!(!d.has_backdrop_image);
    }

    #[test]
    fn parse_item_detail_audio_extracts_artists_album() {
        let body = br#"{
            "Id":"a1","Name":"Tears in Rain","Type":"Audio","RunTimeTicks":1800000000,
            "Artists":["Vangelis"],"Album":"Blade Runner OST",
            "AlbumArtists":[{"Name":"Vangelis","Id":"va"}],
            "ImageTags":{},"BackdropImageTags":["bg1"],
            "UserData":{"Played":true,"PlayCount":4,"IsFavorite":true,"PlaybackPositionTicks":0}
        }"#;
        let d = parse_item_detail_response(body).unwrap();
        assert_eq!(d.artists, vec!["Vangelis"]);
        assert_eq!(d.album.as_deref(), Some("Blade Runner OST"));
        assert_eq!(d.album_artists, vec!["Vangelis"]);
        assert!(!d.has_primary_image);
        assert!(d.has_backdrop_image);
        assert_eq!(d.play_count, 4);
        assert!(d.is_favorite);
    }

    #[test]
    fn parse_sessions_extracts_user_device_client_and_playback() {
        let body = br#"[
            {"Id":"s1","UserId":"u1","UserName":"ali","DeviceName":"Pixel 9","Client":"Finamp",
             "NowPlayingItemId":"item-9","PositionTicks":1234567890,"IsPaused":false},
            {"Id":"s2","UserId":"u2","UserName":"ben","DeviceName":"AppleTV","Client":"Infuse",
             "PositionTicks":0,"IsPaused":false}
        ]"#;
        let v = parse_sessions_response(body).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].id, "s1");
        assert_eq!(v[0].now_playing_item_id.as_deref(), Some("item-9"));
        assert_eq!(v[0].position_ticks, 1234567890);
        assert!(v[1].now_playing_item_id.is_none());
    }

    #[test]
    fn parse_api_keys_extracts_app_name_and_date() {
        let body = br#"{"Items":[
            {"Id":"apikey:cli","AppName":"cli","DateCreated":"2026-05-28T08:00:00Z"}
        ],"TotalRecordCount":1,"StartIndex":0}"#;
        let v = parse_api_keys_response(body).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].app_name, "cli");
        assert_eq!(v[0].id, "apikey:cli");
    }

    #[test]
    fn parse_new_api_key_extracts_access_token_once() {
        let body = br#"{
            "Id":"apikey:cli","AppName":"cli","AccessToken":"abc123","DateCreated":"2026-05-28T08:00:00Z"
        }"#;
        let v = parse_new_api_key_response(body).unwrap();
        assert_eq!(v.access_token, "abc123");
        assert_eq!(v.app_name, "cli");
    }

    #[test]
    fn parse_scheduled_tasks_handles_empty_and_populated() {
        let empty = b"[]";
        assert!(parse_scheduled_tasks_response(empty).unwrap().is_empty());
        let body = br#"[
            {"Id":"t1","Name":"Library scan","Category":"Library","State":"Idle",
             "LastExecutionResult":{"EndTimeUtc":"2026-05-28T05:00:00Z"}},
            {"Id":"t2","Name":"Refresh metadata","Category":"Library","State":"Running"}
        ]"#;
        let v = parse_scheduled_tasks_response(body).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].name, "Library scan");
        assert_eq!(v[0].state, "Idle");
        assert_eq!(v[0].last_execution_iso, "2026-05-28T05:00:00Z");
        assert_eq!(v[1].state, "Running");
        assert_eq!(v[1].last_execution_iso, "");
    }

    #[test]
    fn parse_plugins_extracts_name_version_status() {
        let body = br#"[
            {"Id":"p1","Name":"Subtitle Edit","Version":"1.2.3","Status":"Active"}
        ]"#;
        let v = parse_plugins_response(body).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "Subtitle Edit");
        assert_eq!(v[0].version, "1.2.3");
    }

    #[test]
    fn parse_logs_extracts_name_size_date() {
        let body = br#"[
            {"Name":"pharos.log","Size":12345,"DateModified":"2026-05-28T07:00:00Z"}
        ]"#;
        let v = parse_logs_response(body).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "pharos.log");
        assert_eq!(v[0].size_bytes, 12345);
    }

    #[test]
    fn parse_virtual_folders_extracts_fields() {
        let body = br#"[
            {"Name":"Movies","ItemId":"a","CollectionType":"movies","Locations":["/data/movies"]},
            {"Name":"Shows","ItemId":"b","CollectionType":"tvshows","Locations":["/data/tv","/mnt/tv"]}
        ]"#;
        let v = parse_virtual_folders_response(body).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].name, "Movies");
        assert_eq!(v[0].collection_type, "movies");
        assert_eq!(v[1].locations.len(), 2);
    }

    #[test]
    fn parse_devices_extracts_id_name_app_user() {
        let body = br#"{"Items":[
            {"Id":"d1","Name":"Pixel 9","AppName":"Finamp","LastUserName":"ali"},
            {"Id":"d2","Name":"AppleTV","AppName":"Infuse","LastUserName":"ben"}
        ],"TotalRecordCount":2,"StartIndex":0}"#;
        let v = parse_devices_response(body).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[1].app_name, "Infuse");
    }

    #[test]
    fn parse_activity_entries_empty_returns_empty_vec() {
        let body = br#"{"Items":[],"TotalRecordCount":0,"StartIndex":0}"#;
        let v = parse_activity_entries_response(body).unwrap();
        assert!(v.is_empty());
    }

    #[test]
    fn parse_activity_entries_accepts_string_or_int_id() {
        let body = br#"{"Items":[
            {"Id":"abc","Name":"login","ShortOverview":"","Date":"2026-05-28T10:00:00Z","Severity":"Info"},
            {"Id":42,"Name":"scan","ShortOverview":"x","Date":"2026-05-28T10:01:00Z","Severity":"Trace"}
        ],"TotalRecordCount":2,"StartIndex":0}"#;
        let v = parse_activity_entries_response(body).unwrap();
        assert_eq!(v[0].id, "abc");
        assert_eq!(v[1].id, "42");
    }

    #[test]
    fn parse_user_configuration_uses_defaults_on_partial_payload() {
        let body = br#"{
            "Configuration":{
                "AudioLanguagePreference":"jpn",
                "SubtitleMode":"None",
                "DisplayMissingEpisodes":true
            }
        }"#;
        let c = parse_user_configuration_response(body).unwrap();
        assert_eq!(c.audio_language_preference, "jpn");
        assert_eq!(c.subtitle_mode, "None");
        assert!(c.display_missing_episodes);
        // serde defaults apply to fields the payload omits.
        assert!(c.play_default_audio_track);
        assert!(c.enable_next_episode_auto_play);
    }

    #[test]
    fn user_configuration_to_dto_json_roundtrips_through_parser() {
        let cfg = UserConfiguration {
            audio_language_preference: "eng".into(),
            subtitle_language_preference: "eng".into(),
            subtitle_mode: "Smart".into(),
            play_default_audio_track: false,
            display_missing_episodes: true,
            display_collections_view: true,
            hide_played_in_latest: false,
            remember_audio_selections: false,
            remember_subtitle_selections: false,
            enable_next_episode_auto_play: false,
        };
        let body = serde_json::json!({ "Configuration": user_configuration_to_dto_json(&cfg) });
        let bytes = body.to_string();
        let parsed = parse_user_configuration_response(bytes.as_bytes()).unwrap();
        assert_eq!(parsed, cfg);
    }

    #[test]
    fn parse_live_channels_extracts_id_name_number_logo() {
        let body = br#"{
            "Items":[
                {"Id":"c1","Name":"BBC One","ChannelNumber":"1","ChannelGroupName":"UK","ImageTags":{"Primary":"c1"}},
                {"Id":"c2","Name":"BBC Two","ChannelNumber":"2","ImageTags":{}}
            ],
            "TotalRecordCount":2,"StartIndex":0
        }"#;
        let v = parse_live_channels_response(body).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].id, "c1");
        assert_eq!(v[0].number, "1");
        assert_eq!(v[0].group.as_deref(), Some("UK"));
        assert!(v[0].has_logo);
        assert!(!v[1].has_logo);
        assert!(v[1].group.is_none());
    }

    #[test]
    fn parse_live_programs_extracts_window_entries() {
        let body = br#"{
            "Items":[
                {"Id":"c1-1","ChannelId":"c1","Name":"News","Overview":"Daily news",
                 "StartDate":"2026-05-28T10:00:00.000Z","EndDate":"2026-05-28T10:30:00.000Z"}
            ],
            "TotalRecordCount":1,"StartIndex":0
        }"#;
        let v = parse_live_programs_response(body).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].channel_id, "c1");
        assert_eq!(v[0].title, "News");
        assert!(v[0].start_iso.starts_with("2026-05-28T10:00"));
    }

    #[test]
    fn parse_item_detail_extracts_chapters() {
        let body = br#"{
            "Id":"m1","Name":"Movie","Type":"Movie","RunTimeTicks":36000000000,
            "Chapters":[
                {"Name":"Opening","StartPositionTicks":0},
                {"Name":"Chapter 2","StartPositionTicks":3000000000}
            ],
            "UserData":{"Played":false,"PlayCount":0,"IsFavorite":false,"PlaybackPositionTicks":0}
        }"#;
        let d = parse_item_detail_response(body).unwrap();
        assert_eq!(d.chapters.len(), 2);
        assert_eq!(d.chapters[0].name, "Opening");
        assert_eq!(d.chapters[0].start_position_ticks, 0);
        assert_eq!(d.chapters[1].start_position_ticks, 3_000_000_000);
    }

    #[test]
    fn parse_item_detail_phase3_extracts_people_overview_genres() {
        let body = br#"{
            "Id":"m1","Name":"Blade Runner","Type":"Movie","RunTimeTicks":70200000000,
            "Overview":"A blade runner hunts replicants.",
            "Genres":["Sci-Fi","Drama","Mystery"],
            "People":[
                {"Id":"p1","Name":"Harrison Ford","Role":"Rick Deckard","Type":"Actor","PrimaryImageTag":"tag1"},
                {"Id":"p2","Name":"Ridley Scott","Role":"","Type":"Director","PrimaryImageTag":null}
            ],
            "UserData":{"Played":false,"PlayCount":0,"IsFavorite":false,"PlaybackPositionTicks":0}
        }"#;
        let d = parse_item_detail_response(body).unwrap();
        assert_eq!(d.overview.as_deref(), Some("A blade runner hunts replicants."));
        assert_eq!(d.genres, vec!["Sci-Fi", "Drama", "Mystery"]);
        assert_eq!(d.people.len(), 2);
        assert_eq!(d.people[0].name, "Harrison Ford");
        assert_eq!(d.people[0].role, "Rick Deckard");
        assert_eq!(d.people[0].kind, "Actor");
        assert!(d.people[0].has_image);
        assert_eq!(d.people[1].name, "Ridley Scott");
        assert_eq!(d.people[1].kind, "Director");
        assert!(!d.people[1].has_image);
    }

    #[test]
    fn parse_item_detail_movie_defaults_have_no_episode_fields() {
        let body = br#"{
            "Id":"m1","Name":"Blade Runner","Type":"Movie","RunTimeTicks":70200000000,
            "UserData":{"Played":false,"PlayCount":0,"IsFavorite":false,"PlaybackPositionTicks":0}
        }"#;
        let d = parse_item_detail_response(body).unwrap();
        assert!(d.series_name.is_none());
        assert!(d.season_index.is_none());
        assert!(d.episode_index.is_none());
        assert!(d.artists.is_empty());
        assert!(d.album.is_none());
        assert!(!d.has_primary_image);
        assert!(!d.has_backdrop_image);
    }
}
