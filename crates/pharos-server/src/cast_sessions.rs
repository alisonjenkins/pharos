//! Cast: configured TVs presented to stock clients as Jellyfin sessions.
//!
//! jellyfin-web's "play on another device" picker lists whatever
//! `GET /Sessions` (and the socket `Sessions` patch) returns with
//! `SupportsRemoteControl`, so each configured, reachable TV a viewer may use
//! is projected as a session of its own. The projection is derived on every
//! read and never stored; [`virtual_tv_sessions`] is pure and both wire
//! surfaces are built from its output through typed DTOs (V38/V39).

use crate::{config::CastConfig, sessions::SessionRecord};
use pharos_jellyfin_api::socket_messages::{SessionPlayStateLite, SessionsBroadcastEntry};
use std::collections::HashMap;
use uuid::Uuid;

/// Namespace for [`tv_session_id`]. Fixed forever: changing it renames every
/// TV's session id and strands clients that remembered a target.
const TV_SESSION_NAMESPACE: Uuid = Uuid::from_u128(0x7a3c_91e0_5d2b_4f6a_8c14_0b9e_2d57_a1f3);

/// `Client` shown for a TV; also how stock clients recognise a Cast target.
const TV_CLIENT: &str = "Chromecast";

/// Media types a TV can be sent.
const TV_PLAYABLE_MEDIA_TYPES: [&str; 2] = ["Audio", "Video"];

/// The session id for a configured TV: UUIDv5 of its name in a fixed
/// namespace, rendered as 32 lowercase hex digits like every other session
/// id. Stable across restarts and replicas; names are unique
/// case-insensitively in config, so the name is lower-cased first.
pub fn tv_session_id(tv_name: &str) -> String {
    Uuid::new_v5(&TV_SESSION_NAMESPACE, tv_name.to_lowercase().as_bytes())
        .simple()
        .to_string()
}

/// Who is asking; decides which TVs they may see (V9).
#[derive(Debug, Clone)]
pub struct ViewerInfo {
    pub user_id: String,
    pub is_admin: bool,
}

/// A cast that is currently running on a TV.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveCastInfo {
    pub tv: String,
    pub owner_user_id: String,
    pub owner_user_name: String,
    pub now_playing_item_id: Option<String>,
    pub position_ticks: u64,
    pub is_paused: bool,
}

/// What the cast service knows at read time. Implemented by the service that
/// owns the TV connections; faked in tests.
pub trait CastDirectory: Send + Sync {
    /// The `[cast]` block the service was built from.
    fn config(&self) -> &CastConfig;
    /// Per configured TV name, whether its connection is currently usable.
    fn reachable(&self) -> HashMap<String, bool>;
    /// Casts running now, at most one per TV.
    fn active(&self) -> Vec<ActiveCastInfo>;
}

/// One TV as a session. Derived, never stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualTvSession {
    pub id: String,
    pub device_name: String,
    /// Empty unless a cast is active, then the owner.
    pub user_id: String,
    pub user_name: String,
    pub now_playing_item_id: Option<String>,
    pub position_ticks: u64,
    pub is_paused: bool,
}

/// User ids are compared as bare lowercase hex so a dashed and a simple
/// rendering of one id match.
fn normalise_user_id(id: &str) -> String {
    id.chars()
        .filter(|c| *c != '-')
        .flat_map(char::to_lowercase)
        .collect()
}

/// The TVs `viewer` may see: configured, reachable, and permitted
/// (`allowed_users` empty or naming the viewer; admins see every reachable TV).
pub fn virtual_tv_sessions(
    cfg: &CastConfig,
    reachable: &HashMap<String, bool>,
    viewer: &ViewerInfo,
    active: &[ActiveCastInfo],
) -> Vec<VirtualTvSession> {
    let viewer_id = normalise_user_id(&viewer.user_id);
    cfg.tvs
        .iter()
        .filter(|tv| reachable.get(&tv.name).copied().unwrap_or(false))
        .filter(|tv| {
            viewer.is_admin
                || tv.allowed_users.is_empty()
                || tv
                    .allowed_users
                    .iter()
                    .any(|u| normalise_user_id(u) == viewer_id)
        })
        .map(|tv| {
            let cast = active
                .iter()
                .find(|a| a.tv.to_lowercase() == tv.name.to_lowercase());
            VirtualTvSession {
                id: tv_session_id(&tv.name),
                device_name: tv.name.clone(),
                user_id: cast.map(|c| c.owner_user_id.clone()).unwrap_or_default(),
                user_name: cast.map(|c| c.owner_user_name.clone()).unwrap_or_default(),
                now_playing_item_id: cast.and_then(|c| c.now_playing_item_id.clone()),
                position_ticks: cast.map(|c| c.position_ticks).unwrap_or(0),
                is_paused: cast.map(|c| c.is_paused).unwrap_or(false),
            }
        })
        .collect()
}

/// The sessions `viewer` sees through `directory`.
pub fn visible_sessions(
    directory: &dyn CastDirectory,
    viewer: &ViewerInfo,
) -> Vec<VirtualTvSession> {
    virtual_tv_sessions(
        directory.config(),
        &directory.reachable(),
        viewer,
        &directory.active(),
    )
}

/// REST projection: the same record type `GET /Sessions` already serialises.
impl From<&VirtualTvSession> for SessionRecord {
    fn from(v: &VirtualTvSession) -> Self {
        let now = crate::sessions::iso_now();
        Self {
            id: v.id.clone(),
            user_id: v.user_id.clone(),
            user_name: v.user_name.clone(),
            device_id: v.id.clone(),
            device_name: v.device_name.clone(),
            client: TV_CLIENT.into(),
            application_version: String::new(),
            now_playing_item_id: v.now_playing_item_id.clone(),
            position_ticks: v.position_ticks,
            is_paused: v.is_paused,
            playable_media_types: TV_PLAYABLE_MEDIA_TYPES.map(String::from).to_vec(),
            supported_commands: Vec::new(),
            max_streaming_bitrate: None,
            supports_media_control: true,
            is_active: true,
            supports_remote_control: true,
            has_custom_device_name: false,
            last_activity_date: now.clone(),
            last_playback_check_in: now,
            activity_at: tokio::time::Instant::now(),
        }
    }
}

/// Socket projection: the typed `Sessions` patch entry. `NowPlayingItem` is
/// left out because the kotlin `BaseItemDto` requires a `Type` the session
/// does not carry (B78); the flat id and play state still reach the client.
impl From<&VirtualTvSession> for SessionsBroadcastEntry {
    fn from(v: &VirtualTvSession) -> Self {
        let now = crate::sessions::iso_now();
        Self {
            id: v.id.clone(),
            user_id: v.user_id.clone(),
            user_name: Some(v.user_name.clone()),
            device_id: Some(v.id.clone()),
            device_name: Some(v.device_name.clone()),
            client: Some(TV_CLIENT.into()),
            playable_media_types: TV_PLAYABLE_MEDIA_TYPES.to_vec(),
            last_activity_date: now.clone(),
            last_playback_check_in: now,
            is_active: true,
            supports_media_control: true,
            supports_remote_control: true,
            has_custom_device_name: false,
            supported_commands: Vec::new(),
            now_playing_item: None,
            play_state: SessionPlayStateLite {
                position_ticks: v.position_ticks,
                is_paused: v.is_paused,
                can_seek: true,
                is_muted: false,
                repeat_mode: "RepeatNone",
                playback_order: "Default",
            },
        }
    }
}
