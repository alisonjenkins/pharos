//! `SessionRegistry` actor — owns the set of active playback sessions.
//! V18 — handlers send mpsc messages; no `Mutex` on the request path.

use pharos_core::UserId;
use pharos_jellyfin_api::dto::format_iso8601;
use serde::Serialize;
use tokio::sync::{mpsc, oneshot};

/// Current time as ISO8601 UTC. Stamped on every session event so the
/// serialized `LastActivityDate` / `LastPlaybackCheckIn` are always valid
/// dates — jellyfin-web's dashboard "Active Devices" panel formats them with
/// date-fns, and a missing value yields `new Date(undefined)` → a fatal
/// "Invalid time value" that crashes the whole dashboard landing page.
fn iso_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    format_iso8601(secs)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct SessionRecord {
    pub id: String,
    pub user_id: String,
    pub user_name: String,
    pub device_id: String,
    pub device_name: String,
    pub client: String,
    pub application_version: String,
    pub now_playing_item_id: Option<String>,
    pub position_ticks: u64,
    pub is_paused: bool,
    /// P28 — capabilities the client advertised via
    /// `/Sessions/Capabilities`. Empty Vec when the client never
    /// posted; jellyfin-web's remote-control screen uses this to grey
    /// out unsupported commands.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub playable_media_types: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub supported_commands: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_streaming_bitrate: Option<u64>,
    pub supports_media_control: bool,
    /// ISO8601 UTC, stamped on every event. Never empty — see [`iso_now`].
    pub last_activity_date: String,
    pub last_playback_check_in: String,
}

#[derive(Debug, Clone)]
pub enum SessionEvent {
    Started {
        session_id: String,
        user_id: UserId,
        user_name: String,
        device_id: String,
        device_name: String,
        client: String,
        version: String,
        item_id: String,
        position_ticks: u64,
    },
    Progress {
        session_id: String,
        item_id: String,
        position_ticks: u64,
        is_paused: bool,
    },
    Stopped {
        session_id: String,
    },
    /// P28 — apply a client's advertised capabilities to its
    /// session record. No-op when the session isn't tracked yet
    /// (the next Started event refreshes the capabilities from the
    /// last seen Set).
    SetCapabilities {
        session_id: String,
        playable_media_types: Vec<String>,
        supported_commands: Vec<String>,
        max_streaming_bitrate: Option<u64>,
        supports_media_control: bool,
    },
}

enum Msg {
    Apply(SessionEvent),
    Snapshot(oneshot::Sender<Vec<SessionRecord>>),
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("session actor dropped")]
    ActorDown,
    #[error("session reply dropped")]
    ReplyDropped,
}

#[derive(Clone)]
pub struct SessionRegistry {
    tx: mpsc::Sender<Msg>,
}

impl SessionRegistry {
    pub fn spawn() -> Self {
        let (tx, mut rx) = mpsc::channel::<Msg>(256);
        tokio::spawn(async move {
            let mut state: std::collections::HashMap<String, SessionRecord> =
                std::collections::HashMap::new();
            while let Some(msg) = rx.recv().await {
                match msg {
                    Msg::Apply(SessionEvent::Started {
                        session_id,
                        user_id,
                        user_name,
                        device_id,
                        device_name,
                        client,
                        version,
                        item_id,
                        position_ticks,
                    }) => {
                        // Preserve any capabilities the client
                        // advertised earlier (P28) — Started events
                        // refresh playback state but not caps.
                        let existing_caps = state.get(&session_id).map(|s| {
                            (
                                s.playable_media_types.clone(),
                                s.supported_commands.clone(),
                                s.max_streaming_bitrate,
                                s.supports_media_control,
                            )
                        });
                        state.insert(
                            session_id.clone(),
                            SessionRecord {
                                id: session_id,
                                user_id: user_id.0.simple().to_string(),
                                user_name,
                                device_id,
                                device_name,
                                client,
                                application_version: version,
                                now_playing_item_id: Some(item_id),
                                position_ticks,
                                is_paused: false,
                                playable_media_types: existing_caps
                                    .as_ref()
                                    .map(|c| c.0.clone())
                                    .unwrap_or_default(),
                                supported_commands: existing_caps
                                    .as_ref()
                                    .map(|c| c.1.clone())
                                    .unwrap_or_default(),
                                max_streaming_bitrate: existing_caps.as_ref().and_then(|c| c.2),
                                supports_media_control: existing_caps
                                    .as_ref()
                                    .map(|c| c.3)
                                    .unwrap_or(false),
                                last_activity_date: iso_now(),
                                last_playback_check_in: iso_now(),
                            },
                        );
                    }
                    Msg::Apply(SessionEvent::Progress {
                        session_id,
                        item_id,
                        position_ticks,
                        is_paused,
                    }) => {
                        if let Some(s) = state.get_mut(&session_id) {
                            s.now_playing_item_id = Some(item_id);
                            s.position_ticks = position_ticks;
                            s.is_paused = is_paused;
                            s.last_activity_date = iso_now();
                            s.last_playback_check_in = s.last_activity_date.clone();
                        }
                    }
                    Msg::Apply(SessionEvent::Stopped { session_id }) => {
                        state.remove(&session_id);
                    }
                    Msg::Apply(SessionEvent::SetCapabilities {
                        session_id,
                        playable_media_types,
                        supported_commands,
                        max_streaming_bitrate,
                        supports_media_control,
                    }) => {
                        // Apply when the session already exists. When
                        // it doesn't, stash a stub so subsequent
                        // Started events inherit the caps.
                        let entry =
                            state
                                .entry(session_id.clone())
                                .or_insert_with(|| SessionRecord {
                                    id: session_id,
                                    user_id: String::new(),
                                    user_name: String::new(),
                                    device_id: String::new(),
                                    device_name: String::new(),
                                    client: String::new(),
                                    application_version: String::new(),
                                    now_playing_item_id: None,
                                    position_ticks: 0,
                                    is_paused: false,
                                    playable_media_types: Vec::new(),
                                    supported_commands: Vec::new(),
                                    max_streaming_bitrate: None,
                                    supports_media_control: false,
                                    last_activity_date: iso_now(),
                                    last_playback_check_in: iso_now(),
                                });
                        entry.last_activity_date = iso_now();
                        entry.last_playback_check_in = entry.last_activity_date.clone();
                        entry.playable_media_types = playable_media_types;
                        entry.supported_commands = supported_commands;
                        entry.max_streaming_bitrate = max_streaming_bitrate;
                        entry.supports_media_control = supports_media_control;
                    }
                    Msg::Snapshot(reply) => {
                        let mut all: Vec<SessionRecord> = state.values().cloned().collect();
                        all.sort_by(|a, b| a.id.cmp(&b.id));
                        let _ = reply.send(all);
                    }
                }
            }
        });
        Self { tx }
    }

    pub async fn apply(&self, event: SessionEvent) -> Result<(), SessionError> {
        self.tx
            .send(Msg::Apply(event))
            .await
            .map_err(|_| SessionError::ActorDown)
    }

    pub async fn snapshot(&self) -> Result<Vec<SessionRecord>, SessionError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Msg::Snapshot(tx))
            .await
            .map_err(|_| SessionError::ActorDown)?;
        rx.await.map_err(|_| SessionError::ReplyDropped)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn started(session_id: &str, item: &str, pos: u64) -> SessionEvent {
        SessionEvent::Started {
            session_id: session_id.into(),
            user_id: UserId::new(),
            user_name: "u".into(),
            device_id: "d".into(),
            device_name: "dn".into(),
            client: "c".into(),
            version: "0".into(),
            item_id: item.into(),
            position_ticks: pos,
        }
    }

    #[tokio::test]
    async fn started_then_snapshot_contains_session() {
        let r = SessionRegistry::spawn();
        r.apply(started("s1", "100", 0)).await.unwrap();
        let snap = r.snapshot().await.unwrap();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].now_playing_item_id.as_deref(), Some("100"));
    }

    #[tokio::test]
    async fn progress_updates_position_and_paused() {
        let r = SessionRegistry::spawn();
        r.apply(started("s1", "100", 0)).await.unwrap();
        r.apply(SessionEvent::Progress {
            session_id: "s1".into(),
            item_id: "100".into(),
            position_ticks: 12345,
            is_paused: true,
        })
        .await
        .unwrap();
        let snap = r.snapshot().await.unwrap();
        assert_eq!(snap[0].position_ticks, 12345);
        assert!(snap[0].is_paused);
    }

    #[tokio::test]
    async fn stopped_removes_session() {
        let r = SessionRegistry::spawn();
        r.apply(started("s1", "100", 0)).await.unwrap();
        r.apply(SessionEvent::Stopped {
            session_id: "s1".into(),
        })
        .await
        .unwrap();
        let snap = r.snapshot().await.unwrap();
        assert!(snap.is_empty());
    }
}
