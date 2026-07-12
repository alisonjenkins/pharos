//! Wire types for the extended `/sync/v1/ws` protocol — see
//! `docs/group-sync-protocol.md` §3. Jellyfin-shaped `/socket` messages
//! land in T16 phase 2 via a translation layer; the actor only ever
//! sees `ClientMsg` / `ServerMsg`.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GroupId(pub Uuid);

impl GroupId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for GroupId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for GroupId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MemberId(pub Uuid);

impl MemberId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for MemberId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for MemberId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberSummary {
    pub member_id: MemberId,
    pub name: String,
    pub is_leader: bool,
}

/// Coarse group playback state, mirrored to Jellyfin's `GroupStateType`
/// (`Idle`/`Waiting`/`Playing`/`Paused`). Emitted in [`ServerMsg::StateUpdate`]
/// so a client can drive its SyncPlay UI (spinner while `Waiting`, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupPlayState {
    Idle,
    Waiting,
    Playing,
    Paused,
}

/// One entry in a group's play queue. `playlist_item_id` is the server-assigned,
/// per-entry stable id the Jellyfin client echoes on every command — a
/// `SendCommand` whose `PlaylistItemId` doesn't match the client's current queue
/// item is silently dropped, so this id must stay consistent between the
/// `PlayQueue` update and the following `SendCommand`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueItemInfo {
    pub item_id: String,
    pub playlist_item_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// V8: `token` is plain `String` at the wire boundary so this type can
    /// derive `Deserialize`. Handler must wrap into `SecretString` *before*
    /// any logging/tracing and immediately drop the original. See
    /// `ws::expect_hello`.
    Hello {
        token: String,
        client: String,
        device_id: String,
        name: String,
    },
    Join {
        group_id: GroupId,
    },
    CreateAndJoin,
    Leave,
    Ping {
        client_ms: u64,
    },
    /// NTP step 2: after receiving the `Pong`, the client reports its
    /// receive timestamp (T4) so the server can compute the real
    /// round-trip time. Without this the server only knows T1/T2/T3 and
    /// RTT collapses to 0 (defeating V3 lead-time enforcement). See
    /// `docs/group-sync-protocol.md` §4.
    ClockReport {
        /// Echo of the `Ping.client_ms` (T1) this report corresponds to.
        client_ms: u64,
        /// Client's receive time of the matching `Pong` (T4).
        client_recv_ms: u64,
    },
    LeaderPlay {
        position_ms: u64,
    },
    LeaderPause,
    LeaderSeek {
        position_ms: u64,
    },
    BufferingStart {
        position_ms: u64,
    },
    BufferingEnd {
        position_ms: u64,
    },
    Heartbeat,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Welcome {
        member_id: MemberId,
        server_ms: u64,
    },
    Joined {
        group_id: GroupId,
        leader: MemberId,
        members: Vec<MemberSummary>,
    },
    Pong {
        client_ms_echo: u64,
        server_ms: u64,
    },
    Play {
        at_server_ms: u64,
        position_ms: u64,
    },
    Pause {
        at_server_ms: u64,
        /// The frozen position the group paused at. REQUIRED on the wire:
        /// jellyfin-web's `schedulePause` seeks to the command's
        /// PositionTicks after pausing — a `Pause` without a position makes
        /// the client seek to 0:00 and desync permanently (drift correction
        /// is off by default in 10.11.8).
        position_ms: u64,
    },
    Seek {
        at_server_ms: u64,
        position_ms: u64,
    },
    LeaderChange {
        leader: MemberId,
    },
    MemberJoined {
        member: MemberSummary,
    },
    MemberLeft {
        member_id: MemberId,
    },
    /// Coarse playback-state transition (Jellyfin `SyncPlayGroupUpdate` /
    /// `StateUpdate`). `reason` is a free-form label (e.g. the command that
    /// caused the transition) for diagnostics + the client's UI.
    StateUpdate {
        state: GroupPlayState,
        reason: String,
    },
    /// The group's play queue changed (Jellyfin `SyncPlayGroupUpdate` /
    /// `PlayQueue`). Carries the full playlist so a client (incl. a late
    /// joiner) can render it and load the current item. `is_playing` reflects
    /// whether the group intends to be playing once buffered.
    PlayQueue {
        reason: String,
        items: Vec<QueueItemInfo>,
        playing_index: usize,
        start_position_ms: u64,
        is_playing: bool,
        repeat_mode: String,
        shuffle_mode: String,
        /// Wall-clock (unix ms) of the last real queue CHANGE — NOT the moment
        /// this message was built. jellyfin-web's QueueCore drops a PlayQueue
        /// whose `LastUpdate` is `<=` the one it already applied, so a catch-up
        /// re-send of the same queue MUST carry the same value or the client
        /// re-processes it (restarting playback → "no active player"). Bumped
        /// only when the queue actually changes; reused verbatim on catch-up.
        last_update_unix_ms: u64,
    },
    Error {
        code: ErrorCode,
        detail: String,
    },
    /// The server does not consider this session a member of any group, yet
    /// the client sent a group command (it still believes it's grouped — e.g.
    /// its group was pruned while it was offline). Translated to Jellyfin's
    /// `SyncPlayGroupUpdate`/`NotInGroup`, which stock jellyfin-web handles by
    /// disabling SyncPlay locally — a VISIBLE exit instead of a silent desync
    /// where the sender applies its command locally and nobody else does (B24).
    NotInGroup,
    /// Acknowledge THIS member's departure (Jellyfin `SyncPlayGroupUpdate` /
    /// `GroupLeft`). jellyfin-web only exits SyncPlay mode on receiving this
    /// (or `NotInGroup`) — a `/SyncPlay/Leave` answered with just a 204 leaves
    /// the client wedged in group mode with playback controls hijacked (B25).
    /// `MemberLeft` can't serve here: the leaver is already out of the roster
    /// when it broadcasts, so it only ever reaches the REMAINING members.
    GroupLeft,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    AuthFailed,
    UnknownGroup,
    NotLeader,
    RateLimited,
    Internal,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn hello_token_field_is_plain_string_on_wire() {
        let h = ClientMsg::Hello {
            token: "wire-token".into(),
            client: "test".into(),
            device_id: "d1".into(),
            name: "ali".into(),
        };
        let s = serde_json::to_string(&h).unwrap();
        assert!(s.contains("\"token\":\"wire-token\""), "{s}");
        // Defensive note: handler must wrap into SecretString immediately
        // after deserializing — see ws::expect_hello.
    }

    #[test]
    fn server_play_serializes_snake_case() {
        let m = ServerMsg::Play {
            at_server_ms: 12345,
            position_ms: 100,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("\"type\":\"play\""), "{s}");
        assert!(s.contains("\"at_server_ms\":12345"), "{s}");
        assert!(s.contains("\"position_ms\":100"), "{s}");
    }

    #[test]
    fn client_join_deserialize() {
        let id = Uuid::new_v4();
        let raw = format!(r#"{{"type":"join","group_id":"{id}"}}"#);
        let parsed: ClientMsg = serde_json::from_str(&raw).unwrap();
        match parsed {
            ClientMsg::Join { group_id } => assert_eq!(group_id.0, id),
            other => panic!("expected Join, got {other:?}"),
        }
    }

    #[test]
    fn unknown_type_fails_deserialize() {
        let raw = r#"{"type":"nonsense"}"#;
        let res: Result<ClientMsg, _> = serde_json::from_str(raw);
        assert!(res.is_err());
    }

    #[test]
    fn member_ids_order_lexicographically_for_handoff() {
        // Leader election: lowest MemberId wins (lexicographic UUID).
        let a = MemberId(Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap());
        let b = MemberId(Uuid::parse_str("ffffffff-ffff-ffff-ffff-ffffffffffff").unwrap());
        assert!(a < b);
    }
}
