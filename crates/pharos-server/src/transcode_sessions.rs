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
//! The actor loop does no store I/O, so a slow store cannot stall `get`.

use crate::api::jellyfin::device_profile::Decision;
use pharos_core::{
    MediaId, MediaProbe, PersistedTranscodeSession, SessionHolder, TranscodeSessionStore,
};
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
    /// ABSOLUTE subtitle stream indices that must be BURNED for this client,
    /// resolved at PlaybackInfo from the SubtitleProfiles it declared.
    ///
    /// The client's profile is only ever visible on that one request, and the
    /// segment URLs it later fetches carry a bare `SubtitleStreamIndex` with no
    /// indication of whether the client can render that track itself. Without
    /// this set the segment handler had to assume every requested subtitle
    /// needed burning — so a browser that renders `subrip` natively got a
    /// filter graph on every segment anyway.
    pub burn_subtitle_indices: std::collections::BTreeSet<u32>,
    /// Who negotiated the session, and with which token. The PlaySessionId
    /// authorizes a stream on its own (B75), so each such use checks that
    /// this token is still live and this user may still see the item.
    pub holder: SessionHolder,
}

/// Drop in-memory sessions idle for longer than this. Five minutes covers a
/// reasonable network-blip reconnect window without holding stale state
/// forever — a longer pause re-hydrates from the store.
const EXPIRY: Duration = Duration::from_secs(5 * 60);

/// Durable rows outlive the in-memory cache generously: a paused or
/// failed-over stream can resume well past the 5-minute window, and `get`
/// does not refresh `updated_at`, so the row ages from negotiation time.
/// Six hours comfortably covers a long film plus a pause. It is also the
/// capability's absolute limit: `get` and `get_stored` ignore an older row
/// rather than waiting for the hourly prune, and a capability is always
/// checked against the row (B256).
const DB_RETENTION_SECS: i64 = 6 * 60 * 60;

/// Most live play sessions one user may hold on this replica. Minting one more
/// ends that user's oldest sessions first: a forgotten tab must not block a new
/// playback, and a loop of negotiates must not grow the registry or the durable
/// table without bound.
pub const MAX_LIVE_PLAY_SESSIONS_PER_USER: usize = 32;

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
    /// A newly negotiated session: the actor enforces the per-user cap.
    Mint {
        payload: Box<InsertPayload>,
        reply: oneshot::Sender<Minted>,
    },
    /// A session read back from the store on a local miss. Its user was
    /// already counted where it was minted, so no cap applies here.
    Recache(Box<InsertPayload>),
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

/// What the actor did to make room for a minted session.
struct Minted {
    /// Live sessions the user held before this one was added.
    held: usize,
    /// Session ids ended from memory to stay within the cap, oldest first.
    evicted: Vec<String>,
}

async fn delete_durable_row<S: TranscodeSessionStore>(store: &S, session_id: &str) {
    if let Err(e) = store.remove_transcode_session(session_id).await {
        tracing::warn!(
            psid = %pharos_core::secret_prefix(session_id),
            error = %e,
            "transcode session store delete failed"
        );
    }
}

struct Entry {
    session: TranscodeSession,
    last_used: Instant,
    /// Mint order, so "oldest" means first minted, not least recently used.
    minted: u64,
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
            let mut sessions: HashMap<String, Entry> = HashMap::new();
            let mut next_minted: u64 = 0;
            while let Some(msg) = rx.recv().await {
                // Garbage-collect expired sessions on every touch.
                let now = Instant::now();
                sessions.retain(|_, e| now.duration_since(e.last_used) < EXPIRY);
                match msg {
                    Msg::Mint { payload, reply } => {
                        let InsertPayload {
                            session_id,
                            session,
                        } = *payload;
                        // Count, evict and insert in one actor step, so two
                        // concurrent mints cannot both see room and overshoot.
                        let user = session.holder.user_id;
                        let mut others: Vec<(u64, String)> = sessions
                            .iter()
                            .filter(|(id, e)| {
                                **id != session_id && e.session.holder.user_id == user
                            })
                            .map(|(id, e)| (e.minted, id.clone()))
                            .collect();
                        others.sort_unstable();
                        let held = others.len();
                        let excess =
                            held.saturating_sub(MAX_LIVE_PLAY_SESSIONS_PER_USER.saturating_sub(1));
                        let evicted: Vec<String> =
                            others.into_iter().take(excess).map(|(_, id)| id).collect();
                        for id in &evicted {
                            sessions.remove(id);
                        }
                        sessions.insert(
                            session_id,
                            Entry {
                                session,
                                last_used: now,
                                minted: next_minted,
                            },
                        );
                        next_minted = next_minted.wrapping_add(1);
                        let _ = reply.send(Minted { held, evicted });
                    }
                    Msg::Recache(payload) => {
                        let InsertPayload {
                            session_id,
                            session,
                        } = *payload;
                        sessions.insert(
                            session_id,
                            Entry {
                                session,
                                last_used: now,
                                minted: next_minted,
                            },
                        );
                        next_minted = next_minted.wrapping_add(1);
                    }
                    Msg::Get { session_id, reply } => {
                        let s = sessions.get(&session_id).map(|e| e.session.clone());
                        // Touch last-used on get so an active stream
                        // doesn't expire during a long pause.
                        if let Some(entry) = sessions.get_mut(&session_id) {
                            entry.last_used = now;
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
            burn_subtitle_indices_json: serde_json::to_string(&session.burn_subtitle_indices)?,
            holder: Some(session.holder.clone()),
        })
    }

    /// `None` when the row has no holder: it was written before sessions
    /// recorded one, and a session nobody holds authorizes nothing.
    fn from_persisted(
        p: &PersistedTranscodeSession,
    ) -> Result<Option<TranscodeSession>, serde_json::Error> {
        let Some(holder) = p.holder.clone() else {
            return Ok(None);
        };
        Ok(Some(TranscodeSession {
            media_id: p.media_id,
            decision: serde_json::from_str(&p.decision_json)?,
            source_probe: serde_json::from_str(&p.source_probe_json)?,
            burn_subtitle_indices: serde_json::from_str(&p.burn_subtitle_indices_json)?,
            holder,
        }))
    }

    /// Cache the session locally and write it through to the store so other
    /// replicas can fail over to it. Store failures are logged, not fatal.
    ///
    /// Enforces [`MAX_LIVE_PLAY_SESSIONS_PER_USER`] for the session's holder
    /// twice over. In memory, the actor ends the oldest of that user's
    /// sessions on this replica. In the durable store, the holder's live rows
    /// are trimmed across every replica (B279), since a row keeps authorizing
    /// after its memory entry expires. Ending either takes the same store path
    /// as [`Self::remove`]. The ids ended are returned so the caller can stop
    /// what else they drive.
    ///
    /// All store I/O runs here, on the caller's task: the actor also serves
    /// every `get`, so a slow store must never be awaited inside it.
    ///
    /// Without the actor to serialize them, racing mints may each trim. That is
    /// safe: the trim keeps the newest `keep` rows plus the caller's own, so
    /// the end state is at most the cap, and a session ended by both the
    /// in-memory eviction and a trim is returned once. The trim orders by
    /// `updated_at` (unix seconds) then id, while eviction uses mint order, so
    /// a user minting more than the cap within one second can lose a different
    /// session from the durable store than from memory.
    pub async fn insert(
        &self,
        session_id: String,
        session: TranscodeSession,
    ) -> Result<Vec<String>, TranscodeSessionError> {
        let user = session.holder.user_id;
        match Self::to_persisted(&session) {
            Ok(persisted) => {
                if let Err(e) = self
                    .store
                    .upsert_transcode_session(&session_id, &persisted, now_unix_secs())
                    .await
                {
                    tracing::warn!(
                        psid = %pharos_core::secret_prefix(&session_id),
                        error = %e,
                        "transcode session write-through failed; failover unavailable for this session"
                    );
                }
            }
            Err(e) => tracing::warn!(
                psid = %pharos_core::secret_prefix(&session_id),
                error = %e,
                "transcode session serialize failed; not persisted"
            ),
        }
        let minted_id = session_id.clone();
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Mint {
                payload: Box::new(InsertPayload {
                    session_id,
                    session,
                }),
                reply,
            })
            .await
            .map_err(|_| TranscodeSessionError::ActorDown)?;
        let minted = rx.await.map_err(|_| TranscodeSessionError::ReplyDropped)?;
        if !minted.evicted.is_empty() {
            for id in &minted.evicted {
                self.delete_durable(id).await;
            }
            metrics::counter!("pharos_play_sessions_evicted_total")
                .increment(minted.evicted.len() as u64);
            tracing::info!(
                user.id = %user,
                evicted = minted.evicted.len(),
                held = minted.held,
                "play session cap: ended oldest"
            );
        }
        let trimmed = match self
            .store
            .trim_transcode_sessions_for_holder(
                user,
                &minted_id,
                MAX_LIVE_PLAY_SESSIONS_PER_USER,
                now_unix_secs() - DB_RETENTION_SECS,
            )
            .await
        {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!(
                    user.id = %user,
                    error = %e,
                    "durable play session cap: trim failed; other replicas may hold extra sessions"
                );
                Vec::new()
            }
        };
        if !trimmed.is_empty() {
            for id in &trimmed {
                // Their rows are gone; drop any memory entry so it stops authorizing.
                self.tx
                    .send(Msg::Remove {
                        session_id: id.clone(),
                    })
                    .await
                    .map_err(|_| TranscodeSessionError::ActorDown)?;
            }
            metrics::counter!("pharos_play_sessions_trimmed_total").increment(trimmed.len() as u64);
            tracing::info!(
                user.id = %user,
                trimmed = trimmed.len(),
                "durable play session cap: trimmed oldest"
            );
        }
        let mut ended = minted.evicted;
        for id in trimmed {
            if !ended.contains(&id) {
                ended.push(id);
            }
        }
        Ok(ended)
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

        match self
            .store
            .get_transcode_session(session_id, now_unix_secs() - DB_RETENTION_SECS)
            .await
        {
            Ok(Some(persisted)) => match Self::from_persisted(&persisted) {
                Ok(None) => {
                    tracing::info!(
                        "persisted transcode session has no holder; treating as expired"
                    );
                    Ok(None)
                }
                Ok(Some(session)) => {
                    // Re-cache so subsequent segment GETs hit memory, not the DB.
                    let _ = self
                        .tx
                        .send(Msg::Recache(Box::new(InsertPayload {
                            session_id: session_id.to_string(),
                            session: session.clone(),
                        })))
                        .await;
                    Ok(Some(session))
                }
                Err(e) => {
                    tracing::warn!(
                        psid = %pharos_core::secret_prefix(session_id),
                        error = %e,
                        "persisted transcode session failed to decode; ignoring"
                    );
                    Ok(None)
                }
            },
            Ok(None) => Ok(None),
            Err(e) => {
                tracing::warn!(
                    psid = %pharos_core::secret_prefix(session_id),
                    error = %e,
                    "transcode session store lookup failed; treating as expired"
                );
                Ok(None)
            }
        }
    }

    /// The session as the store holds it, skipping this replica's cache. A
    /// stream capability is checked here: a stop on any replica deletes the
    /// row, and the cutoff applies however recently the cache was touched, so
    /// a PlaySessionId polled every few minutes cannot outlive either (B256).
    pub async fn get_stored(
        &self,
        session_id: &str,
    ) -> Result<Option<TranscodeSession>, pharos_core::DomainError> {
        let Some(persisted) = self
            .store
            .get_transcode_session(session_id, now_unix_secs() - DB_RETENTION_SECS)
            .await?
        else {
            return Ok(None);
        };
        match Self::from_persisted(&persisted) {
            Ok(session) => Ok(session),
            Err(e) => {
                tracing::warn!(
                    psid = %pharos_core::secret_prefix(session_id),
                    error = %e,
                    "persisted transcode session failed to decode; ignoring"
                );
                Ok(None)
            }
        }
    }

    async fn delete_durable(&self, session_id: &str) {
        delete_durable_row(&self.store, session_id).await;
    }

    pub async fn remove(&self, session_id: &str) -> Result<(), TranscodeSessionError> {
        self.delete_durable(session_id).await;
        self.tx
            .send(Msg::Remove {
                session_id: session_id.to_string(),
            })
            .await
            .map_err(|_| TranscodeSessionError::ActorDown)
    }

    /// End `session_id` if `user` holds it. Answers whether it was ended: a
    /// session someone else holds, or none at all, is left alone, so knowing
    /// another user's PlaySessionId is not enough to stop their playback.
    pub async fn end_held(
        &self,
        session_id: &str,
        user: pharos_core::UserId,
    ) -> Result<bool, TranscodeSessionError> {
        match self.get(session_id).await? {
            Some(session) if session.holder.user_id == user => {
                self.remove(session_id).await?;
                Ok(true)
            }
            _ => Ok(false),
        }
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
            burn_subtitle_indices: Default::default(),
            holder: pharos_core::SessionHolder {
                user_id: pharos_core::UserId::new(),
                token_id: "test-token-id".into(),
            },
        }
    }

    /// B256: a row written before sessions recorded a holder authorizes
    /// nothing; it reads as expired.
    #[tokio::test]
    async fn a_stored_session_without_a_holder_is_expired() {
        let store = store().await;
        let mut row =
            TranscodeSessionRegistry::<SqliteStore>::to_persisted(&sample_session()).unwrap();
        row.holder = None;
        store
            .upsert_transcode_session("old", &row, now_unix_secs())
            .await
            .unwrap();
        let r = TranscodeSessionRegistry::spawn(store);
        assert!(r.get("old").await.unwrap().is_none());
    }

    /// B256: a stored session past `DB_RETENTION_SECS` authorizes nothing,
    /// even before the hourly prune removes it.
    /// Disarm check: with `get` passing 0 as the cutoff, the stale row
    /// resolves.
    #[tokio::test]
    async fn a_stored_session_past_retention_is_expired() {
        let store = store().await;
        let row = TranscodeSessionRegistry::<SqliteStore>::to_persisted(&sample_session()).unwrap();
        let written = now_unix_secs() - DB_RETENTION_SECS;
        store
            .upsert_transcode_session("stale", &row, written - 1)
            .await
            .unwrap();
        store
            .upsert_transcode_session("fresh", &row, written + 60)
            .await
            .unwrap();
        let r = TranscodeSessionRegistry::spawn(store);
        assert!(r.get("stale").await.unwrap().is_none());
        assert!(r.get("fresh").await.unwrap().is_some());
    }

    /// B256: a stop on one replica ends the capability on another that still
    /// has the session cached, and a cached session past retention is no
    /// capability either.
    #[tokio::test]
    async fn the_stored_row_is_the_capability() {
        let shared = store().await;
        let a = TranscodeSessionRegistry::spawn(shared.clone());
        let b = TranscodeSessionRegistry::spawn(shared.clone());
        let session = sample_session();
        let holder = session.holder.user_id;
        a.insert("s".into(), session.clone()).await.unwrap();
        assert!(a.get_stored("s").await.unwrap().is_some());
        assert!(b.end_held("s", holder).await.unwrap());
        assert!(
            a.get("s").await.unwrap().is_some(),
            "a's cache is untouched"
        );
        assert!(a.get_stored("s").await.unwrap().is_none());

        a.insert("old".into(), session).await.unwrap();
        let row = shared
            .get_transcode_session("old", 0)
            .await
            .unwrap()
            .unwrap();
        shared
            .upsert_transcode_session("old", &row, now_unix_secs() - DB_RETENTION_SECS - 1)
            .await
            .unwrap();
        assert!(a.get("old").await.unwrap().is_some(), "still cached");
        assert!(a.get_stored("old").await.unwrap().is_none());
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

    fn session_for(user: pharos_core::UserId) -> TranscodeSession {
        let mut s = sample_session();
        s.holder.user_id = user;
        s
    }

    async fn live(r: &TranscodeSessionRegistry<SqliteStore>, ids: &[String]) -> usize {
        let mut n = 0;
        for id in ids {
            if r.get_stored(id).await.unwrap().is_some() {
                n += 1;
            }
        }
        n
    }

    /// Minting one session past the cap ends the user's first-minted session:
    /// its capability stops resolving, the rest stay live, and another user's
    /// session is untouched.
    #[tokio::test]
    async fn minting_past_the_cap_ends_the_users_oldest_session() {
        let r = TranscodeSessionRegistry::spawn(store().await);
        let alice = pharos_core::UserId::new();
        let bob = pharos_core::UserId::new();
        r.insert("bob-0".into(), session_for(bob)).await.unwrap();

        let cap = MAX_LIVE_PLAY_SESSIONS_PER_USER;
        let mut alice_ids = Vec::new();
        for i in 0..cap {
            let id = format!("alice-{i}");
            assert!(r
                .insert(id.clone(), session_for(alice))
                .await
                .unwrap()
                .is_empty());
            alice_ids.push(id);
        }
        let evicted = r
            .insert(format!("alice-{cap}"), session_for(alice))
            .await
            .unwrap();
        assert_eq!(
            evicted,
            vec!["alice-0".to_string()],
            "the first minted is ended"
        );
        alice_ids.push(format!("alice-{cap}"));

        assert!(
            r.get("alice-0").await.unwrap().is_none(),
            "cache dropped it"
        );
        assert!(
            r.get_stored("alice-0").await.unwrap().is_none(),
            "store row deleted"
        );
        assert_eq!(
            live(&r, &alice_ids).await,
            cap,
            "exactly the cap stays live"
        );
        assert!(
            r.get_stored("bob-0").await.unwrap().is_some(),
            "other user untouched"
        );
    }

    /// Racing mints for one user each trim outside the actor, so exact eviction
    /// counts are not defined. The bounds are: never more than the cap live in
    /// the store, and no more than the cap still authorizing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_mints_never_leave_more_than_the_cap_live() {
        let r = TranscodeSessionRegistry::spawn(store().await);
        let alice = pharos_core::UserId::new();
        let total = MAX_LIVE_PLAY_SESSIONS_PER_USER + 8;
        let mut tasks = Vec::new();
        let mut ids = Vec::new();
        for i in 0..total {
            let id = format!("race-{i}");
            ids.push(id.clone());
            let r = r.clone();
            tasks.push(tokio::spawn(async move {
                r.insert(id, session_for(alice)).await.unwrap()
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert!(
            live(&r, &ids).await <= MAX_LIVE_PLAY_SESSIONS_PER_USER,
            "durable rows past the cap"
        );
        let mut cached = 0;
        for id in &ids {
            if r.get(id).await.unwrap().is_some() {
                cached += 1;
            }
        }
        assert!(
            cached <= MAX_LIVE_PLAY_SESSIONS_PER_USER,
            "{cached} sessions still authorize"
        );
    }

    /// A store that parks on a gate that is never opened, to show a hung store
    /// cannot stall the registry actor (and so every `get` on the replica).
    #[derive(Clone)]
    struct GatedStore {
        inner: SqliteStore,
        armed: std::sync::Arc<std::sync::atomic::AtomicBool>,
        never: std::sync::Arc<tokio::sync::Notify>,
    }

    impl GatedStore {
        async fn park(&self) {
            if self.armed.load(std::sync::atomic::Ordering::SeqCst) {
                self.never.notified().await;
            }
        }
    }

    impl TranscodeSessionStore for GatedStore {
        async fn upsert_transcode_session(
            &self,
            play_session_id: &str,
            session: &PersistedTranscodeSession,
            now_unix_secs: i64,
        ) -> pharos_core::DomainResult<()> {
            self.inner
                .upsert_transcode_session(play_session_id, session, now_unix_secs)
                .await
        }
        async fn get_transcode_session(
            &self,
            play_session_id: &str,
            updated_since_unix_secs: i64,
        ) -> pharos_core::DomainResult<Option<PersistedTranscodeSession>> {
            self.inner
                .get_transcode_session(play_session_id, updated_since_unix_secs)
                .await
        }
        async fn remove_transcode_session(
            &self,
            play_session_id: &str,
        ) -> pharos_core::DomainResult<()> {
            self.park().await;
            self.inner.remove_transcode_session(play_session_id).await
        }
        async fn prune_transcode_sessions(
            &self,
            cutoff_unix_secs: i64,
        ) -> pharos_core::DomainResult<u64> {
            self.inner.prune_transcode_sessions(cutoff_unix_secs).await
        }
        async fn trim_transcode_sessions_for_holder(
            &self,
            holder: pharos_core::UserId,
            except_play_session_id: &str,
            keep: usize,
            updated_since_unix_secs: i64,
        ) -> pharos_core::DomainResult<Vec<String>> {
            self.park().await;
            self.inner
                .trim_transcode_sessions_for_holder(
                    holder,
                    except_play_session_id,
                    keep,
                    updated_since_unix_secs,
                )
                .await
        }
    }

    /// The actor also serves every `get`, so a mint stuck on a hung store must
    /// leave an already-cached session resolving at once.
    #[tokio::test]
    async fn a_hung_store_during_mint_does_not_block_get() {
        let gated = GatedStore {
            inner: store().await,
            armed: Default::default(),
            never: Default::default(),
        };
        let r = TranscodeSessionRegistry::spawn(gated.clone());
        let alice = pharos_core::UserId::new();
        r.insert("cached".into(), session_for(alice)).await.unwrap();

        gated.armed.store(true, std::sync::atomic::Ordering::SeqCst);
        let stuck = {
            let r = r.clone();
            tokio::spawn(async move { r.insert("stuck".into(), session_for(alice)).await })
        };
        // Let the mint reach the parked trim.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !stuck.is_finished(),
            "the mint should be parked on the trim"
        );

        let got = tokio::time::timeout(Duration::from_secs(1), r.get("cached"))
            .await
            .unwrap_or_else(|_| {
                panic!("get stalled behind a hung store: the actor is doing store I/O")
            })
            .unwrap();
        assert!(got.is_some());
        stuck.abort();
    }

    /// The eviction is counted once and logged with the user, the number
    /// ended and the number held before the mint.
    #[tokio::test]
    async fn eviction_is_counted_and_logged() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
            type Writer = Buf;
            fn make_writer(&'a self) -> Buf {
                self.clone()
            }
        }

        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _metrics = metrics::set_default_local_recorder(&recorder);
        let buf = Buf::default();
        let sub = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .finish();
        let _log = tracing::subscriber::set_default(sub);

        let r = TranscodeSessionRegistry::spawn(store().await);
        let alice = pharos_core::UserId::new();
        for i in 0..=MAX_LIVE_PLAY_SESSIONS_PER_USER {
            r.insert(format!("log-{i}"), session_for(alice))
                .await
                .unwrap();
        }

        let count = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .find_map(|(ck, _, _, v)| {
                (ck.key().name() == "pharos_play_sessions_evicted_total").then_some(v)
            })
            .unwrap();
        assert!(
            matches!(count, DebugValue::Counter(1)),
            "one eviction counted — got {count:?}"
        );
        let log = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        let line = log
            .lines()
            .find(|l| l.contains("play session cap: ended oldest"))
            .unwrap_or_else(|| panic!("no cap log line in {log}"));
        for want in ["evicted=1", "held=32"] {
            assert!(line.contains(want), "missing {want} in {line}");
        }
        assert!(
            !log.contains("log-0"),
            "a session id must not reach the log: {log}"
        );
    }

    /// B279: the durable rows count too. Another replica's rows keep
    /// authorizing after their memory expired or was never here, so a mint
    /// here trims the holder's oldest durable rows, including one this replica
    /// still holds in memory, which must drop with it. A trimmed row is counted
    /// and logged by id-free fields only.
    #[tokio::test]
    async fn durable_rows_past_the_cap_are_trimmed_on_mint() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
            type Writer = Buf;
            fn make_writer(&'a self) -> Buf {
                self.clone()
            }
        }

        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _metrics = metrics::set_default_local_recorder(&recorder);
        let buf = Buf::default();
        let sub = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .finish();
        let _log = tracing::subscriber::set_default(sub);

        let store = store().await;
        let alice = pharos_core::UserId::new();
        let r = TranscodeSessionRegistry::spawn(store.clone());
        // Oldest: minted here, so it is also in this replica's memory.
        r.insert("mem-old".into(), session_for(alice))
            .await
            .unwrap();

        // Newer rows minted elsewhere, with timestamps after `mem-old`'s so the
        // trim order does not depend on how many seconds the test takes.
        let now = now_unix_secs();
        let row =
            TranscodeSessionRegistry::<SqliteStore>::to_persisted(&session_for(alice)).unwrap();
        let mut seeds = Vec::new();
        for i in 0..MAX_LIVE_PLAY_SESSIONS_PER_USER {
            let id = format!("seed-{i:02}");
            let ts = now
                .saturating_add(100)
                .saturating_add(i64::try_from(i).unwrap_or(0));
            store.upsert_transcode_session(&id, &row, ts).await.unwrap();
            seeds.push(id);
        }

        let evicted = r
            .insert("mint-new".into(), session_for(alice))
            .await
            .unwrap();
        assert_eq!(
            evicted,
            vec!["mem-old".to_string(), "seed-00".to_string()],
            "the two oldest durable rows are ended, oldest first"
        );
        assert!(
            r.get("mem-old").await.unwrap().is_none(),
            "a trimmed session must leave this replica's memory too"
        );
        assert!(r.get_stored("seed-00").await.unwrap().is_none());
        assert!(r.get_stored("seed-01").await.unwrap().is_some());
        assert!(r.get_stored("mint-new").await.unwrap().is_some());

        let mut ids = vec!["mem-old".to_string(), "mint-new".to_string()];
        ids.extend(seeds.iter().cloned());
        assert_eq!(
            live(&r, &ids).await,
            MAX_LIVE_PLAY_SESSIONS_PER_USER,
            "the user keeps exactly the cap of authorizing rows"
        );

        let count = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .find_map(|(ck, _, _, v)| {
                (ck.key().name() == "pharos_play_sessions_trimmed_total").then_some(v)
            })
            .unwrap();
        assert!(
            matches!(count, DebugValue::Counter(2)),
            "two trims counted — got {count:?}"
        );
        let log = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        let line = log
            .lines()
            .find(|l| l.contains("durable play session cap: trimmed oldest"))
            .unwrap_or_else(|| panic!("no durable cap log line in {log}"));
        assert!(line.contains("trimmed=2"), "missing trimmed=2 in {line}");
        for id in ["mem-old", "seed-00", "mint-new"] {
            assert!(
                !log.contains(id),
                "a session id must not reach the log: {log}"
            );
        }
    }
}
