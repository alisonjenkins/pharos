//! Jellyfin response/request DTOs. PascalCase on the wire to match the
//! reference Jellyfin API (V7).

use pharos_core::{User, UserPolicy};
use serde::{Deserialize, Serialize};

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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct AuthenticateByNameRequest {
    pub username: String,
    pub pw: String,
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

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct UserConfigurationDto {
    pub audio_language_preference: &'static str,
    pub play_default_audio_track: bool,
    pub subtitle_language_preference: &'static str,
    pub display_missing_episodes: bool,
    pub grouped_folders: Vec<String>,
    pub subtitle_mode: &'static str,
    pub display_collections_view: bool,
    pub enable_local_password: bool,
    pub ordered_views: Vec<String>,
    pub latest_items_excludes: Vec<String>,
    pub my_media_excludes: Vec<String>,
    pub hide_played_in_latest: bool,
    pub remember_audio_selections: bool,
    pub remember_subtitle_selections: bool,
    pub enable_next_episode_auto_play: bool,
    pub cast_receiver_id: &'static str,
}

impl Default for UserConfigurationDto {
    fn default() -> Self {
        Self {
            audio_language_preference: "",
            play_default_audio_track: true,
            subtitle_language_preference: "",
            display_missing_episodes: false,
            grouped_folders: vec![],
            subtitle_mode: "Default",
            display_collections_view: false,
            enable_local_password: false,
            ordered_views: vec![],
            latest_items_excludes: vec![],
            my_media_excludes: vec![],
            hide_played_in_latest: true,
            remember_audio_selections: true,
            remember_subtitle_selections: true,
            enable_next_episode_auto_play: true,
            cast_receiver_id: "F007D354",
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
    pub screenshot_image_tags: Vec<String>,
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
    pub default_audio_stream_index: u32,
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
/// avoids pulling in `chrono` just for one render path.
fn format_iso8601(unix_secs: i64) -> String {
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
        let default_audio_stream_index = if is_video { 1 } else { 0 };

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
            artists: vec![],
            artist_items: vec![],
            album_artists: vec![],
            genres: vec![],
            genre_items: vec![],
            tags: vec![],
            studios: vec![],
            people: vec![],
            production_locations: vec![],
            provider_ids: serde_json::Map::new(),
            remote_trailers: vec![],
            chapters: vec![],
            trickplay: serde_json::Map::new(),
            external_urls: vec![],
            image_tags: serde_json::Map::new(),
            backdrop_image_tags: vec![],
            screenshot_image_tags: vec![],
        }
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
pub(crate) fn container_for(probe: &pharos_core::MediaProbe, is_video: bool) -> String {
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

pub(crate) fn build_media_streams(
    probe: &pharos_core::MediaProbe,
    is_video: bool,
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
        });
        // Only advertise an audio stream when probe actually found one.
        // Some test fixtures (the BBB WebM corpus) are video-only;
        // fabricating an `aac` stream there breaks DirectPlay because
        // the file has no AAC bytes and the client's audio decoder
        // errors out → "Playback Error".
        if let Some(codec) = probe.audio_codec.clone() {
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
            });
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
        });
    }
    streams
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use pharos_core::MediaProbe;

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
