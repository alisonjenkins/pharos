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
    /// The server refused and said why in the body (the series-identity
    /// calls name the cause: unknown file, provider down).
    #[error("{message}")]
    Rejected { status: u16, message: String },
}

/// The error for a non-2xx answer: the body's text when it has any, else
/// the bare status.
pub fn rejection(status: u16, body: &str) -> ClientError {
    let message = body.trim();
    if message.is_empty() {
        ClientError::Status(status)
    } else {
        ClientError::Rejected {
            status,
            message: message.to_owned(),
        }
    }
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
    /// The server's own `Type`, which `kind` flattens (a book or a folder
    /// becomes `Movie`).
    pub media_type: String,
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
            media_type: h.kind,
            matched_term: h.matched_term,
        })
        .collect())
}

/// T65 / UI — runtime-mutable branding snapshot the dashboard's
/// Branding pane reads + writes. ServerName comes from /System/Info;
/// LoginDisclaimer + CustomCss come from /Branding/Configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BrandingConfig {
    pub server_name: String,
    pub login_disclaimer: String,
    pub custom_css: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct BrandingConfigurationDto {
    #[serde(default)]
    login_disclaimer: String,
    #[serde(default)]
    custom_css: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SystemInfoNameDto {
    #[serde(default)]
    server_name: String,
}

pub fn parse_branding_configuration(bytes: &[u8]) -> Result<(String, String), ClientError> {
    let dto: BrandingConfigurationDto =
        serde_json::from_slice(bytes).map_err(|e| ClientError::Parse(e.to_string()))?;
    Ok((dto.login_disclaimer, dto.custom_css))
}

pub fn parse_system_info_server_name(bytes: &[u8]) -> Result<String, ClientError> {
    let dto: SystemInfoNameDto =
        serde_json::from_slice(bytes).map_err(|e| ClientError::Parse(e.to_string()))?;
    Ok(dto.server_name)
}

/// T63 / UI — `/QuickConnect/Initiate` response.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QuickConnectInitiate {
    pub code: String,
    pub secret: String,
    pub device_id: String,
}

/// T63 / UI — `/QuickConnect/Connect` poll response.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QuickConnectPoll {
    pub authenticated: bool,
    pub access_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct QcInitiateDto {
    #[serde(default)]
    code: String,
    #[serde(default)]
    secret: String,
    #[serde(default)]
    device_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct QcPollDto {
    #[serde(default)]
    authenticated: bool,
    #[serde(default)]
    access_token: Option<String>,
}

pub fn parse_quick_connect_initiate(bytes: &[u8]) -> Result<QuickConnectInitiate, ClientError> {
    let dto: QcInitiateDto =
        serde_json::from_slice(bytes).map_err(|e| ClientError::Parse(e.to_string()))?;
    Ok(QuickConnectInitiate {
        code: dto.code,
        secret: dto.secret,
        device_id: dto.device_id,
    })
}

pub fn parse_quick_connect_poll(bytes: &[u8]) -> Result<QuickConnectPoll, ClientError> {
    let dto: QcPollDto =
        serde_json::from_slice(bytes).map_err(|e| ClientError::Parse(e.to_string()))?;
    Ok(QuickConnectPoll {
        authenticated: dto.authenticated,
        access_token: dto.access_token,
    })
}

/// T55 ph2 — `/Localization/Cultures` row (subset of the upstream
/// Jellyfin DTO — pharos's prefs view only needs the picker label +
/// the two-letter code).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LocalizationCulture {
    pub name: String,
    pub two_letter_iso: String,
    pub three_letter_iso: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct LocalizationCultureDto {
    #[serde(default)]
    name: String,
    #[serde(default, rename = "TwoLetterISOLanguageName")]
    two_letter_iso: String,
    #[serde(default, rename = "ThreeLetterISOLanguageName")]
    three_letter_iso: String,
}

pub fn parse_cultures_response(bytes: &[u8]) -> Result<Vec<LocalizationCulture>, ClientError> {
    let parsed: Vec<LocalizationCultureDto> =
        serde_json::from_slice(bytes).map_err(|e| ClientError::Parse(e.to_string()))?;
    Ok(parsed
        .into_iter()
        .map(|c| LocalizationCulture {
            name: c.name,
            two_letter_iso: c.two_letter_iso,
            three_letter_iso: c.three_letter_iso,
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
    /// The language show descriptions are shown in (ADR-0062); empty is the
    /// server default. Carried so a save does not clear it.
    pub description_language_preference: String,
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
    description_language_preference: String,
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
            description_language_preference: String::new(),
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
        description_language_preference: c.description_language_preference,
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
        "DescriptionLanguagePreference": c.description_language_preference,
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

pub fn parse_scheduled_tasks_response(bytes: &[u8]) -> Result<Vec<ScheduledTask>, ClientError> {
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

/// `/Admin/Media/Duplicates` row: one film the library holds more than once.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct DuplicateFilm {
    pub title: String,
    pub production_year: Option<u32>,
    pub provider_id: Option<String>,
    pub versions: Vec<FilmVersion>,
}

/// One file of a [`DuplicateFilm`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct FilmVersion {
    pub id: String,
    pub path: String,
    pub size_bytes: Option<u64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub video_codec: Option<String>,
    /// Unix seconds.
    pub date_added: Option<i64>,
    pub edition_count: usize,
}

pub fn parse_duplicate_films_response(bytes: &[u8]) -> Result<Vec<DuplicateFilm>, ClientError> {
    serde_json::from_slice(bytes).map_err(|e| ClientError::Parse(e.to_string()))
}

/// `/Admin/Integrity/Damaged` row: a file the integrity scan found
/// damaged, short or unreadable.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct DamagedFile {
    pub media_id: String,
    pub path: Option<String>,
    /// `damaged`, `short` or `unreadable`.
    pub verdict: String,
    pub first_fault: Option<String>,
    pub first_fault_ms: Option<u64>,
    pub demux_errors: u64,
    pub corrupt_packets: u64,
    pub read_errors: u64,
    pub scanned_ms: u64,
    pub declared_ms: Option<u64>,
    /// Where in the media the damage is; empty for a book.
    pub ranges: Vec<DamageRange>,
    /// `movie`, `episode`, `audio` or `book`.
    pub kind: Option<String>,
    pub name: Option<String>,
    pub series_name: Option<String>,
    pub season_number: Option<u32>,
    pub episode_number: Option<u32>,
    /// Unix seconds.
    pub checked_at: Option<i64>,
    pub recheck_pending: bool,
}

/// One damaged span of a file, in ms on the player's timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct DamageRange {
    pub start_ms: u64,
    pub end_ms: u64,
}

/// `/Admin/Integrity/Status`: how far the background check has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct IntegrityStatus {
    pub eligible: u64,
    pub checked: u64,
    pub damaged: u64,
    pub queued: u64,
}

/// The integrity answers the Damaged files tab shows together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrityReport {
    pub status: IntegrityStatus,
    pub files: Vec<DamagedFile>,
    /// Its own result so a replica that cannot run deep checks still shows
    /// the status and the damaged list.
    pub deep_checks: Result<Vec<DeepCheck>, String>,
}

impl Default for IntegrityReport {
    fn default() -> Self {
        Self {
            status: IntegrityStatus::default(),
            files: Vec::new(),
            deep_checks: Ok(Vec::new()),
        }
    }
}

pub fn parse_damaged_files_response(bytes: &[u8]) -> Result<Vec<DamagedFile>, ClientError> {
    serde_json::from_slice(bytes).map_err(|e| ClientError::Parse(e.to_string()))
}

pub fn parse_integrity_status_response(bytes: &[u8]) -> Result<IntegrityStatus, ClientError> {
    serde_json::from_slice(bytes).map_err(|e| ClientError::Parse(e.to_string()))
}

/// `/Admin/Integrity/Deep` row: one on-demand full-decode run. Results live in
/// server memory and are lost on restart.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct DeepCheck {
    pub media_id: String,
    pub path: Option<String>,
    /// `movie`, `episode` or `audio`.
    pub kind: Option<String>,
    pub name: Option<String>,
    pub series_name: Option<String>,
    pub season_number: Option<u32>,
    pub episode_number: Option<u32>,
    /// `queued`, `running`, `clean`, `damaged` or `failed`.
    pub state: String,
    pub errors: u64,
    pub first_error: Option<String>,
    pub first_error_ms: Option<u64>,
    /// Unix seconds.
    pub requested_at: i64,
    /// Unix seconds.
    pub finished_at: Option<i64>,
    pub elapsed_secs: Option<u64>,
    /// Why a `failed` run could not finish.
    pub detail: Option<String>,
}

pub fn parse_deep_checks_response(bytes: &[u8]) -> Result<Vec<DeepCheck>, ClientError> {
    serde_json::from_slice(bytes).map_err(|e| ClientError::Parse(e.to_string()))
}

/// `/admin/series-identity`: what needs a look on the Show identity page
/// (spec 033 §7).
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct SeriesIdentity {
    /// `dry_run` or `apply`.
    pub repair_mode: String,
    pub splits: Vec<SplitFolder>,
    pub flagged: Vec<FlaggedMatch>,
    pub overrides: Vec<HandFix>,
}

/// A folder whose files sit, or would sit, under more than one show.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct SplitFolder {
    pub folder: String,
    /// `applied`, or `pending` while the scanner still has rows to move.
    pub status: String,
    pub groups: Vec<SplitGroup>,
    /// The rows a pending split would still move.
    pub moves: Vec<PendingMove>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct SplitGroup {
    pub series_key: String,
    pub year: Option<u32>,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct PendingMove {
    pub path: String,
    pub from_key: String,
    pub from_year: Option<u32>,
    pub to_key: String,
    pub to_year: Option<u32>,
}

/// A show match the matcher flagged as a close call.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct FlaggedMatch {
    pub series_key: String,
    /// The show's item id, for a link to its page.
    pub series_id: String,
    pub name: String,
    pub provider: Option<String>,
    pub external_id: Option<String>,
    /// Unix seconds.
    pub premiere_date: Option<i64>,
    pub runner_up_id: Option<String>,
    pub runner_up_score: Option<f32>,
}

/// The Series item ref a show's 32-hex id names: the 63-bit hash the
/// server repeats in both halves. `None` for any other id.
pub fn series_item_ref(series_id: &str) -> Option<pharos_client::api::ItemRef> {
    let hex = series_id.trim();
    if hex.len() != 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let (hi, lo) = hex.split_at(16);
    let hash = u64::from_str_radix(lo, 16).ok()?;
    let same = u64::from_str_radix(hi, 16).ok()? == hash;
    (same && hash != 0).then_some(pharos_client::api::ItemRef::Synth(hash))
}

/// A hand fix already on record: where files were sent, or a folder kept
/// together.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct HandFix {
    pub id: i64,
    pub folder: String,
    pub path: Option<String>,
    pub target_key: String,
    pub target_name: Option<String>,
    pub target_year: Option<u32>,
    /// `admin` or `scanner`.
    pub source: String,
    pub keeps_together: bool,
    pub created_by: Option<String>,
    /// Unix seconds.
    pub created_at: i64,
}

/// A show a provider offered for a pick.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct ShowCandidate {
    pub provider: String,
    pub id: String,
    pub name: String,
    pub year: Option<u32>,
}

/// What a fix changed: the paths it moved, and how many episodes it marked
/// to be rebuilt in the background.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct FixDone {
    pub moved: Vec<String>,
    pub rebuilding: usize,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct FlaggedIds {
    series_ids: Vec<String>,
}

pub fn parse_series_identity_response(bytes: &[u8]) -> Result<SeriesIdentity, ClientError> {
    serde_json::from_slice(bytes).map_err(|e| ClientError::Parse(e.to_string()))
}

pub fn parse_flagged_ids_response(bytes: &[u8]) -> Result<Vec<String>, ClientError> {
    serde_json::from_slice::<FlaggedIds>(bytes)
        .map(|f| f.series_ids)
        .map_err(|e| ClientError::Parse(e.to_string()))
}

pub fn parse_show_candidates_response(bytes: &[u8]) -> Result<Vec<ShowCandidate>, ClientError> {
    serde_json::from_slice(bytes).map_err(|e| ClientError::Parse(e.to_string()))
}

pub fn parse_fix_done_response(bytes: &[u8]) -> Result<FixDone, ClientError> {
    serde_json::from_slice(bytes).map_err(|e| ClientError::Parse(e.to_string()))
}

/// The body of a Move: the files, and the group they go to (an existing
/// one by key, or a new one by name and year).
pub fn move_body(
    folder: &str,
    paths: &[String],
    target_key: Option<&str>,
    target_name: Option<&str>,
    target_year: Option<u32>,
) -> String {
    serde_json::json!({
        "Folder": folder,
        "Paths": paths,
        "TargetKey": target_key,
        "TargetName": target_name,
        "TargetYear": target_year,
    })
    .to_string()
}

/// `/Admin/Items/{id}/Metadata`: the fields an admin may edit on one item or
/// show, with their current values.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct ItemMetadata {
    pub id: String,
    /// `item` or `series`.
    pub target: String,
    pub kind: String,
    pub name: String,
    /// Current value by field name; a missing or `null` entry is unset.
    pub fields: serde_json::Map<String, serde_json::Value>,
    /// Field names the admin may change, in display order.
    pub editable: Vec<String>,
    /// Editable fields the scanner no longer overwrites.
    pub locked: Vec<String>,
}

impl ItemMetadata {
    pub fn is_locked(&self, field: &str) -> bool {
        self.locked.iter().any(|f| f == field)
    }
}

pub fn parse_item_metadata_response(bytes: &[u8]) -> Result<ItemMetadata, ClientError> {
    serde_json::from_slice(bytes).map_err(|e| ClientError::Parse(e.to_string()))
}

/// Why a metadata save did not go through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataSaveError {
    /// The server named the field it refused and why.
    Refused {
        field: String,
        reason: String,
    },
    Failed(String),
}

impl std::fmt::Display for MetadataSaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused { field, reason } => write!(f, "{field}: {reason}"),
            Self::Failed(e) => f.write_str(e),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct MetadataRefusalDto {
    field: String,
    reason: String,
}

/// Reads a refused save: a 400 carrying `{"Field","Reason"}` names the field,
/// anything else is a plain failure worded from the body or the status.
pub fn parse_metadata_save_error(status: u16, body: &[u8]) -> MetadataSaveError {
    if status == 400 {
        if let Ok(r) = serde_json::from_slice::<MetadataRefusalDto>(body) {
            return MetadataSaveError::Refused {
                field: r.field,
                reason: r.reason,
            };
        }
    }
    let text = String::from_utf8_lossy(body);
    let text = text.trim();
    MetadataSaveError::Failed(if text.is_empty() {
        format!("status {status}")
    } else {
        text.to_string()
    })
}

/// The POST body: only the fields that changed (written and locked), and the
/// fields handed back to the scanner.
pub fn build_metadata_save_body(
    fields: &[(String, serde_json::Value)],
    unlock: &[String],
) -> String {
    let fields: serde_json::Map<String, serde_json::Value> = fields.iter().cloned().collect();
    serde_json::json!({ "Fields": fields, "Unlock": unlock }).to_string()
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
    /// Audio codecs this device is never sent as a direct play (T130).
    pub no_direct_play_audio_codecs: Vec<String>,
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
    #[serde(default)]
    no_direct_play_audio_codecs: Vec<String>,
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
            no_direct_play_audio_codecs: d.no_direct_play_audio_codecs,
        })
        .collect())
}

/// The `/Devices/Options` body that sets only a device's audio deny list.
pub fn device_audio_deny_body(codecs: &[String]) -> String {
    serde_json::json!({ "NoDirectPlayAudioCodecs": codecs }).to_string()
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

    /// POST `/Sessions/Playing` — playback start reporting.
    pub async fn post_session_playing(
        base: &str,
        token: &str,
        item_id: &str,
        play_session_id: &str,
        position_ticks: u64,
    ) -> Result<(), ClientError> {
        let body = serde_json::json!({
            "ItemId": item_id,
            "PlaySessionId": play_session_id,
            "PositionTicks": position_ticks,
        })
        .to_string();
        let resp = Request::post(&format!("{base}/Sessions/Playing"))
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

    /// POST `/Sessions/Playing/Progress` — periodic progress ping. The
    /// UI throttles these to ~5 s wall-time intervals so the server isn't
    /// hammered on every ontimeupdate.
    pub async fn post_session_playing_progress(
        base: &str,
        token: &str,
        item_id: &str,
        play_session_id: &str,
        position_ticks: u64,
        is_paused: bool,
    ) -> Result<(), ClientError> {
        let body = serde_json::json!({
            "ItemId": item_id,
            "PlaySessionId": play_session_id,
            "PositionTicks": position_ticks,
            "IsPaused": is_paused,
        })
        .to_string();
        let resp = Request::post(&format!("{base}/Sessions/Playing/Progress"))
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

    /// POST `/Sessions/Playing/Stopped` — final position + cleanup.
    pub async fn post_session_playing_stopped(
        base: &str,
        token: &str,
        item_id: &str,
        play_session_id: &str,
        position_ticks: u64,
    ) -> Result<(), ClientError> {
        let body = serde_json::json!({
            "ItemId": item_id,
            "PlaySessionId": play_session_id,
            "PositionTicks": position_ticks,
        })
        .to_string();
        let resp = Request::post(&format!("{base}/Sessions/Playing/Stopped"))
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

    pub async fn list_api_keys(base: &str, token: &str) -> Result<Vec<ApiKey>, ClientError> {
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

    pub async fn revoke_api_key(base: &str, token: &str, key_id: &str) -> Result<(), ClientError> {
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

    pub async fn list_plugins(base: &str, token: &str) -> Result<Vec<PluginEntry>, ClientError> {
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

    pub async fn list_duplicate_films(
        base: &str,
        token: &str,
    ) -> Result<Vec<DuplicateFilm>, ClientError> {
        let resp = Request::get(&format!("{base}/Admin/Media/Duplicates"))
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
        parse_duplicate_films_response(&bytes)
    }

    pub async fn list_damaged_files(
        base: &str,
        token: &str,
    ) -> Result<Vec<DamagedFile>, ClientError> {
        let resp = Request::get(&format!("{base}/Admin/Integrity/Damaged"))
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
        parse_damaged_files_response(&bytes)
    }

    pub async fn integrity_status(base: &str, token: &str) -> Result<IntegrityStatus, ClientError> {
        let resp = Request::get(&format!("{base}/Admin/Integrity/Status"))
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
        parse_integrity_status_response(&bytes)
    }

    /// Queues the file to be read again; the server answers 202.
    pub async fn recheck_damaged_file(
        base: &str,
        token: &str,
        media_id: &str,
    ) -> Result<(), ClientError> {
        let resp = Request::post(&format!(
            "{base}/Admin/Integrity/{}/Recheck",
            urlencode(media_id)
        ))
        .header("X-Emby-Token", token)
        .send()
        .await
        .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            return Err(ClientError::Status(resp.status()));
        }
        Ok(())
    }

    /// One series-identity call; a refusal carries the server's words.
    async fn series_identity_call(
        base: &str,
        token: &str,
        method: gloo_net::http::Method,
        path: &str,
        body: Option<String>,
    ) -> Result<Vec<u8>, ClientError> {
        let req =
            gloo_net::http::RequestBuilder::new(&format!("{base}/admin/series-identity{path}"))
                .method(method)
                .header("X-Emby-Token", token);
        let resp = match body {
            Some(b) => {
                req.header("Content-Type", "application/json")
                    .body(b)
                    .map_err(|e| ClientError::Http(e.to_string()))?
                    .send()
                    .await
            }
            None => req.send().await,
        }
        .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(rejection(status, &text));
        }
        resp.binary()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))
    }

    pub async fn series_identity(base: &str, token: &str) -> Result<SeriesIdentity, ClientError> {
        let bytes =
            series_identity_call(base, token, gloo_net::http::Method::GET, "", None).await?;
        parse_series_identity_response(&bytes)
    }

    pub async fn flagged_series_ids(base: &str, token: &str) -> Result<Vec<String>, ClientError> {
        let bytes =
            series_identity_call(base, token, gloo_net::http::Method::GET, "/flagged", None)
                .await?;
        parse_flagged_ids_response(&bytes)
    }

    pub async fn search_shows(
        base: &str,
        token: &str,
        name: &str,
        year: Option<u32>,
    ) -> Result<Vec<ShowCandidate>, ClientError> {
        let mut path = format!("/search?Name={}", urlencode(name));
        if let Some(y) = year {
            path.push_str(&format!("&Year={y}"));
        }
        let bytes =
            series_identity_call(base, token, gloo_net::http::Method::GET, &path, None).await?;
        parse_show_candidates_response(&bytes)
    }

    pub async fn pick_show(
        base: &str,
        token: &str,
        series_key: &str,
        provider: &str,
        id: &str,
    ) -> Result<FixDone, ClientError> {
        let body = serde_json::json!({ "SeriesKey": series_key, "Provider": provider, "Id": id })
            .to_string();
        let bytes = series_identity_call(
            base,
            token,
            gloo_net::http::Method::POST,
            "/pick",
            Some(body),
        )
        .await?;
        parse_fix_done_response(&bytes)
    }

    pub async fn confirm_show(
        base: &str,
        token: &str,
        series_key: &str,
    ) -> Result<FixDone, ClientError> {
        let body = serde_json::json!({ "SeriesKey": series_key }).to_string();
        let bytes = series_identity_call(
            base,
            token,
            gloo_net::http::Method::POST,
            "/confirm",
            Some(body),
        )
        .await?;
        parse_fix_done_response(&bytes)
    }

    pub async fn move_files(base: &str, token: &str, body: String) -> Result<FixDone, ClientError> {
        let bytes = series_identity_call(
            base,
            token,
            gloo_net::http::Method::POST,
            "/move",
            Some(body),
        )
        .await?;
        parse_fix_done_response(&bytes)
    }

    pub async fn keep_folder_together(
        base: &str,
        token: &str,
        folder: &str,
    ) -> Result<FixDone, ClientError> {
        let body = serde_json::json!({ "Folder": folder }).to_string();
        let bytes = series_identity_call(
            base,
            token,
            gloo_net::http::Method::POST,
            "/keep-together",
            Some(body),
        )
        .await?;
        parse_fix_done_response(&bytes)
    }

    pub async fn remove_hand_fix(base: &str, token: &str, id: i64) -> Result<FixDone, ClientError> {
        let bytes = series_identity_call(
            base,
            token,
            gloo_net::http::Method::DELETE,
            &format!("/overrides/{id}"),
            None,
        )
        .await?;
        parse_fix_done_response(&bytes)
    }

    pub async fn list_deep_checks(base: &str, token: &str) -> Result<Vec<DeepCheck>, ClientError> {
        let resp = Request::get(&format!("{base}/Admin/Integrity/Deep"))
            .header("X-Emby-Token", token)
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            let status = resp.status();
            return match resp.text().await {
                Ok(t) if !t.trim().is_empty() => Err(ClientError::Http(t.trim().to_string())),
                _ => Err(ClientError::Status(status)),
            };
        }
        let bytes = resp
            .binary()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        parse_deep_checks_response(&bytes)
    }

    /// Queues a full decode of the file; the server answers 202 and refuses
    /// with the reason as text (404, 400, 503), which is returned as `Http`.
    pub async fn request_deep_check(
        base: &str,
        token: &str,
        media_id: &str,
    ) -> Result<(), ClientError> {
        let resp = Request::post(&format!(
            "{base}/Admin/Integrity/{}/Deep",
            urlencode(media_id)
        ))
        .header("X-Emby-Token", token)
        .send()
        .await
        .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            let status = resp.status();
            return match resp.text().await {
                Ok(t) if !t.trim().is_empty() => Err(ClientError::Http(t.trim().to_string())),
                _ => Err(ClientError::Status(status)),
            };
        }
        Ok(())
    }

    pub async fn fetch_item_metadata(
        base: &str,
        token: &str,
        id: &str,
    ) -> Result<ItemMetadata, ClientError> {
        let resp = Request::get(&format!("{base}/Admin/Items/{}/Metadata", urlencode(id)))
            .header("X-Emby-Token", token)
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            let status = resp.status();
            return match resp.text().await {
                Ok(t) if !t.trim().is_empty() => Err(ClientError::Http(t.trim().to_string())),
                _ => Err(ClientError::Status(status)),
            };
        }
        let bytes = resp
            .binary()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        parse_item_metadata_response(&bytes)
    }

    /// Writes the changed fields and answers with the item's new view.
    pub async fn save_item_metadata(
        base: &str,
        token: &str,
        id: &str,
        fields: &[(String, serde_json::Value)],
        unlock: &[String],
    ) -> Result<ItemMetadata, MetadataSaveError> {
        let failed = |e: String| MetadataSaveError::Failed(e);
        let resp = Request::post(&format!("{base}/Admin/Items/{}/Metadata", urlencode(id)))
            .header("X-Emby-Token", token)
            .header("Content-Type", "application/json")
            .body(build_metadata_save_body(fields, unlock))
            .map_err(|e| failed(e.to_string()))?
            .send()
            .await
            .map_err(|e| failed(e.to_string()))?;
        let status = resp.status();
        let bytes = resp.binary().await.map_err(|e| failed(e.to_string()))?;
        if !(200..300).contains(&status) {
            return Err(parse_metadata_save_error(status, &bytes));
        }
        parse_item_metadata_response(&bytes).map_err(|e| failed(e.to_string()))
    }

    pub async fn list_logs(base: &str, token: &str) -> Result<Vec<LogEntry>, ClientError> {
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

    pub async fn set_device_audio_deny(
        base: &str,
        token: &str,
        device_id: &str,
        codecs: &[String],
    ) -> Result<(), ClientError> {
        let resp = Request::post(&format!(
            "{base}/Devices/Options?id={}",
            urlencode(device_id)
        ))
        .header("X-Emby-Token", token)
        .header("Content-Type", "application/json")
        .body(super::device_audio_deny_body(codecs))
        .map_err(|e| ClientError::Http(e.to_string()))?
        .send()
        .await
        .map_err(|e| ClientError::Http(e.to_string()))?;
        if !resp.ok() {
            return Err(ClientError::Status(resp.status()));
        }
        Ok(())
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

    pub async fn quick_connect_initiate(
        base: &str,
        device_id: &str,
    ) -> Result<QuickConnectInitiate, ClientError> {
        let header = format!(
            r#"MediaBrowser Client="pharos-ui", Device="browser", DeviceId="{device_id}", Version="0""#,
        );
        let resp = Request::post(&format!("{base}/QuickConnect/Initiate"))
            .header("X-Emby-Authorization", &header)
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
        parse_quick_connect_initiate(&bytes)
    }

    pub async fn quick_connect_authorize(
        base: &str,
        token: &str,
        code: &str,
    ) -> Result<(), ClientError> {
        let resp = Request::post(&format!("{base}/QuickConnect/Authorize?Code={code}"))
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

    pub async fn quick_connect_connect(
        base: &str,
        secret: &str,
    ) -> Result<QuickConnectPoll, ClientError> {
        let resp = Request::get(&format!("{base}/QuickConnect/Connect?Secret={secret}"))
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
        parse_quick_connect_poll(&bytes)
    }

    pub async fn fetch_branding_config(
        base: &str,
        token: &str,
    ) -> Result<BrandingConfig, ClientError> {
        let info_resp = Request::get(&format!("{base}/System/Info"))
            .header("X-Emby-Token", token)
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !info_resp.ok() {
            return Err(ClientError::Status(info_resp.status()));
        }
        let info_bytes = info_resp
            .binary()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        let server_name = parse_system_info_server_name(&info_bytes)?;

        let brand_resp = Request::get(&format!("{base}/Branding/Configuration"))
            .send()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        if !brand_resp.ok() {
            return Err(ClientError::Status(brand_resp.status()));
        }
        let brand_bytes = brand_resp
            .binary()
            .await
            .map_err(|e| ClientError::Http(e.to_string()))?;
        let (login_disclaimer, custom_css) = parse_branding_configuration(&brand_bytes)?;
        Ok(BrandingConfig {
            server_name,
            login_disclaimer,
            custom_css,
        })
    }

    pub async fn save_branding_config(
        base: &str,
        token: &str,
        cfg: &BrandingConfig,
    ) -> Result<(), ClientError> {
        let body = serde_json::json!({
            "ServerName": cfg.server_name,
            "LoginDisclaimer": cfg.login_disclaimer,
            "CustomCss": cfg.custom_css,
        })
        .to_string();
        let resp = Request::post(&format!("{base}/System/Configuration"))
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

    pub async fn list_cultures(
        base: &str,
        token: &str,
    ) -> Result<Vec<LocalizationCulture>, ClientError> {
        let resp = Request::get(&format!("{base}/Localization/Cultures"))
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
        parse_cultures_response(&bytes)
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
    fn parse_duplicate_films_keeps_each_version() {
        let body = br#"[{"Title":"Heat","ProductionYear":1995,"ProviderId":"tmdb:949",
            "Versions":[{"Id":"1","Path":"/m/Heat.mkv","SizeBytes":4000000000,"Width":1920,
            "Height":1080,"VideoCodec":"hevc","DateAdded":1700000000,"EditionCount":0},
            {"Id":"2","Path":"/m/Heat.avi"}]}]"#;
        let v = parse_duplicate_films_response(body).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].provider_id.as_deref(), Some("tmdb:949"));
        assert_eq!(v[0].versions[0].height, Some(1080));
        assert_eq!(v[0].versions[1].path, "/m/Heat.avi");
        assert_eq!(v[0].versions[1].size_bytes, None);
    }

    #[test]
    fn parse_item_metadata_reads_fields_editable_and_locked() {
        let body = br#"{"Id":"abc","Target":"item","Kind":"Movie","Name":"Alien",
            "Fields":{"Title":"Alien","SortName":null,"ProductionYear":1979,"CommunityRating":8.5},
            "Editable":["Title","SortName","ProductionYear"],"Locked":["Title"]}"#;
        let m = parse_item_metadata_response(body).unwrap();
        assert_eq!((m.id.as_str(), m.target.as_str()), ("abc", "item"));
        assert_eq!(m.name, "Alien");
        assert_eq!(m.editable, ["Title", "SortName", "ProductionYear"]);
        assert!(m.is_locked("Title") && !m.is_locked("SortName"));
        assert_eq!(m.fields["ProductionYear"], serde_json::json!(1979));
        assert!(m.fields["SortName"].is_null());
    }

    #[test]
    fn parse_item_metadata_reads_a_show_with_genres() {
        let body = br#"{"Id":"s1","Target":"series","Kind":"Series","Name":"Show",
            "Fields":{"Genres":["Drama","Sci-Fi"]},"Editable":["Genres"]}"#;
        let m = parse_item_metadata_response(body).unwrap();
        assert_eq!(m.fields["Genres"], serde_json::json!(["Drama", "Sci-Fi"]));
        assert!(m.locked.is_empty());
    }

    #[test]
    fn parse_item_metadata_rejects_non_json() {
        assert!(matches!(
            parse_item_metadata_response(b"nope"),
            Err(ClientError::Parse(_))
        ));
    }

    #[test]
    fn a_400_with_field_and_reason_is_a_refusal_of_that_field() {
        let body =
            br#"{"Field":"ProductionYear","Reason":"ProductionYear 19999 is outside 1850..=2200"}"#;
        assert_eq!(
            parse_metadata_save_error(400, body),
            MetadataSaveError::Refused {
                field: "ProductionYear".into(),
                reason: "ProductionYear 19999 is outside 1850..=2200".into(),
            }
        );
    }

    #[test]
    fn other_save_failures_carry_the_body_or_the_status() {
        assert_eq!(
            parse_metadata_save_error(404, b" no such item \n"),
            MetadataSaveError::Failed("no such item".into())
        );
        assert_eq!(
            parse_metadata_save_error(500, b""),
            MetadataSaveError::Failed("status 500".into())
        );
        assert_eq!(
            parse_metadata_save_error(400, b"not json"),
            MetadataSaveError::Failed("not json".into())
        );
    }

    #[test]
    fn the_save_body_holds_only_the_given_fields_and_unlocks() {
        let body = build_metadata_save_body(
            &[
                ("Title".into(), serde_json::json!("Alien")),
                ("Tagline".into(), serde_json::Value::Null),
            ],
            &["Overview".to_string()],
        );
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"Fields":{"Title":"Alien","Tagline":null},"Unlock":["Overview"]})
        );
        let empty: serde_json::Value =
            serde_json::from_str(&build_metadata_save_body(&[], &[])).unwrap();
        assert_eq!(empty, serde_json::json!({"Fields":{},"Unlock":[]}));
    }

    #[test]
    fn parse_damaged_files_reads_nulls_and_missing_fields() {
        let body = br#"[
            {"MediaId":"123","Path":"/tv/Show/S01E02.mkv","Verdict":"damaged",
             "FirstFault":"Invalid data found","FirstFaultMs":2533000,"DemuxErrors":3,
             "CorruptPackets":4,"ReadErrors":0,"ScannedMs":600000,"DeclaredMs":2700000,
             "Kind":"episode","Name":"Pilot","SeriesName":"Show","SeasonNumber":1,
             "EpisodeNumber":2,"CheckedAt":1762214400,"RecheckPending":true,
             "Ranges":[{"StartMs":2533000,"EndMs":2541000}]},
            {"MediaId":"9","Path":null,"Verdict":"unreadable","FirstFault":null,
             "FirstFaultMs":null,"DemuxErrors":0,"CorruptPackets":0,"ReadErrors":7,
             "ScannedMs":0,"DeclaredMs":null,"Kind":null,"Name":null,"SeriesName":null,
             "SeasonNumber":null,"EpisodeNumber":null,"CheckedAt":null}
        ]"#;
        let v = parse_damaged_files_response(body).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].media_id, "123");
        assert_eq!(v[0].first_fault_ms, Some(2_533_000));
        assert_eq!(v[0].series_name.as_deref(), Some("Show"));
        assert_eq!(v[0].episode_number, Some(2));
        assert!(v[0].recheck_pending);
        assert_eq!(
            v[0].ranges,
            vec![DamageRange {
                start_ms: 2_533_000,
                end_ms: 2_541_000
            }]
        );
        assert!(v[1].ranges.is_empty(), "a server without spans sends none");
        assert_eq!(v[1].path, None);
        assert_eq!(v[1].verdict, "unreadable");
        assert_eq!(v[1].read_errors, 7);
        assert_eq!(v[1].checked_at, None);
        assert!(!v[1].recheck_pending, "a missing RecheckPending is false");
        assert!(parse_damaged_files_response(b"[]").unwrap().is_empty());
        assert!(parse_damaged_files_response(b"{").is_err());
    }

    #[test]
    fn parse_deep_checks_reads_nulls_and_missing_fields() {
        let body = br#"[
            {"MediaId":"123","Path":"/m/Heat.mkv","Kind":"movie","Name":"Heat",
             "SeriesName":null,"SeasonNumber":null,"EpisodeNumber":null,
             "State":"damaged","Errors":4,"FirstError":"Invalid NAL unit size",
             "FirstErrorMs":2533000,"RequestedAt":1762214400,"FinishedAt":1762215000,
             "ElapsedSecs":600,"Detail":null},
            {"MediaId":"9","Path":null,"Kind":null,"Name":null,"State":"queued",
             "Errors":0,"FirstError":null,"FirstErrorMs":null,"RequestedAt":1762216000,
             "FinishedAt":null,"ElapsedSecs":null,"Detail":null}
        ]"#;
        let v = parse_deep_checks_response(body).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].state, "damaged");
        assert_eq!(v[0].errors, 4);
        assert_eq!(v[0].first_error_ms, Some(2_533_000));
        assert_eq!(v[0].finished_at, Some(1_762_215_000));
        assert_eq!(v[0].elapsed_secs, Some(600));
        assert_eq!(v[1].path, None);
        assert_eq!(v[1].finished_at, None);
        assert_eq!(v[1].series_name, None, "a missing SeriesName is None");
        assert_eq!(v[1].requested_at, 1_762_216_000);
        assert!(parse_deep_checks_response(b"[]").unwrap().is_empty());
        assert!(parse_deep_checks_response(b"{").is_err());
    }

    #[test]
    fn parse_integrity_status_reads_the_four_counts() {
        let v = parse_integrity_status_response(
            br#"{"Eligible":9700,"Checked":4120,"Damaged":3,"Queued":2}"#,
        )
        .unwrap();
        assert_eq!(
            v,
            IntegrityStatus {
                eligible: 9700,
                checked: 4120,
                damaged: 3,
                queued: 2
            }
        );
        let partial = parse_integrity_status_response(br#"{"Eligible":5}"#).unwrap();
        assert_eq!((partial.eligible, partial.queued), (5, 0));
    }

    #[test]
    fn parse_series_identity_reads_all_three_lists() {
        let body = br#"{
            "RepairMode":"dry_run",
            "Splits":[{"Folder":"/tv/Cosmos","Status":"pending",
                "Groups":[{"SeriesKey":"/tv/Cosmos\u001f1980","Year":1980,"Paths":["/tv/Cosmos/a.mkv"]},
                          {"SeriesKey":"/tv/Cosmos\u001f2014","Year":2014,"Paths":["/tv/Cosmos/b.mkv","/tv/Cosmos/c.mkv"]}],
                "Moves":[{"Path":"/tv/Cosmos/b.mkv","FromKey":"/tv/Cosmos","FromYear":null,
                          "ToKey":"/tv/Cosmos\u001f2014","ToYear":2014}]}],
            "Flagged":[{"SeriesKey":"Fate","SeriesId":"00ff00ff00ff00ff00ff00ff00ff00ff","Name":"Fate",
                "Provider":"tvdb","ExternalId":"81","PremiereDate":1100000000,
                "RunnerUpId":"82","RunnerUpScore":0.8}],
            "Overrides":[{"Id":7,"Folder":"/tv/Cosmos","Path":null,"TargetKey":"/tv/Cosmos",
                "TargetName":null,"TargetYear":null,"Source":"admin","KeepsTogether":true,
                "CreatedBy":"ali","CreatedAt":1700000000}]
        }"#;
        let v = parse_series_identity_response(body).unwrap();
        assert_eq!(v.repair_mode, "dry_run");
        assert_eq!(v.splits[0].status, "pending");
        assert_eq!(v.splits[0].groups[1].paths.len(), 2);
        assert_eq!(v.splits[0].groups[1].year, Some(2014));
        assert_eq!(v.splits[0].moves[0].from_year, None);
        assert_eq!(v.splits[0].moves[0].to_year, Some(2014));
        assert_eq!(v.flagged[0].runner_up_id.as_deref(), Some("82"));
        assert_eq!(v.flagged[0].premiere_date, Some(1_100_000_000));
        assert!(v.overrides[0].keeps_together);
        assert_eq!(v.overrides[0].created_by.as_deref(), Some("ali"));
    }

    #[test]
    fn parse_series_identity_tolerates_an_empty_page() {
        let v = parse_series_identity_response(br#"{"RepairMode":"apply"}"#).unwrap();
        assert_eq!(v.repair_mode, "apply");
        assert!(v.splits.is_empty() && v.flagged.is_empty() && v.overrides.is_empty());
        assert!(parse_series_identity_response(b"{").is_err());
    }

    #[test]
    fn parse_flagged_ids_reads_the_series_ids() {
        let v = parse_flagged_ids_response(br#"{"SeriesIds":["aa","bb"]}"#).unwrap();
        assert_eq!(v, ["aa", "bb"]);
        assert!(parse_flagged_ids_response(b"{}").unwrap().is_empty());
        assert!(parse_flagged_ids_response(b"[").is_err());
    }

    #[test]
    fn parse_show_candidates_and_fix_done() {
        let c = parse_show_candidates_response(
            br#"[{"Provider":"tvdb","Id":"81","Name":"Cosmos","Year":1980},
                 {"Provider":"tmdb","Id":"9","Name":"Cosmos","Year":null}]"#,
        )
        .unwrap();
        assert_eq!((c[0].provider.as_str(), c[0].year), ("tvdb", Some(1980)));
        assert_eq!(c[1].year, None);
        let d = parse_fix_done_response(br#"{"Moved":["/a","/b"],"Rebuilding":12}"#).unwrap();
        assert_eq!(
            d,
            FixDone {
                moved: vec!["/a".into(), "/b".into()],
                rebuilding: 12
            }
        );
        assert!(parse_fix_done_response(b"x").is_err());
    }

    #[test]
    fn move_body_names_an_existing_group_or_a_new_one() {
        let paths = vec!["/a".to_string()];
        let existing: serde_json::Value =
            serde_json::from_str(&move_body("/tv/X", &paths, Some("k"), None, None)).unwrap();
        assert_eq!(existing["TargetKey"], "k");
        assert!(existing["TargetName"].is_null() && existing["TargetYear"].is_null());
        let fresh: serde_json::Value =
            serde_json::from_str(&move_body("/tv/X", &paths, None, Some("X"), Some(2014))).unwrap();
        assert!(fresh["TargetKey"].is_null());
        assert_eq!(
            (fresh["TargetName"].as_str(), fresh["TargetYear"].as_u64()),
            (Some("X"), Some(2014))
        );
        assert_eq!(fresh["Paths"][0], "/a");
    }

    #[test]
    fn a_refusal_carries_the_servers_words() {
        assert_eq!(
            rejection(404, " no such file \n").to_string(),
            "no such file"
        );
        assert_eq!(rejection(502, "").to_string(), "status 502");
    }

    #[test]
    fn a_series_id_names_its_synth_item() {
        use pharos_client::api::ItemRef;
        assert_eq!(
            series_item_ref("000000000000002a000000000000002a"),
            Some(ItemRef::Synth(42))
        );
        // A library item, a halves mismatch and junk are not shows.
        assert_eq!(series_item_ref("0000000000000000000000000000002a"), None);
        assert_eq!(series_item_ref("000000000000002a000000000000002b"), None);
        assert_eq!(series_item_ref("zz"), None);
        assert_eq!(series_item_ref("+00000000000002a+00000000000002a"), None);
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
    fn parse_devices_reads_the_audio_deny_list_and_defaults_it_empty() {
        let body = br#"{"Items":[
            {"Id":"tv","Name":"TV","NoDirectPlayAudioCodecs":["eac3"]},
            {"Id":"pc","Name":"PC"}
        ]}"#;
        let v = parse_devices_response(body).unwrap();
        assert_eq!(v[0].no_direct_play_audio_codecs, vec!["eac3".to_string()]);
        assert!(v[1].no_direct_play_audio_codecs.is_empty());
    }

    /// The body names only the deny list, so saving it cannot touch the
    /// device's custom name.
    #[test]
    fn the_audio_deny_body_carries_only_the_list() {
        let v: serde_json::Value =
            serde_json::from_str(&device_audio_deny_body(&["ac3".into()])).unwrap();
        assert_eq!(v, serde_json::json!({ "NoDirectPlayAudioCodecs": ["ac3"] }));
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
    fn parse_branding_configuration_extracts_disclaimer_and_css() {
        let body = br#"{"LoginDisclaimer":"Hello","CustomCss":"body { color: red }"}"#;
        let (disc, css) = parse_branding_configuration(body).unwrap();
        assert_eq!(disc, "Hello");
        assert_eq!(css, "body { color: red }");
    }

    #[test]
    fn parse_system_info_server_name_extracts_name() {
        let body = br#"{"ServerName":"My Pharos","Id":"abc","Version":"10.11.0"}"#;
        let name = parse_system_info_server_name(body).unwrap();
        assert_eq!(name, "My Pharos");
    }

    #[test]
    fn parse_quick_connect_initiate_extracts_code_secret() {
        let body = br#"{
            "Code":"123456","Secret":"abc123def","DeviceId":"pw-1","Authenticated":false
        }"#;
        let v = parse_quick_connect_initiate(body).unwrap();
        assert_eq!(v.code, "123456");
        assert_eq!(v.secret, "abc123def");
        assert_eq!(v.device_id, "pw-1");
    }

    #[test]
    fn parse_quick_connect_poll_pending_has_no_token() {
        let body = br#"{"Code":"1","DeviceId":"d","Authenticated":false}"#;
        let v = parse_quick_connect_poll(body).unwrap();
        assert!(!v.authenticated);
        assert!(v.access_token.is_none());
    }

    #[test]
    fn parse_quick_connect_poll_authorized_carries_token() {
        let body = br#"{
            "Code":"1","DeviceId":"d","Authenticated":true,"AccessToken":"tok-xyz"
        }"#;
        let v = parse_quick_connect_poll(body).unwrap();
        assert!(v.authenticated);
        assert_eq!(v.access_token.as_deref(), Some("tok-xyz"));
    }

    #[test]
    fn parse_cultures_extracts_name_and_iso_codes() {
        let body = br#"[
            {"Name":"English","TwoLetterISOLanguageName":"en","ThreeLetterISOLanguageName":"eng"},
            {"Name":"French","TwoLetterISOLanguageName":"fr","ThreeLetterISOLanguageName":"fre"}
        ]"#;
        let v = parse_cultures_response(body).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].name, "English");
        assert_eq!(v[0].two_letter_iso, "en");
        assert_eq!(v[1].three_letter_iso, "fre");
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
            description_language_preference: "fra".into(),
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
        assert_eq!(
            d.overview.as_deref(),
            Some("A blade runner hunts replicants.")
        );
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
