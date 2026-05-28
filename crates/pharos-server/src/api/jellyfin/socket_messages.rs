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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct GroupUpdateData {
    #[serde(rename = "Type")]
    pub kind: &'static str,
    pub group_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct CommandData {
    pub command: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_ticks: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
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
}
