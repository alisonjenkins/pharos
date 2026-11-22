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
    MediaId, MediaProbe, PersistedTranscodeSession, PlaySessionId, SessionHolder,
    TranscodeSessionStore,
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
    #[error("session store: {0}")]
    Store(String),
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
        session_id: PlaySessionId,
        reply: oneshot::Sender<(PlaySessionId, Option<TranscodeSession>)>,
    },
    Remove {
        session_id: PlaySessionId,
    },
}

struct InsertPayload {
    session_id: PlaySessionId,
    session: TranscodeSession,
}

/// What the actor did to make room for a minted session.
struct Minted {
    /// Live sessions the user held before this one was added.
    held: usize,
    /// Session ids ended from memory to stay within the cap, oldest first.
    evicted: Vec<PlaySessionId>,
}

async fn delete_durable_row<S: TranscodeSessionStore>(store: &S, session_id: &PlaySessionId) {
    if let Err(e) = store.remove_transcode_session(session_id.as_str()).await {
        tracing::warn!(
            psid = %pharos_core::secret_prefix(session_id.as_str()),
            error = %e,
            "transcode session store delete failed"
        );
    }
}

/// What a native grant's stop did ([`TranscodeSessionRegistry::end_granted`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantEnd {
    Ended,
    /// The session is at another grant epoch (a renewal took it over), or
    /// it ended between the check and the delete with no removal at this
    /// grant's epoch remembered. Left as it is.
    Superseded,
    /// Another user holds it.
    NotHeld,
    /// The row is gone, and the store remembers it was removed at this
    /// grant's epoch: by this grant's own stop (whose answer may have been
    /// lost), or by the session cap while it was still this grant's, also
    /// when the cap removed it between this stop's read and its delete.
    EndedAt,
    /// The row is gone with no removal at this grant's epoch remembered: it
    /// was pruned, or it ended after a renewal took it over.
    Gone,
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
            let mut sessions: HashMap<PlaySessionId, Entry> = HashMap::new();
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
                        let mut others: Vec<(u64, PlaySessionId)> = sessions
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
                        let evicted: Vec<PlaySessionId> =
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
                        let _ = reply.send((session_id, s));
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
        session_id: PlaySessionId,
        session: TranscodeSession,
    ) -> Result<Vec<PlaySessionId>, TranscodeSessionError> {
        let user = session.holder.user_id;
        match Self::to_persisted(&session) {
            Ok(persisted) => {
                if let Err(e) = self
                    .store
                    .upsert_transcode_session(session_id.as_str(), &persisted, now_unix_secs())
                    .await
                {
                    tracing::warn!(
                        psid = %pharos_core::secret_prefix(session_id.as_str()),
                        error = %e,
                        "transcode session write-through failed; failover unavailable for this session"
                    );
                }
            }
            Err(e) => tracing::warn!(
                psid = %pharos_core::secret_prefix(session_id.as_str()),
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
                minted_id.as_str(),
                MAX_LIVE_PLAY_SESSIONS_PER_USER,
                now_unix_secs() - DB_RETENTION_SECS,
            )
            .await
        {
            Ok(ids) => ids.into_iter().map(PlaySessionId::from).collect::<Vec<_>>(),
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
    /// underlying cause logged. A cache that cannot answer (its actor has
    /// stopped) is a miss too, so a session another replica's grant moved
    /// here is still served from its row (B1490).
    pub async fn get(
        &self,
        session_id: PlaySessionId,
    ) -> Result<Option<TranscodeSession>, TranscodeSessionError> {
        let (tx, rx) = oneshot::channel();
        let session_id = match self
            .tx
            .send(Msg::Get {
                session_id,
                reply: tx,
            })
            .await
        {
            Ok(()) => {
                let (session_id, local) =
                    rx.await.map_err(|_| TranscodeSessionError::ReplyDropped)?;
                if local.is_some() {
                    return Ok(local);
                }
                session_id
            }
            Err(mpsc::error::SendError(Msg::Get { session_id, .. })) => {
                tracing::warn!(
                    psid = %pharos_core::secret_prefix(session_id.as_str()),
                    "transcode session cache is down; reading the store"
                );
                session_id
            }
            Err(_) => return Err(TranscodeSessionError::ActorDown),
        };

        match self
            .store
            .get_transcode_session(session_id.as_str(), now_unix_secs() - DB_RETENTION_SECS)
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
                            session_id: session_id.clone(),
                            session: session.clone(),
                        })))
                        .await;
                    Ok(Some(session))
                }
                Err(e) => {
                    tracing::warn!(
                        psid = %pharos_core::secret_prefix(session_id.as_str()),
                        error = %e,
                        "persisted transcode session failed to decode; ignoring"
                    );
                    Ok(None)
                }
            },
            Ok(None) => Ok(None),
            Err(e) => {
                tracing::warn!(
                    psid = %pharos_core::secret_prefix(session_id.as_str()),
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
        session_id: &PlaySessionId,
    ) -> Result<Option<TranscodeSession>, pharos_core::DomainError> {
        let Some(persisted) = self
            .store
            .get_transcode_session(session_id.as_str(), now_unix_secs() - DB_RETENTION_SECS)
            .await?
        else {
            return Ok(None);
        };
        match Self::from_persisted(&persisted) {
            Ok(session) => Ok(session),
            Err(e) => {
                tracing::warn!(
                    psid = %pharos_core::secret_prefix(session_id.as_str()),
                    error = %e,
                    "persisted transcode session failed to decode; ignoring"
                );
                Ok(None)
            }
        }
    }

    async fn delete_durable(&self, session_id: &PlaySessionId) {
        delete_durable_row(&self.store, session_id).await;
    }

    pub async fn remove(&self, session_id: &PlaySessionId) -> Result<(), TranscodeSessionError> {
        self.delete_durable(session_id).await;
        self.tx
            .send(Msg::Remove {
                session_id: session_id.clone(),
            })
            .await
            .map_err(|_| TranscodeSessionError::ActorDown)
    }

    /// End `session_id` if `user` holds it. Answers whether it was ended: a
    /// session someone else holds, or none at all, is left alone, so knowing
    /// another user's PlaySessionId is not enough to stop their playback.
    pub async fn end_held(
        &self,
        session_id: &PlaySessionId,
        user: pharos_core::UserId,
    ) -> Result<bool, TranscodeSessionError> {
        match self.get(session_id.clone()).await? {
            Some(session) if session.holder.user_id == user => {
                self.remove(session_id).await?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Makes `holder` the one the session's stream checks, in the store and
    /// then in memory: the playback was taken over by another token (B1397),
    /// so that token's revocation ends it and the old one's no longer does.
    /// The store goes first and only touches an existing row, so a session
    /// another replica stopped is not brought back by this replica's stale
    /// cache (B256); a store failure is returned, never swallowed, so the old
    /// token is not silently left as holder. `false` when the session is gone.
    ///
    /// With `epoch`, only while the session is still at that grant epoch
    /// (B1490): a grant a renewal took the session over from changes nothing,
    /// and reads as `false`, gone, like a stopped one.
    pub async fn rebind_holder(
        &self,
        session_id: &PlaySessionId,
        holder: SessionHolder,
        epoch: Option<i64>,
    ) -> Result<bool, TranscodeSessionError> {
        let Some(mut session) = self.get(session_id.clone()).await? else {
            return Ok(false);
        };
        let updated = self
            .store
            .update_transcode_session_holder(session_id.as_str(), &holder, now_unix_secs(), epoch)
            .await
            .map_err(|e| TranscodeSessionError::Store(e.to_string()))?;
        if !updated {
            // Stopped elsewhere: drop the stale local copy too.
            let _ = self
                .tx
                .send(Msg::Remove {
                    session_id: session_id.clone(),
                })
                .await;
            return Ok(false);
        }
        session.holder = holder;
        // The rebind committed; a cache that cannot take the copy does not
        // undo it, and `get` reads the store while the cache is down.
        if self
            .tx
            .send(Msg::Recache(Box::new(InsertPayload {
                session_id: session_id.clone(),
                session,
            })))
            .await
            .is_err()
        {
            tracing::warn!(
                psid = %pharos_core::secret_prefix(session_id.as_str()),
                "transcode session holder rebound, but caching it here failed"
            );
        }
        Ok(true)
    }

    /// The grant epoch `session_id` was at when its row was removed, as the
    /// store remembers it (B1490); `None` when it remembers no such removal.
    pub async fn ended_at(
        &self,
        session_id: &PlaySessionId,
    ) -> Result<Option<i64>, TranscodeSessionError> {
        self.store
            .transcode_session_end(session_id.as_str())
            .await
            .map_err(|e| TranscodeSessionError::Store(e.to_string()))
    }

    /// A renewal's takeover of `session` (B1490): in the store, one statement
    /// that refuses while any grant's lease on it runs and otherwise moves
    /// its epoch on and makes `holder` its holder; then, as
    /// [`Self::rebind_holder`] does, in memory. `Some(epoch)` = taken over,
    /// at that grant epoch; `None` = refused, and nothing changed.
    pub async fn take_over(
        &self,
        session_id: &PlaySessionId,
        mut session: TranscodeSession,
        source: &str,
        holder: SessionHolder,
        lease_until_unix_secs: i64,
    ) -> Result<Option<i64>, TranscodeSessionError> {
        let now = now_unix_secs();
        let Some(epoch) = self
            .store
            .take_over_transcode_session(
                session_id.as_str(),
                source,
                &holder,
                now,
                now - DB_RETENTION_SECS,
                lease_until_unix_secs,
            )
            .await
            .map_err(|e| TranscodeSessionError::Store(e.to_string()))?
        else {
            return Ok(None);
        };
        session.holder = holder;
        // The takeover committed: its epoch and lease are the grant's even
        // if the copy here cannot be updated, so the hold attaches to them
        // (and lets the lease go when it drops). A cache that cannot take
        // the copy cannot answer for it either, so `get` reads the row from
        // the store.
        if self
            .tx
            .send(Msg::Recache(Box::new(InsertPayload {
                session_id: session_id.clone(),
                session,
            })))
            .await
            .is_err()
        {
            tracing::warn!(
                psid = %pharos_core::secret_prefix(session_id.as_str()),
                epoch,
                "transcode session takeover committed, but caching it here failed"
            );
        }
        Ok(Some(epoch))
    }

    /// `session_id`'s lease as the store has it, to say why a takeover was
    /// refused.
    pub async fn grant(
        &self,
        session_id: &PlaySessionId,
    ) -> Result<Option<pharos_core::SessionGrant>, TranscodeSessionError> {
        self.store
            .transcode_session_grant(session_id.as_str())
            .await
            .map_err(|e| TranscodeSessionError::Store(e.to_string()))
    }

    /// A native grant's stop of `session_id`, which it holds at grant
    /// `epoch`: [`Self::end_held`], except that the row is removed only while
    /// it is still at that epoch, in the delete itself, so a stop from a
    /// grant a renewal took the session over from cannot end the stream the
    /// new grant plays (B1490).
    pub async fn end_granted(
        &self,
        session_id: &PlaySessionId,
        user: pharos_core::UserId,
        epoch: i64,
    ) -> Result<GrantEnd, TranscodeSessionError> {
        // Read from the store, keeping its errors: the cache, or a read that
        // failed taken as no row, would let a stop act on a session it
        // cannot show is its own. No retention cutoff: a lease's renewals
        // do not touch `updated_at`, so a grant playing for longer than the
        // retention still owns a row the cutoff would hide.
        let row = self
            .store
            .get_transcode_session(session_id.as_str(), i64::MIN)
            .await
            .map_err(|e| TranscodeSessionError::Store(e.to_string()))?;
        match row {
            // Gone: whether it ended while it was still this grant's (its own
            // stop whose answer was lost, or the session cap) is what the
            // store remembers of the removal, never this replica's view.
            None => {
                let at = self
                    .store
                    .transcode_session_end(session_id.as_str())
                    .await
                    .map_err(|e| TranscodeSessionError::Store(e.to_string()))?;
                return Ok(if at == Some(epoch) {
                    GrantEnd::EndedAt
                } else {
                    GrantEnd::Gone
                });
            }
            Some(row) if row.holder.as_ref().is_some_and(|h| h.user_id == user) => {}
            Some(_) => return Ok(GrantEnd::NotHeld),
        }
        let removed = self
            .store
            .remove_transcode_session_at(session_id.as_str(), epoch)
            .await
            .map_err(|e| TranscodeSessionError::Store(e.to_string()))?;
        // Either way the copy here goes: the session ended, or it answers to
        // another grant now, which the next use reloads from the store. The
        // delete decided the verdict; a cache that cannot take the removal
        // does not change it, and `get` reads the store when it is down.
        if self
            .tx
            .send(Msg::Remove {
                session_id: session_id.clone(),
            })
            .await
            .is_err()
        {
            tracing::warn!(
                psid = %pharos_core::secret_prefix(session_id.as_str()),
                epoch,
                removed,
                "transcode session stop: the store answered, but the cache here is down"
            );
        }
        if removed {
            return Ok(GrantEnd::Ended);
        }
        // Nothing removed: a renewal took the session over, or the row went
        // between the read above and the delete (the session cap). Only the
        // store's record of the removal tells the two apart.
        let at = self
            .store
            .transcode_session_end(session_id.as_str())
            .await
            .map_err(|e| TranscodeSessionError::Store(e.to_string()))?;
        Ok(if at == Some(epoch) {
            GrantEnd::EndedAt
        } else {
            GrantEnd::Superseded
        })
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
        assert!(r.get("old".into()).await.unwrap().is_none());
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
        assert!(r.get("stale".into()).await.unwrap().is_none());
        assert!(r.get("fresh".into()).await.unwrap().is_some());
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
        assert!(a.get_stored(&"s".into()).await.unwrap().is_some());
        assert!(b.end_held(&"s".into(), holder).await.unwrap());
        assert!(
            a.get("s".into()).await.unwrap().is_some(),
            "a's cache is untouched"
        );
        assert!(a.get_stored(&"s".into()).await.unwrap().is_none());

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
        assert!(a.get("old".into()).await.unwrap().is_some(), "still cached");
        assert!(a.get_stored(&"old".into()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn insert_then_get_returns_session() {
        let r = TranscodeSessionRegistry::spawn(store().await);
        r.insert("s1".into(), sample_session()).await.unwrap();
        let got = r.get("s1".into()).await.unwrap().unwrap();
        assert_eq!(got.media_id, 42);
    }

    #[tokio::test]
    async fn get_missing_returns_none() {
        let r = TranscodeSessionRegistry::spawn(store().await);
        let got = r.get("missing".into()).await.unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn remove_clears_session() {
        let r = TranscodeSessionRegistry::spawn(store().await);
        r.insert("s1".into(), sample_session()).await.unwrap();
        r.remove(&"s1".into()).await.unwrap();
        let got = r.get("s1".into()).await.unwrap();
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
        let got = replica_b.get("cross".into()).await.unwrap().unwrap();
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
        replica_a.remove(&"gone".into()).await.unwrap();
        assert!(replica_b.get("gone".into()).await.unwrap().is_none());
    }

    /// B1490: a takeover committed on a replica whose cache actor has
    /// stopped is still served: `get` reads the row when the cache cannot
    /// answer, so a renewal that answered `Resumed` is not followed by a 500
    /// on every segment. Disarm check: with `get` failing `ActorDown` when
    /// the cache send fails, the lookup errs.
    #[tokio::test]
    async fn a_takeover_is_served_with_the_cache_down() {
        let store = store().await;
        let alice = pharos_core::UserId::new();
        let up = TranscodeSessionRegistry::spawn(store.clone());
        up.insert("moved".into(), session_for(alice)).await.unwrap();
        // Leased elsewhere, and lapsed: free to take over.
        assert!(store
            .lease_transcode_session("moved", "file", 0)
            .await
            .unwrap()
            .is_some());
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let down = TranscodeSessionRegistry {
            tx,
            store: store.clone(),
        };
        let session = session_for(alice);
        let holder = session.holder.clone();
        let epoch = down
            .take_over(
                &"moved".into(),
                session,
                "file",
                holder,
                now_unix_secs() + 30,
            )
            .await
            .unwrap();
        assert!(epoch.is_some(), "the takeover commits");
        let got = down.get("moved".into()).await.unwrap();
        assert!(
            got.is_some_and(|s| s.holder.user_id == alice),
            "the moved session is served from its row"
        );
        // Its grant's stop ends it, and says so, though the cache cannot
        // take the removal.
        let ended = down
            .end_granted(&"moved".into(), alice, epoch.unwrap_or_default())
            .await
            .unwrap();
        assert_eq!(ended, GrantEnd::Ended, "the stop's verdict stands");
    }

    /// B1490: a stop whose delete committed but whose answer was lost is
    /// known, when the same grant retries it, to have ended the session at
    /// its epoch; a stop from a grant a renewal took the session over from,
    /// after the newer grant ended it, is not. Disarm check: with
    /// `end_granted` answering `Gone` for every gone row, the retry reads
    /// `Gone`.
    /// B1490: the session cap removing the row between a grant's read of it
    /// and its fenced delete still leaves the stop known ended at its epoch,
    /// so its final position is kept. Disarm check: with the no-removal arm
    /// answering `Superseded` without reading the store's record, this reads
    /// `Superseded`.
    #[tokio::test]
    async fn a_stop_the_cap_races_is_known_ended_at_its_epoch() {
        let gated = GatedStore {
            inner: store().await,
            armed: Default::default(),
            never: Default::default(),
            fail_holder: Default::default(),
            lose_remove_reply: Default::default(),
            cap_before_remove: Default::default(),
        };
        let alice = pharos_core::UserId::new();
        let r = TranscodeSessionRegistry::spawn(gated.clone());
        r.insert("raced".into(), session_for(alice)).await.unwrap();
        let epoch = gated
            .inner
            .lease_transcode_session("raced", "file", now_unix_secs() + 30)
            .await
            .unwrap()
            .unwrap_or_default();
        assert!(epoch > 0, "the control: the row is leased");
        gated
            .cap_before_remove
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            r.end_granted(&"raced".into(), alice, epoch).await.unwrap(),
            GrantEnd::EndedAt,
        );
    }

    /// B1490: a grant whose first lease errored is fenced at epoch 1 while
    /// its row may still be at epoch 0; its stop ends that row and records
    /// the end at the grant's own epoch, so a retry after a lost reply still
    /// knows it ended the session. Disarm check: with the end recorded at the
    /// row's epoch (0, so none), the retry reads `Gone`.
    #[tokio::test]
    async fn a_stop_of_a_never_leased_row_is_known_ended_on_retry() {
        let gated = GatedStore {
            inner: store().await,
            armed: Default::default(),
            never: Default::default(),
            fail_holder: Default::default(),
            lose_remove_reply: Default::default(),
            cap_before_remove: Default::default(),
        };
        let alice = pharos_core::UserId::new();
        let r = TranscodeSessionRegistry::spawn(gated.clone());
        r.insert("first-lease-failed".into(), session_for(alice))
            .await
            .unwrap();
        gated
            .lose_remove_reply
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(
            r.end_granted(&"first-lease-failed".into(), alice, 1)
                .await
                .is_err(),
            "the control: the stop's answer is lost"
        );
        gated
            .lose_remove_reply
            .store(false, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            r.end_granted(&"first-lease-failed".into(), alice, 1)
                .await
                .unwrap(),
            GrantEnd::EndedAt,
        );
        assert_eq!(
            r.end_granted(&"first-lease-failed".into(), alice, 2)
                .await
                .unwrap(),
            GrantEnd::Gone,
            "another epoch's stop is not this one's"
        );
    }

    /// B1490: a grant that has played for longer than the retention window
    /// still owns its row (its lease renewals do not touch `updated_at`), so
    /// its stop ends it. Disarm check: with the stop's read cut off at the
    /// retention, it reads `Gone` and the row stays.
    #[tokio::test]
    async fn a_stop_after_the_retention_window_still_ends_its_session() {
        let inner = store().await;
        let alice = pharos_core::UserId::new();
        let r = TranscodeSessionRegistry::spawn(inner.clone());
        r.insert("long-film".into(), session_for(alice))
            .await
            .unwrap();
        let epoch = inner
            .lease_transcode_session("long-film", "file", now_unix_secs() + 30)
            .await
            .unwrap()
            .unwrap_or_default();
        sqlx::query("UPDATE transcode_sessions SET updated_at = 0")
            .execute(inner.pool())
            .await
            .unwrap();
        assert_eq!(
            r.end_granted(&"long-film".into(), alice, epoch)
                .await
                .unwrap(),
            GrantEnd::Ended,
        );
    }

    #[tokio::test]
    async fn a_stop_whose_answer_was_lost_is_known_ended_on_retry() {
        let gated = GatedStore {
            inner: store().await,
            armed: Default::default(),
            never: Default::default(),
            fail_holder: Default::default(),
            lose_remove_reply: Default::default(),
            cap_before_remove: Default::default(),
        };
        let alice = pharos_core::UserId::new();
        let r = TranscodeSessionRegistry::spawn(gated.clone());
        let until = now_unix_secs() + 30;

        r.insert("lost-reply".into(), session_for(alice))
            .await
            .unwrap();
        let epoch = gated
            .inner
            .lease_transcode_session("lost-reply", "file", until)
            .await
            .unwrap()
            .unwrap_or_default();
        gated
            .lose_remove_reply
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(
            r.end_granted(&"lost-reply".into(), alice, epoch)
                .await
                .is_err(),
            "the control: the stop's answer is lost"
        );
        gated
            .lose_remove_reply
            .store(false, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            r.end_granted(&"lost-reply".into(), alice, epoch)
                .await
                .unwrap(),
            GrantEnd::EndedAt,
            "the retry knows it ended the session"
        );

        r.insert("moved-on".into(), session_for(alice))
            .await
            .unwrap();
        let first = gated
            .inner
            .lease_transcode_session("moved-on", "file", until)
            .await
            .unwrap()
            .unwrap_or_default();
        assert!(gated
            .inner
            .release_transcode_session_lease("moved-on", first)
            .await
            .unwrap());
        let renewing = pharos_core::SessionHolder {
            user_id: alice,
            token_id: "renewing".into(),
        };
        let second = r
            .take_over(
                &"moved-on".into(),
                session_for(alice),
                "file",
                renewing,
                until,
            )
            .await
            .unwrap()
            .unwrap_or_default();
        assert!(second > first, "the control: it was taken over");
        assert_eq!(
            r.end_granted(&"moved-on".into(), alice, second)
                .await
                .unwrap(),
            GrantEnd::Ended,
            "the newer grant ends it"
        );
        assert_eq!(
            r.end_granted(&"moved-on".into(), alice, first)
                .await
                .unwrap(),
            GrantEnd::Gone,
            "the older grant's stop is not its end"
        );
    }

    fn session_for(user: pharos_core::UserId) -> TranscodeSession {
        let mut s = sample_session();
        s.holder.user_id = user;
        s
    }

    async fn live(r: &TranscodeSessionRegistry<SqliteStore>, ids: &[PlaySessionId]) -> usize {
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
            let id = PlaySessionId::from(format!("alice-{i}"));
            assert!(r
                .insert(id.clone(), session_for(alice))
                .await
                .unwrap()
                .is_empty());
            alice_ids.push(id);
        }
        let evicted = r
            .insert(format!("alice-{cap}").into(), session_for(alice))
            .await
            .unwrap();
        assert_eq!(
            evicted,
            vec![PlaySessionId::from("alice-0")],
            "the first minted is ended"
        );
        alice_ids.push(PlaySessionId::from(format!("alice-{cap}")));

        assert!(
            r.get("alice-0".into()).await.unwrap().is_none(),
            "cache dropped it"
        );
        assert!(
            r.get_stored(&"alice-0".into()).await.unwrap().is_none(),
            "store row deleted"
        );
        assert_eq!(
            live(&r, &alice_ids).await,
            cap,
            "exactly the cap stays live"
        );
        assert!(
            r.get_stored(&"bob-0".into()).await.unwrap().is_some(),
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
            let id = PlaySessionId::from(format!("race-{i}"));
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
            if r.get(id.clone()).await.unwrap().is_some() {
                cached += 1;
            }
        }
        assert!(
            cached <= MAX_LIVE_PLAY_SESSIONS_PER_USER,
            "{cached} sessions still authorize"
        );
    }

    fn holder_of(token: &str, user: pharos_core::UserId) -> SessionHolder {
        SessionHolder {
            user_id: user,
            token_id: token.into(),
        }
    }

    /// B1397: rebinding moves the holder in memory and in the store.
    #[tokio::test]
    async fn rebind_holder_moves_the_holder_everywhere() {
        let store = store().await;
        let alice = pharos_core::UserId::new();
        let r = TranscodeSessionRegistry::spawn(store.clone());
        r.insert("s".into(), session_for(alice)).await.unwrap();
        let new = holder_of("new-token", alice);
        assert!(r
            .rebind_holder(&"s".into(), new.clone(), None)
            .await
            .unwrap());
        assert_eq!(r.get("s".into()).await.unwrap().unwrap().holder, new);
        let row = store.get_transcode_session("s", 0).await.unwrap().unwrap();
        assert_eq!(row.holder, Some(new));
    }

    /// B256: a session another replica stopped (row deleted) is not revived by
    /// a rebind through this replica's stale cache.
    /// Disarm check: with the holder written by upsert, the row is recreated
    /// and the session authorizes again.
    #[tokio::test]
    async fn rebind_holder_does_not_revive_a_stopped_session() {
        let store = store().await;
        let alice = pharos_core::UserId::new();
        let r = TranscodeSessionRegistry::spawn(store.clone());
        r.insert("s".into(), session_for(alice)).await.unwrap();
        // Another replica stops it: the row goes, this cache keeps its copy.
        store.remove_transcode_session("s").await.unwrap();
        let rebound = r
            .rebind_holder(&"s".into(), holder_of("new-token", alice), None)
            .await
            .unwrap();
        assert!(!rebound, "a stopped session is gone, not rebound");
        assert!(
            store.get_transcode_session("s", 0).await.unwrap().is_none(),
            "the row must not be recreated"
        );
        assert!(r.get("s".into()).await.unwrap().is_none());
    }

    /// B1397: a store failure is reported and leaves the old holder in place,
    /// so adoption is refused rather than leaving the old token as holder.
    /// Disarm check: with the error swallowed, this returns Ok(true).
    #[tokio::test]
    async fn rebind_holder_reports_a_store_failure_and_keeps_the_old_holder() {
        let gated = GatedStore {
            inner: store().await,
            armed: Default::default(),
            never: Default::default(),
            fail_holder: Default::default(),
            lose_remove_reply: Default::default(),
            cap_before_remove: Default::default(),
        };
        let alice = pharos_core::UserId::new();
        let r = TranscodeSessionRegistry::spawn(gated.clone());
        let original = session_for(alice);
        let old = original.holder.clone();
        r.insert("s".into(), original).await.unwrap();
        gated
            .fail_holder
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let out = r
            .rebind_holder(&"s".into(), holder_of("new-token", alice), None)
            .await;
        assert!(
            matches!(out, Err(TranscodeSessionError::Store(_))),
            "{out:?}"
        );
        assert_eq!(r.get("s".into()).await.unwrap().unwrap().holder, old);
    }

    /// A store that parks on a gate that is never opened, to show a hung store
    /// cannot stall the registry actor (and so every `get` on the replica).
    #[derive(Clone)]
    struct GatedStore {
        inner: SqliteStore,
        armed: std::sync::Arc<std::sync::atomic::AtomicBool>,
        never: std::sync::Arc<tokio::sync::Notify>,
        fail_holder: std::sync::Arc<std::sync::atomic::AtomicBool>,
        /// A grant's stop commits, and its answer is lost.
        lose_remove_reply: std::sync::Arc<std::sync::atomic::AtomicBool>,
        /// The session cap removes the row between a stop's read of it and
        /// its fenced delete.
        cap_before_remove: std::sync::Arc<std::sync::atomic::AtomicBool>,
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
        async fn update_transcode_session_holder(
            &self,
            play_session_id: &str,
            holder: &SessionHolder,
            now_unix_secs: i64,
            epoch: Option<i64>,
        ) -> pharos_core::DomainResult<bool> {
            if self.fail_holder.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(pharos_core::DomainError::Backend(
                    "holder update down".into(),
                ));
            }
            self.inner
                .update_transcode_session_holder(play_session_id, holder, now_unix_secs, epoch)
                .await
        }
        async fn lease_transcode_session(
            &self,
            play_session_id: &str,
            source: &str,
            lease_until_unix_secs: i64,
        ) -> pharos_core::DomainResult<Option<i64>> {
            self.inner
                .lease_transcode_session(play_session_id, source, lease_until_unix_secs)
                .await
        }
        async fn take_over_transcode_session(
            &self,
            play_session_id: &str,
            source: &str,
            holder: &SessionHolder,
            now_unix_secs: i64,
            updated_since_unix_secs: i64,
            lease_until_unix_secs: i64,
        ) -> pharos_core::DomainResult<Option<i64>> {
            self.inner
                .take_over_transcode_session(
                    play_session_id,
                    source,
                    holder,
                    now_unix_secs,
                    updated_since_unix_secs,
                    lease_until_unix_secs,
                )
                .await
        }
        async fn renew_transcode_session_lease(
            &self,
            play_session_id: &str,
            epoch: i64,
            lease_until_unix_secs: i64,
        ) -> pharos_core::DomainResult<bool> {
            self.inner
                .renew_transcode_session_lease(play_session_id, epoch, lease_until_unix_secs)
                .await
        }
        async fn release_transcode_session_lease(
            &self,
            play_session_id: &str,
            epoch: i64,
        ) -> pharos_core::DomainResult<bool> {
            self.inner
                .release_transcode_session_lease(play_session_id, epoch)
                .await
        }
        async fn transcode_session_grant(
            &self,
            play_session_id: &str,
        ) -> pharos_core::DomainResult<Option<pharos_core::SessionGrant>> {
            self.inner.transcode_session_grant(play_session_id).await
        }
        async fn remove_transcode_session_at(
            &self,
            play_session_id: &str,
            epoch: i64,
        ) -> pharos_core::DomainResult<bool> {
            self.park().await;
            if self
                .cap_before_remove
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                self.inner.remove_transcode_session(play_session_id).await?;
            }
            let removed = self
                .inner
                .remove_transcode_session_at(play_session_id, epoch)
                .await;
            if self
                .lose_remove_reply
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err(pharos_core::DomainError::Backend("connection reset".into()));
            }
            removed
        }
        async fn transcode_session_end(
            &self,
            play_session_id: &str,
        ) -> pharos_core::DomainResult<Option<i64>> {
            self.inner.transcode_session_end(play_session_id).await
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
            fail_holder: Default::default(),
            lose_remove_reply: Default::default(),
            cap_before_remove: Default::default(),
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

        let got = tokio::time::timeout(Duration::from_secs(1), r.get("cached".into()))
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
            r.insert(format!("log-{i}").into(), session_for(alice))
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
            store
                .upsert_transcode_session(id.as_str(), &row, ts)
                .await
                .unwrap();
            seeds.push(PlaySessionId::from(id));
        }

        let evicted = r
            .insert("mint-new".into(), session_for(alice))
            .await
            .unwrap();
        assert_eq!(
            evicted,
            vec![
                PlaySessionId::from("mem-old"),
                PlaySessionId::from("seed-00")
            ],
            "the two oldest durable rows are ended, oldest first"
        );
        assert!(
            r.get("mem-old".into()).await.unwrap().is_none(),
            "a trimmed session must leave this replica's memory too"
        );
        assert!(r.get_stored(&"seed-00".into()).await.unwrap().is_none());
        assert!(r.get_stored(&"seed-01".into()).await.unwrap().is_some());
        assert!(r.get_stored(&"mint-new".into()).await.unwrap().is_some());

        let mut ids = vec![
            PlaySessionId::from("mem-old"),
            PlaySessionId::from("mint-new"),
        ];
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
