//! Per-play-session transcoder negotiation cache (actor — V18) with a
//! store-backed write-through for cross-replica failover (Phase B1).
//!
//! `playback_info` runs the device-profile negotiator and returns a
//! `Decision`. For transcoded playback, the same `Decision` is needed
//! again later when the HLS master playlist + segments are generated
//! — otherwise the segment handler hard-codes H.264 + AAC + TS even
//! when the negotiator picked, say, HEVC + Opus.
//!
//! We stash the `Decision` (plus the source MediaProbe + media_id) in
//! a tokio-task-owned registry keyed on PlaySessionId. The in-memory map
//! is the hot cache; every mutation also writes through to a
//! [`TranscodeSessionStore`] so a *different* replica — one that never ran
//! this session's `playback_info` — can still resolve it and serve the next
//! segment instead of 410-ing the viewer. On a local cache miss `get`
//! falls back to the store and re-caches. In-memory sessions expire after
//! `EXPIRY` of inactivity; the durable rows are pruned only after the far
//! more generous `DB_RETENTION_SECS`.
//!
//! Store write-through is best-effort: a DB hiccup is logged (with the
//! underlying cause) and playback continues on the local replica — the only
//! thing lost is failover for that one session, never the stream itself.

use crate::api::jellyfin::device_profile::Decision;
use pharos_core::{MediaId, MediaProbe, PersistedTranscodeSession, TranscodeSessionStore};
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

/// Drop in-memory sessions idle for longer than this. Five minutes covers a
/// reasonable network-blip reconnect window without holding stale state
/// forever — a longer pause re-hydrates from the store.
const EXPIRY: Duration = Duration::from_secs(5 * 60);

/// Durable rows outlive the in-memory cache generously: a paused or
/// failed-over stream can resume well past the 5-minute window, and `get`
/// does not refresh `updated_at`, so the row ages from negotiation time.
/// Six hours comfortably covers a long film plus a pause.
const DB_RETENTION_SECS: i64 = 6 * 60 * 60;

fn now_unix_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

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
pub struct TranscodeSessionRegistry<S> {
    tx: mpsc::Sender<Msg>,
    store: S,
}

impl<S> TranscodeSessionRegistry<S>
where
    S: TranscodeSessionStore + Clone + Send + Sync + 'static,
{
    pub fn spawn(store: S) -> Self {
        let (tx, mut rx) = mpsc::channel::<Msg>(64);
        tokio::spawn(async move {
            let mut sessions: HashMap<String, (TranscodeSession, Instant)> = HashMap::new();
            while let Some(msg) = rx.recv().await {
                // Garbage-collect expired sessions on every touch.
                let now = Instant::now();
                sessions.retain(|_, (_, last)| now.duration_since(*last) < EXPIRY);
                match msg {
                    Msg::Insert(payload) => {
                        let InsertPayload {
                            session_id,
                            session,
                        } = *payload;
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
        Self { tx, store }
    }

    fn to_persisted(
        session: &TranscodeSession,
    ) -> Result<PersistedTranscodeSession, serde_json::Error> {
        Ok(PersistedTranscodeSession {
            media_id: session.media_id,
            decision_json: serde_json::to_string(&session.decision)?,
            source_probe_json: serde_json::to_string(&session.source_probe)?,
        })
    }

    fn from_persisted(
        p: &PersistedTranscodeSession,
    ) -> Result<TranscodeSession, serde_json::Error> {
        Ok(TranscodeSession {
            media_id: p.media_id,
            decision: serde_json::from_str(&p.decision_json)?,
            source_probe: serde_json::from_str(&p.source_probe_json)?,
        })
    }

    /// Cache the session locally and write it through to the store so other
    /// replicas can fail over to it. Store failures are logged, not fatal.
    pub async fn insert(
        &self,
        session_id: String,
        session: TranscodeSession,
    ) -> Result<(), TranscodeSessionError> {
        match Self::to_persisted(&session) {
            Ok(persisted) => {
                if let Err(e) = self
                    .store
                    .upsert_transcode_session(&session_id, &persisted, now_unix_secs())
                    .await
                {
                    tracing::warn!(
                        psid = %session_id,
                        error = %e,
                        "transcode session write-through failed; failover unavailable for this session"
                    );
                }
            }
            Err(e) => tracing::warn!(
                psid = %session_id,
                error = %e,
                "transcode session serialize failed; not persisted"
            ),
        }
        self.tx
            .send(Msg::Insert(Box::new(InsertPayload {
                session_id,
                session,
            })))
            .await
            .map_err(|_| TranscodeSessionError::ActorDown)
    }

    /// Local cache first; on a miss fall back to the store (the failover /
    /// cold-replica path) and re-cache. A store error or an undecodable row
    /// resolves to `None` (the caller 410s, exactly as before), with the
    /// underlying cause logged.
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
        let local = rx.await.map_err(|_| TranscodeSessionError::ReplyDropped)?;
        if local.is_some() {
            return Ok(local);
        }

        match self.store.get_transcode_session(session_id).await {
            Ok(Some(persisted)) => match Self::from_persisted(&persisted) {
                Ok(session) => {
                    // Re-cache so subsequent segment GETs hit memory, not the DB.
                    let _ = self
                        .tx
                        .send(Msg::Insert(Box::new(InsertPayload {
                            session_id: session_id.to_string(),
                            session: session.clone(),
                        })))
                        .await;
                    Ok(Some(session))
                }
                Err(e) => {
                    tracing::warn!(
                        psid = %session_id,
                        error = %e,
                        "persisted transcode session failed to decode; ignoring"
                    );
                    Ok(None)
                }
            },
            Ok(None) => Ok(None),
            Err(e) => {
                tracing::warn!(
                    psid = %session_id,
                    error = %e,
                    "transcode session store lookup failed; treating as expired"
                );
                Ok(None)
            }
        }
    }

    pub async fn remove(&self, session_id: &str) -> Result<(), TranscodeSessionError> {
        if let Err(e) = self.store.remove_transcode_session(session_id).await {
            tracing::warn!(
                psid = %session_id,
                error = %e,
                "transcode session store delete failed"
            );
        }
        self.tx
            .send(Msg::Remove {
                session_id: session_id.to_string(),
            })
            .await
            .map_err(|_| TranscodeSessionError::ActorDown)
    }

    /// Drop durable rows idle past `DB_RETENTION_SECS`. Idempotent + cheap;
    /// safe to run on every replica. Returns rows removed.
    pub async fn prune_store(&self) -> u64 {
        let cutoff = now_unix_secs() - DB_RETENTION_SECS;
        match self.store.prune_transcode_sessions(cutoff).await {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(error = %e, "transcode session store prune failed");
                0
            }
        }
    }

    /// Spawn a background task that prunes stale durable rows hourly.
    pub fn spawn_pruner(&self) {
        let this = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60 * 60));
            loop {
                tick.tick().await;
                this.prune_store().await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::api::jellyfin::device_profile::Decision;
    use pharos_store_sqlx::sqlite::SqliteStore;

    async fn store() -> SqliteStore {
        SqliteStore::connect("sqlite::memory:").await.unwrap()
    }

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
        let r = TranscodeSessionRegistry::spawn(store().await);
        r.insert("s1".into(), sample_session()).await.unwrap();
        let got = r.get("s1").await.unwrap().unwrap();
        assert_eq!(got.media_id, 42);
    }

    #[tokio::test]
    async fn get_missing_returns_none() {
        let r = TranscodeSessionRegistry::spawn(store().await);
        let got = r.get("missing").await.unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn remove_clears_session() {
        let r = TranscodeSessionRegistry::spawn(store().await);
        r.insert("s1".into(), sample_session()).await.unwrap();
        r.remove("s1").await.unwrap();
        let got = r.get("s1").await.unwrap();
        assert!(got.is_none());
    }

    /// The Phase B1 failover: replica B (a fresh registry with an empty
    /// cache) resolves a session replica A negotiated, via the shared store.
    #[tokio::test]
    async fn get_falls_back_to_store_across_registries() {
        let shared = store().await;
        let replica_a = TranscodeSessionRegistry::spawn(shared.clone());
        let replica_b = TranscodeSessionRegistry::spawn(shared.clone());

        replica_a
            .insert("cross".into(), sample_session())
            .await
            .unwrap();

        // B never saw the insert in its local cache, but recovers it from
        // the store and returns the negotiated decision.
        let got = replica_b.get("cross").await.unwrap().unwrap();
        assert_eq!(got.media_id, 42);
        match got.decision {
            Decision::Transcode {
                target_video_codec, ..
            } => assert_eq!(target_video_codec.as_deref(), Some("h264")),
            other => panic!("decision did not round-trip through the store: {other:?}"),
        }
    }

    /// Removing on one replica tombstones the shared row so a failover
    /// lookup on another replica also misses.
    #[tokio::test]
    async fn remove_propagates_through_store() {
        let shared = store().await;
        let replica_a = TranscodeSessionRegistry::spawn(shared.clone());
        let replica_b = TranscodeSessionRegistry::spawn(shared.clone());

        replica_a
            .insert("gone".into(), sample_session())
            .await
            .unwrap();
        replica_a.remove("gone").await.unwrap();
        assert!(replica_b.get("gone").await.unwrap().is_none());
    }
}
