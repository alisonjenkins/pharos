//! Jellyfin-shaped `/socket` wire types. The Jellyfin WebSocket is
//! multipurpose — clients tag every message with a top-level
//! `MessageType` field and an opaque `Data` payload. SyncPlay is one
//! family of message types; phase 1 covers just those.
//!
//! V20: this module is the *translation surface* between Jellyfin's
//! wire shapes and pharos's internal `ClientMsg`/`ServerMsg`. The
//! `sync` actor never sees Jellyfin shapes.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Inbound {
    pub message_type: String,
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub data: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Outbound {
    pub message_type: &'static str,
    pub message_id: String,
    pub data: serde_json::Value,
}

impl Outbound {
    pub fn new(message_type: &'static str, data: serde_json::Value) -> Self {
        Self {
            message_type,
            message_id: Uuid::new_v4().simple().to_string(),
            data,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SyncPlayJoinData {
    pub group_id: Uuid,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SyncPlayPlayData {
    #[serde(default)]
    pub playback_position_ticks: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SyncPlaySeekData {
    #[serde(default)]
    pub position_ticks: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SyncPlayBufferingData {
    #[serde(default)]
    pub is_playing: bool,
    #[serde(default)]
    pub playback_position_ticks: u64,
}

/// Jellyfin `SyncPlayGroupUpdate` payload: `{ GroupId, Type, Data }`. `Data`'s
/// shape depends on `Type` — `GroupInfoDto` for `GroupJoined`/`GroupLeft`,
/// [`GroupStateUpdate`] for `StateUpdate`, [`PlayQueueUpdate`] for `PlayQueue`,
/// the username string for `UserJoined`/`UserLeft`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct GroupUpdateData {
    #[serde(rename = "Type")]
    pub kind: &'static str,
    pub group_id: String,
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    pub data: serde_json::Value,
}

/// Jellyfin `GroupInfoDto` — the `Data` of a `GroupJoined`/`GroupLeft` update.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct GroupInfoData {
    pub group_id: String,
    pub group_name: String,
    /// `Idle` | `Waiting` | `Playing` | `Paused`.
    pub state: &'static str,
    pub participants: Vec<String>,
    pub last_updated_at: String,
}

/// Jellyfin `GroupStateUpdate` — the `Data` of a `StateUpdate` group update.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct GroupStateUpdate {
    /// `Idle` | `Waiting` | `Playing` | `Paused`.
    pub state: &'static str,
    /// The command that caused the transition (diagnostic / UI).
    pub reason: String,
}

/// One entry of a `PlayQueueUpdate.Playlist`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct QueuePlaylistItem {
    pub item_id: String,
    pub playlist_item_id: String,
}

/// Jellyfin `PlayQueueUpdate` — the `Data` of a `PlayQueue` group update.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct PlayQueueUpdate {
    pub reason: String,
    pub last_update: String,
    pub playlist: Vec<QueuePlaylistItem>,
    pub playing_item_index: usize,
    pub start_position_ticks: u64,
    pub is_playing: bool,
    pub shuffle_mode: String,
    pub repeat_mode: String,
}

/// Jellyfin `SessionInfoDto.NowPlayingItem` — the minimal `BaseItemDto` a
/// `Sessions` socket broadcast carries. jellyfin-web (loose JS) tolerates a
/// bare `{ "Id": … }`, but the jellyfin-sdk-kotlin `BaseItemDto` deserializer
/// hard-REQUIRES its only two no-default fields, `Id` + `Type`; a `Type`-less
/// object throws `kotlinx.serialization.MissingFieldException` and CRASHES the
/// native Android TV client (B78). Both fields are mandatory here — that is
/// exactly why this is a typed struct and not a `serde_json::json!` literal
/// (the raw literal bypassed the kotlin-DTO audit that owns this contract).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct NowPlayingItemLite {
    pub id: String,
    #[serde(rename = "Type")]
    pub kind: &'static str,
}

/// `SessionInfoDto.PlayState` (kotlin `PlayerStateInfo`). Its no-default
/// REQUIRED fields are `CanSeek`/`IsPaused`/`IsMuted`/`RepeatMode`/
/// `PlaybackOrder` — a native client crashes on any missing one (B78). The
/// two enums must be valid members (`RepeatNone` / `Default`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct SessionPlayStateLite {
    pub position_ticks: u64,
    pub is_paused: bool,
    pub can_seek: bool,
    pub is_muted: bool,
    pub repeat_mode: &'static str,
    pub playback_order: &'static str,
}

/// One `SessionInfoDto` entry of a `Sessions` socket broadcast (P10 — the patch
/// jellyfin-web's "Currently Watching" sidebar + native remote screens apply by
/// `Id`). The kotlin `SessionInfoDto` REQUIRES the capability/timestamp/list
/// fields below (no defaults) in addition to `UserId` — a native client crashes
/// on any missing one (B78). Lists are emitted (possibly empty); DateTimes are
/// real ISO-8601. The capability booleans are conservative defaults — a
/// progress patch doesn't carry the session's registered capabilities (the
/// registry does); false never over-advertises control pharos can't route.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct SessionsBroadcastEntry {
    pub id: String,
    pub user_id: String,
    pub playable_media_types: Vec<&'static str>,
    pub last_activity_date: String,
    pub last_playback_check_in: String,
    pub is_active: bool,
    pub supports_media_control: bool,
    pub supports_remote_control: bool,
    pub has_custom_device_name: bool,
    pub supported_commands: Vec<&'static str>,
    /// Nullable in the kotlin `SessionInfoDto`; OMITTED (not null-with-Id) when
    /// the item's kind couldn't be resolved — never emit a `NowPlayingItem`
    /// without a `Type`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub now_playing_item: Option<NowPlayingItemLite>,
    pub play_state: SessionPlayStateLite,
}

/// Jellyfin `PlaystateMessage.Data` (`PlaystateRequest`) — a remote transport
/// command pushed to a controlled session. `Command` is a `PlaystateCommand`
/// enum on the kotlin side; the emitter must only place a valid member here
/// (B79 — `Play` is NOT a member; it goes out as a `Play` message instead).
/// `ControllingUserId` is a nullable String in the SDK. Typed per B78/V38.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct PlayStateMessageData {
    pub controlling_user_id: String,
    pub session_id: String,
    pub command: String,
    /// `arg["SeekPositionTicks"]` — a number, or `Null` when absent (kotlin
    /// `PlayStateMessage.SeekPositionTicks` is nullable Long).
    pub seek_position_ticks: serde_json::Value,
}

/// Jellyfin `GeneralCommandMessage.Data` (`GeneralCommand`) — a display/volume/
/// etc command pushed to a controlled session. `Name` is a `GeneralCommandType`
/// enum (the emitter must only place a valid member — B79). `ControllingUserId`
/// is a NON-null `UUID` and `Arguments` is a `Map<String,String>` in the SDK, so
/// a native cast-target fails decode on an empty id or non-string arg values
/// (B79). Typed per B78/V38.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct GeneralCommandMessageData {
    pub controlling_user_id: String,
    pub session_id: String,
    pub name: String,
    /// A JSON object whose values are ALL strings (kotlin `Map<String,String>`).
    pub arguments: serde_json::Value,
}

/// Jellyfin `PlayMessage.Data` (`PlayRequest`) — "play these items here", the
/// dedicated cast MessageType (NOT a `PlaystateCommand`; that was the B79 crash
/// — `Play` was wrongly emitted as a PlayState command). `PlayCommand` (enum
/// PlayNow/PlayNext/PlayLast/…) and `ControllingUserId` (UUID) are no-default in
/// the SDK; `ItemIds`/`StartPositionTicks` are nullable and omitted when absent.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct PlayRequestData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_ids: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_position_ticks: Option<i64>,
    pub play_command: String,
    pub controlling_user_id: String,
    pub session_id: String,
}

/// Jellyfin `UserDataChangedMessage.Data` (`UserDataChangeInfo`) — the wrapper
/// around a list of already-typed [`crate::dto::UserItemDataDto`] entries
/// (serialized to Value upstream). Typed wrapper per B78/V38.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct UserDataChangeInfo {
    pub user_id: String,
    pub user_data_list: Vec<serde_json::Value>,
}

/// Jellyfin `LibraryChangedMessage.Data` (`LibraryUpdateInfo`). Every one of
/// the seven fields is no-default (non-nullable `List`/`Boolean`) in the kotlin
/// SDK — a native client that subscribes to `LibraryChanged` fails the whole
/// message on any omission (jellyfin-web tolerates absence). Typed per B78/V38.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct LibraryUpdateInfoData {
    pub folders_added_to: Vec<String>,
    pub folders_removed_from: Vec<String>,
    pub items_added: Vec<String>,
    pub items_removed: Vec<String>,
    pub items_updated: Vec<String>,
    pub collection_folders: Vec<String>,
    pub is_empty: bool,
}

/// Jellyfin `SendCommand` payload (a `SyncPlayCommand` message). The client
/// drops any command whose `PlaylistItemId` doesn't match its current queue
/// item and dedups on `Command`+`PlaylistItemId`, so those fields must stay
/// consistent with the preceding `PlayQueueUpdate`. `When` is the absolute UTC
/// instant to act; `EmittedAt` gates against the client's sync-enable time —
/// both are ISO-8601 UTC (ms precision).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct CommandData {
    /// Non-nullable Guid in the C# SendCommand — always on the wire from
    /// real Jellyfin; strict SDK clients (kotlin native apps) require it.
    pub group_id: String,
    pub command: &'static str,
    /// Nullable in C# — the only optional SendCommand field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_ticks: Option<u64>,
    // When / EmittedAt / PlaylistItemId are non-nullable in C# (DateTime /
    // DateTime / Guid) — always serialized, never skipped: the kotlin SDK
    // fails the whole command otherwise (jellyfin-web tolerates absence).
    pub when: String,
    pub emitted_at: String,
    pub playlist_item_id: String,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn inbound_parses_pascalcase() {
        let raw = r#"{"MessageType":"SyncPlayJoinGroup","MessageId":"abc","Data":{"GroupId":"00000000-0000-0000-0000-000000000001"}}"#;
        let m: Inbound = serde_json::from_str(raw).unwrap();
        assert_eq!(m.message_type, "SyncPlayJoinGroup");
        let join: SyncPlayJoinData = serde_json::from_value(m.data).unwrap();
        assert_eq!(
            join.group_id,
            Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap()
        );
    }

    #[test]
    fn outbound_serializes_pascalcase() {
        let m = Outbound::new(
            "SyncPlayGroupUpdate",
            serde_json::json!({"Type":"GroupJoined","GroupId":"abc"}),
        );
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("\"MessageType\":\"SyncPlayGroupUpdate\""), "{s}");
        assert!(s.contains("\"Type\":\"GroupJoined\""), "{s}");
    }

    #[test]
    fn command_data_serializes_when_emitted_at_and_playlist_item_id() {
        // The client drops a command whose When/EmittedAt/PlaylistItemId are
        // missing or mismatched — assert all three ride the wire in PascalCase.
        let c = CommandData {
            group_id: "g-1".into(),
            command: "Unpause",
            position_ticks: Some(50_000),
            when: "2026-07-10T12:00:00.123Z".into(),
            emitted_at: "2026-07-10T11:59:59.900Z".into(),
            playlist_item_id: "pli-1".into(),
        };
        let s = serde_json::to_string(&c).unwrap();
        assert!(s.contains("\"Command\":\"Unpause\""), "{s}");
        // Non-nullable Guid in the C# SendCommand — must always ride along.
        assert!(s.contains("\"GroupId\":\"g-1\""), "{s}");
        assert!(s.contains("\"When\":\"2026-07-10T12:00:00.123Z\""), "{s}");
        assert!(
            s.contains("\"EmittedAt\":\"2026-07-10T11:59:59.900Z\""),
            "{s}"
        );
        assert!(s.contains("\"PlaylistItemId\":\"pli-1\""), "{s}");
        assert!(s.contains("\"PositionTicks\":50000"), "{s}");
    }

    #[test]
    fn play_queue_update_serializes_pascalcase() {
        let u = PlayQueueUpdate {
            reason: "NewPlaylist".into(),
            last_update: "2026-07-10T12:00:00.000Z".into(),
            playlist: vec![QueuePlaylistItem {
                item_id: "ep1".into(),
                playlist_item_id: "pli-1".into(),
            }],
            playing_item_index: 0,
            start_position_ticks: 0,
            is_playing: true,
            shuffle_mode: "Sorted".into(),
            repeat_mode: "RepeatNone".into(),
        };
        let s = serde_json::to_string(&u).unwrap();
        assert!(s.contains("\"Reason\":\"NewPlaylist\""), "{s}");
        assert!(s.contains("\"Playlist\":[{\"ItemId\":\"ep1\""), "{s}");
        assert!(s.contains("\"PlaylistItemId\":\"pli-1\""), "{s}");
        assert!(s.contains("\"PlayingItemIndex\":0"), "{s}");
        assert!(s.contains("\"IsPlaying\":true"), "{s}");
    }

    #[test]
    fn group_update_data_omits_null_data() {
        // GroupJoined carries a GroupInfoDto; UserJoined carries a bare string.
        let joined = GroupUpdateData {
            kind: "StateUpdate",
            group_id: "g1".into(),
            data: serde_json::to_value(GroupStateUpdate {
                state: "Playing",
                reason: "ready".into(),
            })
            .unwrap(),
        };
        let s = serde_json::to_string(&joined).unwrap();
        assert!(s.contains("\"Type\":\"StateUpdate\""), "{s}");
        assert!(s.contains("\"State\":\"Playing\""), "{s}");
    }
}
