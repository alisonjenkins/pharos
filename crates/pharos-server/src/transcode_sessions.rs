//! Per-play-session transcoder negotiation cache (actor — V18).
//!
//! `playback_info` runs the device-profile negotiator and returns a
//! `Decision`. For transcoded playback, the same `Decision` is needed
//! again later when the HLS master playlist + segments are generated
//! — otherwise the segment handler hard-codes H.264 + AAC + TS even
//! when the negotiator picked, say, HEVC + Opus.
//!
//! We stash the `Decision` (plus the source MediaProbe + media_id) in
//! a tokio-task-owned registry keyed on PlaySessionId. Sessions
//! expire after `EXPIRY` of inactivity. Mutation flows through the
//! actor's mpsc channel; readers send a message and await a oneshot
//! reply.

use crate::api::jellyfin::device_profile::Decision;
use pharos_core::{MediaId, MediaProbe};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, thiserror::Error)]
pub enum TranscodeSessionError {
    #[error("registry actor dropped")]
    ActorDown,
    #[error("reply dropped")]
    ReplyDropped,
}

/// What `playback_info` recorded about a session we'll later need to
/// regenerate segments for. The negotiator + the source probe are
/// both saved so the segment handler doesn't have to re-run either.
#[derive(Debug, Clone)]
pub struct TranscodeSession {
    pub media_id: MediaId,
    pub decision: Decision,
    pub source_probe: MediaProbe,
}

/// Drop sessions idle for longer than this. Five minutes covers a
/// reasonable network-blip reconnect window without holding stale
/// state forever.
const EXPIRY: Duration = Duration::from_secs(5 * 60);

// Boxed Insert payload — `TranscodeSession` is large (MediaProbe +
// Decision); the other variants would otherwise inflate every
// channel slot.
enum Msg {
    Insert(Box<InsertPayload>),
    Get {
        session_id: String,
        reply: oneshot::Sender<Option<TranscodeSession>>,
    },
    Remove {
        session_id: String,
    },
}

struct InsertPayload {
    session_id: String,
    session: TranscodeSession,
}

#[derive(Clone)]
pub struct TranscodeSessionRegistry {
    tx: mpsc::Sender<Msg>,
}

impl TranscodeSessionRegistry {
    pub fn spawn() -> Self {
        let (tx, mut rx) = mpsc::channel::<Msg>(64);
        tokio::spawn(async move {
            let mut sessions: HashMap<String, (TranscodeSession, Instant)> = HashMap::new();
            while let Some(msg) = rx.recv().await {
                // Garbage-collect expired sessions on every touch.
                let now = Instant::now();
                sessions.retain(|_, (_, last)| now.duration_since(*last) < EXPIRY);
                match msg {
                    Msg::Insert(payload) => {
                        let InsertPayload { session_id, session } = *payload;
                        sessions.insert(session_id, (session, now));
                    }
                    Msg::Get { session_id, reply } => {
                        let s = sessions.get(&session_id).map(|(s, _)| s.clone());
                        // Touch last-used on get so an active stream
                        // doesn't expire during a long pause.
                        if let Some(entry) = sessions.get_mut(&session_id) {
                            entry.1 = now;
                        }
                        let _ = reply.send(s);
                    }
                    Msg::Remove { session_id } => {
                        sessions.remove(&session_id);
                    }
                }
            }
        });
        Self { tx }
    }

    pub async fn insert(
        &self,
        session_id: String,
        session: TranscodeSession,
    ) -> Result<(), TranscodeSessionError> {
        self.tx
            .send(Msg::Insert(Box::new(InsertPayload {
                session_id,
                session,
            })))
            .await
            .map_err(|_| TranscodeSessionError::ActorDown)
    }

    pub async fn get(
        &self,
        session_id: &str,
    ) -> Result<Option<TranscodeSession>, TranscodeSessionError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Msg::Get {
                session_id: session_id.to_string(),
                reply: tx,
            })
            .await
            .map_err(|_| TranscodeSessionError::ActorDown)?;
        rx.await.map_err(|_| TranscodeSessionError::ReplyDropped)
    }

    pub async fn remove(&self, session_id: &str) -> Result<(), TranscodeSessionError> {
        self.tx
            .send(Msg::Remove {
                session_id: session_id.to_string(),
            })
            .await
            .map_err(|_| TranscodeSessionError::ActorDown)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::api::jellyfin::device_profile::Decision;

    fn sample_session() -> TranscodeSession {
        TranscodeSession {
            media_id: 42,
            decision: Decision::Transcode {
                target_container: "mp4".into(),
                target_video_codec: Some("h264".into()),
                target_audio_codec: Some("aac".into()),
                max_video_bitrate_bps: Some(2_500_000),
            },
            source_probe: MediaProbe::default(),
        }
    }

    #[tokio::test]
    async fn insert_then_get_returns_session() {
        let r = TranscodeSessionRegistry::spawn();
        r.insert("s1".into(), sample_session()).await.unwrap();
        let got = r.get("s1").await.unwrap().unwrap();
        assert_eq!(got.media_id, 42);
    }

    #[tokio::test]
    async fn get_missing_returns_none() {
        let r = TranscodeSessionRegistry::spawn();
        let got = r.get("missing").await.unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn remove_clears_session() {
        let r = TranscodeSessionRegistry::spawn();
        r.insert("s1".into(), sample_session()).await.unwrap();
        r.remove("s1").await.unwrap();
        let got = r.get("s1").await.unwrap();
        assert!(got.is_none());
    }
}
