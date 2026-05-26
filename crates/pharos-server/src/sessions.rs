//! `SessionRegistry` actor — owns the set of active playback sessions.
//! V18 — handlers send mpsc messages; no `Mutex` on the request path.

use pharos_core::UserId;
use serde::Serialize;
use tokio::sync::{mpsc, oneshot};

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
                        }
                    }
                    Msg::Apply(SessionEvent::Stopped { session_id }) => {
                        state.remove(&session_id);
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
