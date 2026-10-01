//! `SessionRegistry` actor — owns the set of active playback sessions.
//! V18 — handlers send mpsc messages; no `Mutex` on the request path.

use pharos_core::UserId;
use pharos_jellyfin_api::dto::format_iso8601;
use serde::Serialize;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// Mirrors Jellyfin's `SessionManager.CheckForIdlePlayback`
/// (`Emby.Server.Implementations/Session/SessionManager.cs`): a session
/// with a `NowPlayingItem` whose last playback check-in is older than this
/// is treated as stopped (B227/V170). A capabilities-only stub (no
/// `now_playing_item_id`) is exempt — see [`sweep_idle_sessions`].
const SESSION_IDLE_TTL: Duration = Duration::from_secs(5 * 60);

/// Cadence of the idle sweep. Matching `SESSION_IDLE_TTL` mirrors upstream's
/// own 5-minute timer and keeps a session from going more than one interval
/// past the TTL before it is caught.
const SESSION_SWEEP_INTERVAL: Duration = Duration::from_secs(5 * 60);

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
    // Remaining non-nullable value-type fields of the C# SessionInfoDto —
    // always on the wire from real Jellyfin; strict SDK clients require them.
    pub is_active: bool,
    pub supports_remote_control: bool,
    pub has_custom_device_name: bool,
    /// ISO8601 UTC, stamped on every event. Never empty — see [`iso_now`].
    pub last_activity_date: String,
    pub last_playback_check_in: String,
    /// Monotonic stamp of the last Started/Progress event, for age
    /// comparisons (B227) that must not depend on wall-clock parsing or
    /// drift. Internal only — never serialized to the wire.
    #[serde(skip)]
    pub activity_at: tokio::time::Instant,
}

impl SessionRecord {
    /// Milliseconds since this record's last Started/Progress event.
    pub fn activity_age_ms(&self) -> u64 {
        u64::try_from(self.activity_at.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
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

/// Evict every session record whose `NowPlayingItem` has had no playback
/// check-in within [`SESSION_IDLE_TTL`] (B227/V170, parity with Jellyfin's
/// `CheckForIdlePlayback`). A capabilities-only stub (`now_playing_item_id`
/// is `None`) is left alone: it carries no playback to go idle, and
/// destroying it would only cost the NEXT `Started` on that session id its
/// P28 caps inheritance for no parity benefit — real Jellyfin's idle check
/// is scoped to sessions with a `NowPlayingItem` too.
///
/// Gap: real Jellyfin's `OnPlaybackStopped` also persists final `UserData`
/// (resume position / played state) for the session it idles out.
/// `SessionRegistry` has no store handle — it is spawned standalone in
/// `AppState::new`/`load` before `Stores` is threaded through — and wiring
/// one in is a bigger change than this fix. An idle-evicted session here
/// just disappears from `/Sessions`; its last reported position was already
/// persisted by the client's own `/Sessions/Playing/Progress` calls, so the
/// only loss is a final position between the last progress report and the
/// idle cutoff.
fn sweep_idle_sessions(state: &mut std::collections::HashMap<String, SessionRecord>) {
    let mut evicted: Vec<SessionRecord> = Vec::new();
    state.retain(|_, s| {
        if s.now_playing_item_id.is_some() && s.activity_at.elapsed() >= SESSION_IDLE_TTL {
            evicted.push(s.clone());
            false
        } else {
            true
        }
    });
    for s in &evicted {
        tracing::info!(
            session_id = %s.id,
            user_id = %s.user_id,
            device_id = %s.device_id,
            item_id = s.now_playing_item_id.as_deref().unwrap_or(""),
            idle_ms = s.activity_age_ms(),
            position_ticks = s.position_ticks,
            "session: idle-evicted (no playback check-in within SESSION_IDLE_TTL)"
        );
    }
    if !evicted.is_empty() {
        metrics::counter!("pharos_session_idle_evicted_total").increment(evicted.len() as u64);
    }
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
            let mut sweep = tokio::time::interval(SESSION_SWEEP_INTERVAL);
            // `interval` fires its first tick immediately; consume it here
            // so the loop below only sweeps on the real cadence.
            sweep.tick().await;
            loop {
                let msg = tokio::select! {
                    biased;
                    _ = sweep.tick() => {
                        sweep_idle_sessions(&mut state);
                        continue;
                    }
                    msg = rx.recv() => msg,
                };
                let Some(msg) = msg else {
                    break;
                };
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
                        // V169/B227 — a device plays one thing at a time. A
                        // Started for (user, device) supersedes any OTHER
                        // record already tracked for that same pair: without
                        // this, a playback that never reports Stopped (a
                        // SyncPlay item swap destroying the player, a reload,
                        // a crash) leaves a ghost that outlives the real
                        // session forever, and `creator_now_playing` (B227)
                        // could resolve it instead of the live one. Matching
                        // on (user_id, device_id) — never device alone — is
                        // load-bearing: B53 same-UA browsers share a device
                        // id across different users. A capabilities-only stub
                        // (empty user_id/device_id, `SetCapabilities` pre
                        // creating it under the incoming session id) never
                        // matches this filter, so caps inheritance above is
                        // unaffected, and `id == &session_id` keeps a refresh
                        // of the SAME session out of its own eviction list.
                        let uid = user_id.0.simple().to_string();
                        let mut superseded: Vec<(String, Option<String>)> = Vec::new();
                        state.retain(|id, s| {
                            if id == &session_id || s.user_id != uid || s.device_id != device_id {
                                return true;
                            }
                            superseded.push((s.id.clone(), s.now_playing_item_id.clone()));
                            false
                        });
                        if !superseded.is_empty() {
                            for (old_session_id, old_item_id) in &superseded {
                                tracing::info!(
                                    old_session_id = %old_session_id,
                                    old_item_id = old_item_id.as_deref().unwrap_or(""),
                                    new_session_id = %session_id,
                                    user_id = %uid,
                                    device_id = %device_id,
                                    "session: superseded by new playback on the same device"
                                );
                            }
                            metrics::counter!("pharos_session_superseded_total")
                                .increment(superseded.len() as u64);
                        }
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
                                is_active: true,
                                supports_remote_control: true,
                                has_custom_device_name: false,
                                last_activity_date: iso_now(),
                                last_playback_check_in: iso_now(),
                                activity_at: tokio::time::Instant::now(),
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
                            s.activity_at = tokio::time::Instant::now();
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
                                    is_active: true,
                                    supports_remote_control: true,
                                    has_custom_device_name: false,
                                    last_activity_date: iso_now(),
                                    last_playback_check_in: iso_now(),
                                    activity_at: tokio::time::Instant::now(),
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

    fn started_as(session_id: &str, user_id: UserId, device_id: &str, item: &str) -> SessionEvent {
        SessionEvent::Started {
            session_id: session_id.into(),
            user_id,
            user_name: "u".into(),
            device_id: device_id.into(),
            device_name: "dn".into(),
            client: "c".into(),
            version: "0".into(),
            item_id: item.into(),
            position_ticks: 0,
        }
    }

    /// B227/V169 — a device plays one thing at a time: a new Started for a
    /// (user, device) already tracked must evict the OLD record, leaving
    /// only the new one. Without this a playback that never reports Stopped
    /// (a SyncPlay item swap, a reload, a crash) sits forever and can be
    /// resolved by `creator_now_playing` instead of the live session.
    #[tokio::test]
    async fn started_evicts_other_sessions_on_same_user_and_device() {
        let r = SessionRegistry::spawn();
        let user = UserId::new();
        r.apply(started_as("ghost", user, "dev", "stale-item"))
            .await
            .unwrap();
        r.apply(started_as("live", user, "dev", "fresh-item"))
            .await
            .unwrap();
        let snap = r.snapshot().await.unwrap();
        assert_eq!(snap.len(), 1, "only the new session must remain: {snap:?}");
        assert_eq!(snap[0].id, "live");
        assert_eq!(snap[0].now_playing_item_id.as_deref(), Some("fresh-item"));
    }

    /// B53 — jellyfin-web derives an identical device id across same-UA
    /// installs, so two different users can share one device id. Eviction
    /// must key on (user, device), never device alone, or a second person's
    /// Started would wipe the first person's still-live session.
    #[tokio::test]
    async fn started_does_not_evict_a_different_user_on_the_same_device() {
        let r = SessionRegistry::spawn();
        let alison = UserId::new();
        let lace = UserId::new();
        r.apply(started_as("alison-sess", alison, "shared-dev", "a-item"))
            .await
            .unwrap();
        r.apply(started_as("lace-sess", lace, "shared-dev", "l-item"))
            .await
            .unwrap();
        let mut snap = r.snapshot().await.unwrap();
        snap.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(
            snap.len(),
            2,
            "different users must not evict each other: {snap:?}"
        );
    }

    /// B227/V170 idle TTL, mirroring Jellyfin's `CheckForIdlePlayback`: a
    /// session with no playback check-in for longer than `SESSION_IDLE_TTL`
    /// must be evicted by the periodic sweep.
    #[tokio::test(start_paused = true)]
    async fn idle_session_is_evicted_after_the_ttl() {
        let r = SessionRegistry::spawn();
        r.apply(started("s1", "100", 0)).await.unwrap();
        // The actor's sweep interval is constructed lazily on its first
        // poll. Yielding here lets that happen BEFORE the clock jumps, so
        // the interval's deadline is computed from the real baseline, not
        // from a clock that has already advanced past it.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        tokio::time::advance(SESSION_IDLE_TTL + SESSION_SWEEP_INTERVAL).await;
        tokio::task::yield_now().await;
        let snap = r.snapshot().await.unwrap();
        assert!(snap.is_empty(), "an idle session must be swept: {snap:?}");
    }

    /// A Progress report resets the idle clock: a session that keeps
    /// checking in must survive past where the ORIGINAL TTL window (counted
    /// from Started) would have fired.
    #[tokio::test(start_paused = true)]
    async fn progress_within_the_ttl_keeps_the_session_alive() {
        let r = SessionRegistry::spawn();
        r.apply(started("s1", "100", 0)).await.unwrap();
        // See idle_session_is_evicted_after_the_ttl: let the actor's sweep
        // interval register its real baseline deadline before any advance.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        tokio::time::advance(SESSION_IDLE_TTL / 2).await;
        r.apply(SessionEvent::Progress {
            session_id: "s1".into(),
            item_id: "100".into(),
            position_ticks: 1,
            is_paused: false,
        })
        .await
        .unwrap();
        // Total elapsed since Started now exceeds SESSION_IDLE_TTL, but only
        // SESSION_IDLE_TTL / 2 + 30s has passed since the Progress above.
        tokio::time::advance(SESSION_IDLE_TTL / 2 + Duration::from_secs(30)).await;
        tokio::task::yield_now().await;
        let snap = r.snapshot().await.unwrap();
        assert_eq!(
            snap.len(),
            1,
            "a session with recent activity must survive: {snap:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn idle_eviction_increments_the_metric() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let r = SessionRegistry::spawn();
        r.apply(started("s1", "100", 0)).await.unwrap();
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        tokio::time::advance(SESSION_IDLE_TTL + SESSION_SWEEP_INTERVAL).await;
        tokio::task::yield_now().await;
        let _ = r.snapshot().await.unwrap();

        let count = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .find_map(|(ck, _, _, v)| {
                (ck.key().name() == "pharos_session_idle_evicted_total").then_some(v)
            })
            .unwrap();
        assert!(
            matches!(count, DebugValue::Counter(1)),
            "exactly one idle eviction happened — got {count:?}"
        );
    }
}
