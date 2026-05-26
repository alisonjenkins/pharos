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
            has_password: true,
            has_configured_password: true,
            policy: UserPolicyDto::from_domain(&user.policy),
            configuration: UserConfigurationDto::default(),
            primary_image_aspect_ratio: 1.0,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
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
    pub container: &'static str,
    #[serde(rename = "Type")]
    pub kind: &'static str,
    pub is_remote: bool,
    pub supports_direct_play: bool,
    pub supports_direct_stream: bool,
    pub supports_transcoding: bool,
    pub run_time_ticks: u64,
    pub protocol: &'static str,
    pub media_streams: Vec<MediaStreamDto>,
    pub bitrate: u64,
    pub size: u64,
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
    pub codec: &'static str,
    pub is_default: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channels: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct UserItemDataDto {
    pub played: bool,
    pub play_count: u32,
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
        let kind = match item.kind {
            pharos_core::MediaKind::Movie => "Movie",
            pharos_core::MediaKind::Episode => "Episode",
            pharos_core::MediaKind::Audio => "Audio",
        };
        let media_type = match item.kind {
            pharos_core::MediaKind::Audio => "Audio",
            _ => "Video",
        };
        let container: &'static str = match item.kind {
            pharos_core::MediaKind::Audio => "mp3",
            _ => "webm",
        };
        let run_time_ticks: u64 = 50_000_000; // 5 s; updated once we probe.
        let is_video = !matches!(item.kind, pharos_core::MediaKind::Audio);
        let mut streams = Vec::new();
        if is_video {
            streams.push(MediaStreamDto {
                kind: "Video",
                index: 0,
                codec: "vp9",
                is_default: true,
                width: Some(320),
                height: Some(240),
                channels: None,
                sample_rate: None,
            });
            streams.push(MediaStreamDto {
                kind: "Audio",
                index: 1,
                codec: "opus",
                is_default: true,
                width: None,
                height: None,
                channels: Some(1),
                sample_rate: Some(48000),
            });
        } else {
            streams.push(MediaStreamDto {
                kind: "Audio",
                index: 0,
                codec: "aac",
                is_default: true,
                width: None,
                height: None,
                channels: Some(2),
                sample_rate: Some(44100),
            });
        }
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
                run_time_ticks,
                protocol: "File",
                media_streams: streams,
                bitrate: 200_000,
                size: 107_356,
                name: item.title.clone(),
                default_audio_stream_index: if is_video { 1 } else { 0 },
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
