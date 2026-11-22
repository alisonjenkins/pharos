//! The play sessions this replica's native grants hold, and each one's lease
//! in the shared store (B1490, ADR-0150).
//!
//! A renewal may keep the play session it names only when no grant on any
//! replica still holds it. The counts here see this replica's grants; the
//! lease on the session's own row (migration 0082) is what the others see.
//! The first grant on a session leases it, this replica renews the leases of
//! every session its grants hold, and the last hold to go lets the lease go.
//! A renewal takes a session over in one statement that refuses while a
//! lease runs and moves the session's grant epoch on, and every later stop,
//! holder change, renewal or release from a grant names the epoch it holds,
//! so one from a grant the session has moved on from changes nothing.
//!
//! Nothing here holds a database connection: a lease is a value in a row,
//! so a lost connection or a stopped replica just stops renewing it, and it
//! lapses after [`LEASE_SECS`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use pharos_core::TranscodeSessionStore;

use super::playback::ResumeOutcome;

/// How long a lease lasts without renewing (B1490). After this a renewal on
/// another replica may take over a session whose replica stopped renewing it:
/// one that died, or lost the store.
pub const LEASE_SECS: i64 = 30;

/// How often this replica renews its leases: three times a lease, so one
/// failed renewal still leaves the lease running.
pub const RENEW_EVERY: Duration = Duration::from_secs(10);

/// How long one lease renewal may take before it counts as failed: well
/// under a lease, so a renewal stalled on the store cannot hold the others
/// (they run side by side) or outlast the lease it renews.
pub const RENEW_TIMEOUT: Duration = Duration::from_secs(5);

/// The grant epoch every session's first lease takes (from 0).
pub const FIRST_EPOCH: i64 = 1;

/// When a lease taken or renewed at `now_unix_secs` lapses.
pub const fn lease_until(now_unix_secs: i64) -> i64 {
    now_unix_secs.saturating_add(LEASE_SECS)
}

pub fn now_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

/// What became of a new session's lease; the `outcome` of
/// `pharos_play_session_claim_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimOutcome {
    Claimed,
    /// The session was gone, or claimed already.
    Refused,
    Failed,
    /// The replica is draining: the grant plays, but takes no lease, so no
    /// renewal may take its session over.
    Draining,
}

impl ClaimOutcome {
    pub const ALL: [Self; 4] = [Self::Claimed, Self::Refused, Self::Failed, Self::Draining];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Claimed => "claimed",
            Self::Refused => "refused",
            Self::Failed => "failed",
            Self::Draining => "draining",
        }
    }

    fn record(self, play_session_id: &str, reason: &str) {
        metrics::counter!("pharos_play_session_claim_total", "outcome" => self.label())
            .increment(1);
        if self != Self::Claimed {
            tracing::warn!(
                play_session_id = %pharos_core::secret_prefix(play_session_id),
                outcome = self.label(),
                %reason,
                "playback: a new play session took no lease, so no renewal can keep it"
            );
        }
    }
}

/// What a lease renewal or release did; the `outcome` of
/// `pharos_play_session_lease_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseOutcome {
    Renewed,
    /// The session is at another grant epoch, or gone: a renewal took it over
    /// after the lease lapsed, or it ended. This replica's grants on it are
    /// fenced out from then on.
    Lost,
    RenewFailed,
    Released,
    /// Nothing to let go: the session had moved on, or ended.
    ReleaseMoot,
    ReleaseFailed,
}

impl LeaseOutcome {
    pub const ALL: [Self; 6] = [
        Self::Renewed,
        Self::Lost,
        Self::RenewFailed,
        Self::Released,
        Self::ReleaseMoot,
        Self::ReleaseFailed,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Renewed => "renewed",
            Self::Lost => "lost",
            Self::RenewFailed => "renew_failed",
            Self::Released => "released",
            Self::ReleaseMoot => "release_moot",
            Self::ReleaseFailed => "release_failed",
        }
    }

    fn record(self) {
        metrics::counter!("pharos_play_session_lease_total", "outcome" => self.label())
            .increment(1);
    }
}

/// Why a renewal could not have the session it named.
#[derive(Debug)]
pub struct ClaimRefused {
    pub outcome: ResumeOutcome,
    pub reason: String,
}

/// How many live grants on this replica hold each play session, and the
/// grant epoch they hold its lease at.
pub struct PlayHolds<S = crate::state::Stores> {
    store: S,
    inner: Mutex<Inner>,
    /// The wall clock leases are dated by; tests move it on.
    clock: fn() -> i64,
    renew_timeout: Duration,
}

#[derive(Default)]
struct Inner {
    sessions: HashMap<String, Held>,
    /// Set when this replica starts draining, under the same lock as the
    /// map, so no lease is attached after the drain let them all go.
    draining: bool,
    /// Leases the last hold on a session let go, by session, at the epoch
    /// it let go: their release runs on its own task, and a drain that
    /// begins meanwhile waits for them too.
    releasing: HashMap<String, i64>,
}

#[derive(Default)]
struct Held {
    count: usize,
    /// The grant epoch the session's lease was taken at; kept after the lease
    /// is lost or let go, so the grants' writes stay fenced to it.
    epoch: Option<i64>,
    /// Whether this replica still renews the lease.
    renewing: bool,
    /// The deadline of the lease's last write known to have committed with
    /// it still ahead, by this replica's clock. Another replica judges the
    /// deadline by its own clock, so this does not rule out a takeover
    /// before it; once one commits, the store's epoch fence rejects this
    /// replica's stale writes.
    held_until: Option<i64>,
}

impl<S> std::fmt::Debug for PlayHolds<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner();
        f.debug_struct("PlayHolds")
            .field("sessions", &inner.sessions.len())
            .field("draining", &inner.draining)
            .finish()
    }
}

impl<S> PlayHolds<S> {
    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether a live grant on this replica holds `play_session_id`.
    pub fn is_held(&self, play_session_id: &str) -> bool {
        self.inner()
            .sessions
            .get(play_session_id)
            .is_some_and(|h| h.count > 0)
    }

    /// The grant epoch this replica's grants hold `play_session_id`'s lease
    /// at, if they took one.
    pub fn epoch(&self, play_session_id: &str) -> Option<i64> {
        self.inner()
            .sessions
            .get(play_session_id)
            .and_then(|h| h.epoch)
    }

    /// Whether this replica still renews `play_session_id`'s lease.
    pub fn is_renewing(&self, play_session_id: &str) -> bool {
        self.inner()
            .sessions
            .get(play_session_id)
            .is_some_and(|h| h.renewing)
    }

    /// Whether this replica holds `play_session_id`'s lease at `epoch`, as far
    /// as it can tell: it still renews it there, and its last committed
    /// deadline is ahead by this replica's clock. That is a snapshot: a
    /// takeover elsewhere compares the deadline with its own clock and can
    /// commit right after this answers, so it does not exclude one. What
    /// guards against a stale grant is the store's epoch fence on every
    /// stop, holder change, renewal and release.
    pub fn holds_lease(&self, play_session_id: &str, epoch: i64) -> bool {
        let now = (self.clock)();
        self.inner().sessions.get(play_session_id).is_some_and(|h| {
            h.epoch == Some(epoch) && h.renewing && h.held_until.is_some_and(|u| now < u)
        })
    }

    /// Whether a renewal may keep `play_session_id` at `epoch`, decided under
    /// one lock: this replica holds the lease there by the same local
    /// snapshot as [`Self::holds_lease`], and is not draining. Like that one
    /// it does not exclude a takeover elsewhere; once one commits, the
    /// store's epoch fence rejects this replica's stale writes.
    pub fn keeps(&self, play_session_id: &str, epoch: i64) -> bool {
        let now = (self.clock)();
        let inner = self.inner();
        !inner.draining
            && inner.sessions.get(play_session_id).is_some_and(|h| {
                h.epoch == Some(epoch) && h.renewing && h.held_until.is_some_and(|u| now < u)
            })
    }

    /// The wall-clock second leases are dated by.
    pub fn now(&self) -> i64 {
        (self.clock)()
    }
}

impl<S> PlayHolds<S>
where
    S: TranscodeSessionStore + Clone + Send + Sync + 'static,
{
    pub fn new(store: S) -> Self {
        Self {
            store,
            inner: Mutex::default(),
            clock: now_unix_secs,
            renew_timeout: RENEW_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn with(store: S, clock: fn() -> i64, renew_timeout: Duration) -> Self {
        Self {
            store,
            inner: Mutex::default(),
            clock,
            renew_timeout,
        }
    }

    /// A hold on `play_session_id` beside any others: a minted session's
    /// before its lease, or one adopted while its release is still being
    /// decided, whose lease the first hold already has.
    pub fn acquire(self: &Arc<Self>, play_session_id: &str) -> SessionHold<S> {
        self.inner()
            .sessions
            .entry(play_session_id.to_owned())
            .or_default()
            .count += 1;
        SessionHold {
            holds: Arc::clone(self),
            play_session_id: play_session_id.to_owned(),
        }
    }

    /// The first hold on `play_session_id`, for a renewal to take it over
    /// with: refused while a live grant on this replica holds it, or the
    /// replica is draining. Grants on other replicas are excluded by the
    /// takeover in the store; see [`Self::attach`].
    pub fn reserve(
        self: &Arc<Self>,
        play_session_id: &str,
    ) -> Result<SessionHold<S>, ClaimRefused> {
        let mut inner = self.inner();
        if inner.draining {
            return Err(ClaimRefused {
                outcome: ResumeOutcome::Live,
                reason: "this replica is draining; the one replacing it takes the stream over"
                    .to_owned(),
            });
        }
        if inner
            .sessions
            .get(play_session_id)
            .is_some_and(|h| h.count > 0)
        {
            return Err(ClaimRefused {
                outcome: ResumeOutcome::Live,
                reason: "a live grant on this replica holds it".to_owned(),
            });
        }
        inner.sessions.insert(
            play_session_id.to_owned(),
            Held {
                count: 1,
                ..Held::default()
            },
        );
        drop(inner);
        Ok(SessionHold {
            holds: Arc::clone(self),
            play_session_id: play_session_id.to_owned(),
        })
    }

    /// Records that `hold`'s grant has the session's lease at `epoch`, which
    /// this replica then renews. Refused, with the lease let go again, when
    /// the drain began while the lease was being taken: the drain let every
    /// lease go, and one attached after it would keep the replica replacing
    /// this one off the stream. The epoch is recorded either way, so the
    /// grant's stop and rebind stay fenced once that replica takes over.
    /// `until` is the deadline the lease's write committed, when it is known
    /// to have committed.
    pub async fn attach(
        &self,
        hold: &SessionHold<S>,
        epoch: i64,
        until: Option<i64>,
    ) -> Result<(), ClaimRefused> {
        let id = hold.play_session_id();
        let (recorded, draining) = {
            let mut inner = self.inner();
            let draining = inner.draining;
            match inner.sessions.get_mut(id) {
                Some(h) => {
                    h.epoch = Some(epoch);
                    h.renewing = !draining;
                    h.held_until = until;
                    (true, draining)
                }
                None => (false, draining),
            }
        };
        if recorded && !draining {
            return Ok(());
        }
        self.release(id, epoch).await;
        Err(ClaimRefused {
            outcome: ResumeOutcome::Live,
            reason: "this replica began draining while it took the lease".to_owned(),
        })
    }

    /// The lease of the first grant on a session it just minted and stored,
    /// playing `source`. The grant plays either way; one that took no lease
    /// is counted, and no renewal can take its session over, which is what
    /// a renewal did before B1490.
    pub async fn lease_minted(&self, hold: &SessionHold<S>, source: &str) {
        let id = hold.play_session_id();
        if self.inner().draining {
            ClaimOutcome::Draining.record(id, "this replica is draining");
            return;
        }
        let until = lease_until(self.now());
        let (outcome, reason) = match self.store.lease_transcode_session(id, source, until).await {
            Ok(Some(epoch)) => match self.attach(hold, epoch, Some(until)).await {
                Ok(()) => {
                    self.refresh_if_lapsed(hold, until).await;
                    (ClaimOutcome::Claimed, String::new())
                }
                Err(refused) => (ClaimOutcome::Draining, refused.reason),
            },
            Ok(None) => (
                ClaimOutcome::Refused,
                "the session was gone or claimed already".to_owned(),
            ),
            Err(e) => {
                // The write may have committed with only its answer lost. A
                // first lease is always epoch 1, so the grant fences itself
                // as 1 and keeps renewing it: if it committed, the renewals
                // hold it as any lease; if not, the first renewal finds
                // nothing at 1 and stops, and a stop fenced at 1 still ends
                // the session (still at 0, never past 1).
                let reason =
                    format!("leasing it failed, fenced as epoch 1 in case it committed: {e}");
                match self.attach(hold, FIRST_EPOCH, None).await {
                    Ok(()) => (ClaimOutcome::Failed, reason),
                    Err(refused) => (ClaimOutcome::Draining, refused.reason),
                }
            }
        };
        outcome.record(id, &reason);
    }

    /// Lets the lease at `epoch` go. Answers whether it was this replica's
    /// to let go.
    async fn release(&self, play_session_id: &str, epoch: i64) -> bool {
        let outcome = match self
            .store
            .release_transcode_session_lease(play_session_id, epoch)
            .await
        {
            Ok(true) => LeaseOutcome::Released,
            Ok(false) => LeaseOutcome::ReleaseMoot,
            Err(e) => {
                tracing::warn!(
                    play_session_id = %pharos_core::secret_prefix(play_session_id),
                    epoch,
                    error = %e,
                    "playback: letting a play session's lease go failed; it lapses on its own"
                );
                LeaseOutcome::ReleaseFailed
            }
        };
        outcome.record();
        outcome == LeaseOutcome::Released
    }

    /// Renews the lease of every session this replica's grants hold. A lease
    /// found at another epoch, or gone, is not renewed again: a renewal took
    /// the session over after it lapsed, or it ended, and the grants here
    /// stay fenced to the old epoch.
    pub async fn renew_all(&self) {
        let leased: Vec<(String, i64)> = {
            let inner = self.inner();
            if inner.draining {
                return;
            }
            inner
                .sessions
                .iter()
                .filter(|(_, h)| h.renewing)
                .filter_map(|(id, h)| Some((id.clone(), h.epoch?)))
                .collect()
        };
        // Side by side, each bounded: one stalled on the store cannot keep
        // the others from renewing before their leases lapse.
        futures_util::future::join_all(leased.iter().map(|(id, epoch)| self.renew(id, *epoch)))
            .await;
    }

    /// Renews the lease at `epoch` once, within the renewal timeout, and
    /// stops renewing it here when it moved on. `Renewed` only when the
    /// write committed with its deadline still ahead.
    async fn renew(&self, id: &str, epoch: i64) -> LeaseOutcome {
        let until = lease_until(self.now());
        let renewed = tokio::time::timeout(
            self.renew_timeout,
            self.store.renew_transcode_session_lease(id, epoch, until),
        )
        .await;
        let outcome = match renewed {
            Ok(Ok(true)) if self.now() < until => {
                if let Some(h) = self.inner().sessions.get_mut(id) {
                    if h.epoch == Some(epoch) {
                        h.held_until = Some(until);
                    }
                }
                LeaseOutcome::Renewed
            }
            // It committed, but so late its deadline had passed: another
            // replica may already have taken the session over. Not held, and
            // renewed again on the next tick, which finds out.
            Ok(Ok(true)) => {
                tracing::warn!(
                    play_session_id = %pharos_core::secret_prefix(id),
                    epoch,
                    until,
                    now = self.now(),
                    "playback: a play session's lease renewal landed after its own deadline"
                );
                LeaseOutcome::RenewFailed
            }
            Err(_) => {
                tracing::warn!(
                    play_session_id = %pharos_core::secret_prefix(id),
                    epoch,
                    timeout_ms = u64::try_from(self.renew_timeout.as_millis()).unwrap_or(u64::MAX),
                    "playback: renewing a play session's lease timed out"
                );
                LeaseOutcome::RenewFailed
            }
            Ok(Ok(false)) => {
                if let Some(h) = self.inner().sessions.get_mut(id) {
                    if h.epoch == Some(epoch) {
                        h.renewing = false;
                    }
                }
                tracing::warn!(
                    play_session_id = %pharos_core::secret_prefix(id),
                    epoch,
                    "playback: a play session's lease moved on or ended; its grants here are fenced out"
                );
                LeaseOutcome::Lost
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    play_session_id = %pharos_core::secret_prefix(id),
                    epoch,
                    error = %e,
                    "playback: renewing a play session's lease failed"
                );
                LeaseOutcome::RenewFailed
            }
        };
        outcome.record();
        outcome
    }

    /// A lease write that waited (for a connection, or a row lock) can land
    /// with its deadline `until`, taken before it, already past, which would
    /// let another replica take the session over at once. Renews `hold`'s
    /// lease straight away when that happened; `None` when the lease is
    /// still running, or the hold has no renewing lease.
    pub async fn refresh_if_lapsed(
        &self,
        hold: &SessionHold<S>,
        until: i64,
    ) -> Option<LeaseOutcome> {
        if self.now() < until {
            return None;
        }
        let id = hold.play_session_id();
        let epoch = {
            let inner = self.inner();
            let h = inner.sessions.get(id)?;
            if !h.renewing || inner.draining {
                return None;
            }
            h.epoch?
        };
        Some(self.renew(id, epoch).await)
    }

    /// Renews the leases every [`RENEW_EVERY`] for as long as the server runs.
    pub fn spawn_renewal(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(RENEW_EVERY);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                self.renew_all().await;
            }
        });
    }

    /// Lets every lease this replica holds go, at the start of a drain, so
    /// the replica replacing it can take over a stream whose renewal reaches
    /// it while this one finishes in-flight requests. The local counts and
    /// epochs stay: a renewal that still reaches this replica cannot take a
    /// session a grant here holds, and those grants' writes stay fenced. No
    /// lease is taken after it.
    pub async fn drain(&self) {
        let leased: Vec<(String, i64)> = {
            let mut inner = self.inner();
            inner.draining = true;
            let mut leased: Vec<(String, i64)> = inner
                .sessions
                .iter_mut()
                .filter_map(|(id, h)| {
                    let was = std::mem::take(&mut h.renewing);
                    was.then_some((id.clone(), h.epoch?))
                })
                .collect();
            // A release the last hold began may not have landed yet; one at
            // the same epoch again is moot if it has, and the drain does not
            // finish before it.
            leased.extend(inner.releasing.iter().map(|(id, e)| (id.clone(), *e)));
            leased
        };
        let results = futures_util::future::join_all(
            leased.iter().map(|(id, epoch)| self.release(id, *epoch)),
        )
        .await;
        tracing::info!(
            leases = leased.len(),
            released = results.iter().filter(|r| **r).count(),
            "playback: draining, let every play-session lease go for the replica taking over"
        );
    }

    /// Forgets every hold on `play_session_id` and lets its lease go, as a
    /// drain then a restart does: the grants that held it are gone with the
    /// old process. Tests stand in for a rollout with it while their grant
    /// objects stay alive.
    pub async fn forget(&self, play_session_id: &str) {
        let gone = self.inner().sessions.remove(play_session_id);
        if let Some(epoch) = gone.and_then(|h| h.epoch) {
            self.release(play_session_id, epoch).await;
        }
    }
}

/// One grant's hold on its play session, released on drop.
pub struct SessionHold<S = crate::state::Stores>
where
    S: TranscodeSessionStore + Clone + Send + Sync + 'static,
{
    holds: Arc<PlayHolds<S>>,
    play_session_id: String,
}

impl<S> std::fmt::Debug for SessionHold<S>
where
    S: TranscodeSessionStore + Clone + Send + Sync + 'static,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionHold")
            .field(
                "play_session_id",
                &pharos_core::secret_prefix(&self.play_session_id),
            )
            .finish()
    }
}

impl<S> SessionHold<S>
where
    S: TranscodeSessionStore + Clone + Send + Sync + 'static,
{
    pub fn play_session_id(&self) -> &str {
        &self.play_session_id
    }

    /// The grant epoch this grant's writes to the session are fenced to:
    /// `None` when it took no lease, and then they are not fenced.
    pub fn epoch(&self) -> Option<i64> {
        self.holds.epoch(&self.play_session_id)
    }
}

impl<S> Drop for SessionHold<S>
where
    S: TranscodeSessionStore + Clone + Send + Sync + 'static,
{
    fn drop(&mut self) {
        let rt = tokio::runtime::Handle::try_current().ok();
        let release = {
            let mut inner = self.holds.inner();
            let draining = inner.draining;
            let last = match inner.sessions.get_mut(&self.play_session_id) {
                Some(h) => {
                    h.count = h.count.saturating_sub(1);
                    h.count == 0
                }
                None => false,
            };
            let release = if last {
                inner
                    .sessions
                    .remove(&self.play_session_id)
                    .filter(|h| h.renewing && !draining)
                    .and_then(|h| h.epoch)
            } else {
                None
            };
            if let (Some(epoch), Some(_)) = (release, &rt) {
                inner.releasing.insert(self.play_session_id.clone(), epoch);
            }
            release
        };
        // The last hold lets the lease go, so a renewal may take the session
        // over at once. Without a runtime it lapses on its own.
        if let (Some(epoch), Some(rt)) = (release, rt) {
            let holds = Arc::clone(&self.holds);
            let id = std::mem::take(&mut self.play_session_id);
            rt.spawn(async move {
                holds.release(&id, epoch).await;
                let mut inner = holds.inner();
                if inner.releasing.get(&id) == Some(&epoch) {
                    inner.releasing.remove(&id);
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{ClaimOutcome, LeaseOutcome, PlayHolds, ResumeOutcome};
    use pharos_core::{PersistedTranscodeSession, SessionHolder, TranscodeSessionStore, UserId};
    use pharos_store_sqlx::sqlite::SqliteStore;
    use std::sync::Arc;

    async fn store() -> SqliteStore {
        SqliteStore::connect("sqlite::memory:").await.unwrap()
    }

    async fn stored(store: &SqliteStore, id: &str, user: UserId) {
        let row = PersistedTranscodeSession {
            media_id: 1,
            decision_json: r#""DirectPlay""#.into(),
            source_probe_json: "{}".into(),
            burn_subtitle_indices_json: "[]".into(),
            holder: Some(SessionHolder {
                user_id: user,
                token_id: "token".into(),
            }),
        };
        store
            .upsert_transcode_session(id, &row, super::now_unix_secs())
            .await
            .unwrap();
    }

    async fn take_over(store: &SqliteStore, id: &str, user: UserId) -> Option<i64> {
        let now = super::now_unix_secs();
        let holder = SessionHolder {
            user_id: user,
            token_id: "renewing".into(),
        };
        store
            .take_over_transcode_session(id, "file", &holder, now, 0, super::lease_until(now))
            .await
            .unwrap()
    }

    /// Lets the hold-drop release task run.
    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    /// Waits for the hold-drop release task to let `id`'s lease go, for at
    /// most 5 s (well inside the 30 s lease, so a lapse cannot pass for it).
    async fn released(store: &SqliteStore, id: &str) -> bool {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let grant = store.transcode_session_grant(id).await.unwrap();
            if grant.is_some_and(|g| g.lease_until_unix_secs.is_none()) {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// B1490: a session any live grant on this replica holds cannot be
    /// reserved by a renewal, whether that grant minted it or adopted it; it
    /// frees once the last hold drops.
    /// Disarm check: with `reserve` ignoring the count, the reservations hold.
    #[tokio::test]
    async fn a_held_session_is_not_reserved_until_every_hold_drops() {
        let holds = Arc::new(PlayHolds::new(store().await));
        let id = "play-hold-test-session";
        let minted = holds.acquire(id);
        assert!(holds.reserve(id).is_err(), "the minting grant holds it");
        let adopted = holds.acquire(id);
        drop(minted);
        assert!(holds.reserve(id).is_err(), "the adopting grant holds it");
        assert!(holds.reserve("another-session").is_ok(), "others are free");
        drop(adopted);
        let resumed = holds.reserve(id).expect("free once no grant holds it");
        assert!(holds.reserve(id).is_err(), "a second renewal is refused");
        drop(resumed);
        assert!(!holds.is_held(id));
    }

    /// B1490: a minted session's lease keeps a renewal on another replica off
    /// it while this replica renews it; the last hold dropping lets it go,
    /// and the renewal then takes it over. Disarm check: with the drop not
    /// letting the lease go, the takeover after it is refused.
    #[tokio::test]
    async fn a_lease_excludes_other_replicas_until_its_last_hold_drops() {
        let store = store().await;
        let user = UserId::new();
        let id = "lease-test-session";
        stored(&store, id, user).await;
        let here = Arc::new(PlayHolds::new(store.clone()));
        let minted = here.acquire(id);
        here.lease_minted(&minted, "file").await;
        let epoch = minted.epoch().expect("leased");
        assert!(here.is_renewing(id));
        assert_eq!(
            take_over(&store, id, user).await,
            None,
            "another replica's renewal is refused while the lease runs"
        );
        here.renew_all().await;
        assert!(here.is_renewing(id), "the renewal kept it");
        let adopted = here.acquire(id);
        drop(minted);
        settle().await;
        assert_eq!(take_over(&store, id, user).await, None, "a hold remains");
        drop(adopted);
        assert!(released(&store, id).await, "the last hold let the lease go");
        let next = take_over(&store, id, user)
            .await
            .expect("the last hold let the lease go");
        assert!(next > epoch);
    }

    /// B1490: when a lease lapses (the replica lost the store, or stopped)
    /// and a renewal elsewhere takes the session over, this replica stops
    /// renewing it, and its grants' epoch is the old one, which the store
    /// fences out of every write. Disarm check: with `renew_all` keeping a
    /// lost lease, `is_renewing` stays true.
    #[tokio::test]
    async fn a_lapsed_lease_taken_over_elsewhere_is_given_up_here() {
        let store = store().await;
        let user = UserId::new();
        let id = "lapsed-test-session";
        stored(&store, id, user).await;
        let here = Arc::new(PlayHolds::new(store.clone()));
        let grant = here.acquire(id);
        // A lease that has already lapsed, as one this replica failed to
        // renew for a whole lease would be.
        let epoch = store
            .lease_transcode_session(id, "file", 0)
            .await
            .unwrap()
            .expect("leased");
        here.attach(&grant, epoch, Some(0)).await.expect("attached");
        let taken = take_over(&store, id, user).await.expect("lapsed, so taken");
        assert!(taken > epoch);
        here.renew_all().await;
        assert!(!here.is_renewing(id), "lost, so no longer renewed");
        assert_eq!(grant.epoch(), Some(epoch), "the grant stays fenced to it");
        assert!(
            !store.remove_transcode_session_at(id, epoch).await.unwrap(),
            "its stop cannot end the new grant's stream"
        );
    }

    /// B1490: a lease whose write landed with its deadline already past (it
    /// waited for a connection or a row lock) is renewed at once, so no
    /// other replica can take the session straight over. Disarm check: with
    /// `refresh_if_lapsed` answering `None`, the lease stays lapsed and the
    /// takeover after it wins.
    #[tokio::test]
    async fn a_lease_that_landed_lapsed_is_renewed_at_once() {
        let store = store().await;
        let user = UserId::new();
        let id = "landed-lapsed-session";
        stored(&store, id, user).await;
        let here = Arc::new(PlayHolds::new(store.clone()));
        let grant = here.acquire(id);
        let until = 0;
        let epoch = store
            .lease_transcode_session(id, "file", until)
            .await
            .unwrap()
            .expect("leased");
        here.attach(&grant, epoch, Some(0)).await.expect("attached");
        assert_eq!(
            here.refresh_if_lapsed(&grant, until).await,
            Some(LeaseOutcome::Renewed)
        );
        assert_eq!(
            take_over(&store, id, user).await,
            None,
            "the renewed lease keeps other replicas off it"
        );
        let fresh = super::lease_until(super::now_unix_secs());
        assert_eq!(
            here.refresh_if_lapsed(&grant, fresh).await,
            None,
            "a running lease is left"
        );
        drop(grant);
    }

    /// B1490: a new session whose lease could not be taken (here: refused,
    /// as the row was not stored yet; a store failure leaves it the same)
    /// plays on unleased, and no renewal on any replica can take it over:
    /// it was never leased, so nothing says no other grant plays it.
    /// Disarm check: with the takeover's `grant_epoch > 0` guard removed,
    /// the takeover wins.
    #[tokio::test]
    async fn a_session_whose_lease_failed_is_never_taken_over() {
        let store = store().await;
        let user = UserId::new();
        let id = "unleased-test-session";
        let here = Arc::new(PlayHolds::new(store.clone()));
        let grant = here.acquire(id);
        here.lease_minted(&grant, "file").await;
        assert_eq!(grant.epoch(), None, "refused: no row to lease");
        stored(&store, id, user).await;
        // Every other takeover predicate holds (same holder, fresh row, no
        // running lease, the same source), so only the epoch-0 guard refuses.
        sqlx::query(
            "UPDATE transcode_sessions SET grant_source = 'file' WHERE play_session_id = ?",
        )
        .bind(id)
        .execute(store.pool())
        .await
        .unwrap();
        assert_eq!(
            take_over(&store, id, user).await,
            None,
            "an unleased session is never taken over"
        );
        drop(grant);
    }

    /// A store whose first-lease write fails: after committing it (its
    /// answer lost) or before. With `fails` false the write succeeds.
    /// `drains`, once set, is the replica that begins its drain while the
    /// write is in flight.
    #[derive(Clone)]
    struct LeaseFails {
        inner: SqliteStore,
        commits: bool,
        fails: bool,
        drains: Arc<std::sync::OnceLock<std::sync::Weak<PlayHolds<LeaseFails>>>>,
        /// Sessions whose lease renewals never answer.
        stalls: &'static [&'static str],
        /// Stalled renewals started and not yet given up on.
        stalling: Arc<std::sync::atomic::AtomicUsize>,
        /// For each other renewal that committed, how many stalled ones were
        /// pending when it did.
        renewed_beside: Arc<std::sync::Mutex<Vec<usize>>>,
        /// How long a lease release waits before it is written.
        release_delay: Option<std::time::Duration>,
    }

    /// Counts a stalled renewal as pending until its future is dropped (the
    /// renewal's timeout cancels it).
    struct Stalling(Arc<std::sync::atomic::AtomicUsize>);

    impl Drop for Stalling {
        fn drop(&mut self) {
            self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl LeaseFails {
        fn new(inner: SqliteStore, commits: bool, fails: bool) -> Self {
            Self {
                inner,
                commits,
                fails,
                drains: Arc::default(),
                stalls: &[],
                stalling: Arc::default(),
                renewed_beside: Arc::default(),
                release_delay: None,
            }
        }
    }

    impl TranscodeSessionStore for LeaseFails {
        async fn upsert_transcode_session(
            &self,
            id: &str,
            session: &PersistedTranscodeSession,
            now: i64,
        ) -> pharos_core::DomainResult<()> {
            self.inner.upsert_transcode_session(id, session, now).await
        }
        async fn update_transcode_session_holder(
            &self,
            id: &str,
            holder: &SessionHolder,
            now: i64,
            epoch: Option<i64>,
        ) -> pharos_core::DomainResult<bool> {
            self.inner
                .update_transcode_session_holder(id, holder, now, epoch)
                .await
        }
        async fn get_transcode_session(
            &self,
            id: &str,
            since: i64,
        ) -> pharos_core::DomainResult<Option<PersistedTranscodeSession>> {
            self.inner.get_transcode_session(id, since).await
        }
        async fn remove_transcode_session(&self, id: &str) -> pharos_core::DomainResult<()> {
            self.inner.remove_transcode_session(id).await
        }
        async fn prune_transcode_sessions(&self, cutoff: i64) -> pharos_core::DomainResult<u64> {
            self.inner.prune_transcode_sessions(cutoff).await
        }
        async fn trim_transcode_sessions_for_holder(
            &self,
            holder: UserId,
            except: &str,
            keep: usize,
            since: i64,
        ) -> pharos_core::DomainResult<Vec<String>> {
            self.inner
                .trim_transcode_sessions_for_holder(holder, except, keep, since)
                .await
        }
        async fn lease_transcode_session(
            &self,
            id: &str,
            source: &str,
            until: i64,
        ) -> pharos_core::DomainResult<Option<i64>> {
            let leased = if self.commits {
                self.inner
                    .lease_transcode_session(id, source, until)
                    .await?
            } else {
                None
            };
            if let Some(holds) = self.drains.get().and_then(std::sync::Weak::upgrade) {
                holds.drain().await;
            }
            if self.fails {
                return Err(pharos_core::DomainError::Backend("connection reset".into()));
            }
            Ok(leased)
        }
        async fn take_over_transcode_session(
            &self,
            id: &str,
            source: &str,
            holder: &SessionHolder,
            now: i64,
            since: i64,
            until: i64,
        ) -> pharos_core::DomainResult<Option<i64>> {
            self.inner
                .take_over_transcode_session(id, source, holder, now, since, until)
                .await
        }
        async fn renew_transcode_session_lease(
            &self,
            id: &str,
            epoch: i64,
            until: i64,
        ) -> pharos_core::DomainResult<bool> {
            use std::sync::atomic::Ordering::SeqCst;
            if self.stalls.contains(&id) {
                self.stalling.fetch_add(1, SeqCst);
                let _pending = Stalling(Arc::clone(&self.stalling));
                std::future::pending::<()>().await;
            }
            if !self.stalls.is_empty() {
                // Give a sweep that runs them together the chance to start a
                // stalled one first; one that renews one at a time either has
                // not reached one yet or has already given up on them all.
                for _ in 0..50 {
                    if self.stalling.load(SeqCst) > 0 {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }
            let renewed = self
                .inner
                .renew_transcode_session_lease(id, epoch, until)
                .await;
            if let Ok(mut beside) = self.renewed_beside.lock() {
                beside.push(self.stalling.load(SeqCst));
            }
            renewed
        }
        async fn release_transcode_session_lease(
            &self,
            id: &str,
            epoch: i64,
        ) -> pharos_core::DomainResult<bool> {
            if let Some(delay) = self.release_delay {
                tokio::time::sleep(delay).await;
            }
            self.inner.release_transcode_session_lease(id, epoch).await
        }
        async fn transcode_session_grant(
            &self,
            id: &str,
        ) -> pharos_core::DomainResult<Option<pharos_core::SessionGrant>> {
            self.inner.transcode_session_grant(id).await
        }
        async fn remove_transcode_session_at(
            &self,
            id: &str,
            epoch: i64,
        ) -> pharos_core::DomainResult<bool> {
            self.inner.remove_transcode_session_at(id, epoch).await
        }
        async fn transcode_session_end(&self, id: &str) -> pharos_core::DomainResult<Option<i64>> {
            self.inner.transcode_session_end(id).await
        }
    }

    /// B1490: a first lease whose write failed may still have committed
    /// (only its answer lost). The grant fences itself as epoch 1 either
    /// way: when it committed, it renews the lease and, once a renewal
    /// elsewhere has taken the session over, its stop ends nothing; when it
    /// did not, its stop still ends its own session.
    /// Disarm check: with `lease_minted` attaching nothing on an error, the
    /// committed case's stop runs unfenced and the session's epoch is not
    /// held to 1, so the first assertion fails.
    #[tokio::test]
    async fn a_failed_first_lease_fences_the_grant_whether_or_not_it_committed() {
        let user = UserId::new();
        for commits in [true, false] {
            let inner = store().await;
            let id = "lease-fails-test-session";
            stored(&inner, id, user).await;
            let here = Arc::new(PlayHolds::new(LeaseFails::new(
                inner.clone(),
                commits,
                true,
            )));
            let grant = here.acquire(id);
            here.lease_minted(&grant, "file").await;
            assert_eq!(grant.epoch(), Some(1), "commits={commits}: fenced as 1");
            here.renew_all().await;
            assert_eq!(
                here.is_renewing(id),
                commits,
                "a committed lease is kept; one that never landed is given up"
            );
            if commits {
                // Its lease lapses and a renewal elsewhere takes it over.
                assert!(inner.renew_transcode_session_lease(id, 1, 0).await.unwrap());
                assert!(take_over(&inner, id, user).await.is_some());
                assert!(
                    !inner.remove_transcode_session_at(id, 1).await.unwrap(),
                    "the old grant's stop cannot end the new owner's stream"
                );
            } else {
                assert!(
                    inner.remove_transcode_session_at(id, 1).await.unwrap(),
                    "a stop fenced at 1 still ends a session left at 0"
                );
            }
            drop(grant);
        }
    }

    /// B1490: when the drain begins while a minted session's first lease is
    /// being written, the lease is let go again, but the grant keeps its
    /// epoch, whether the write answered or committed with its answer lost.
    /// Once the replica replacing this one takes the session over, the old
    /// grant's delayed stop and rebind, which name that epoch, change
    /// nothing. Disarm check: with `attach` recording no epoch while
    /// draining, the grant's epoch is `None`.
    #[tokio::test]
    async fn a_lease_the_drain_interrupts_still_fences_its_grant() {
        let user = UserId::new();
        for fails in [false, true] {
            let inner = store().await;
            let id = "drain-interrupts-lease-session";
            stored(&inner, id, user).await;
            let wrapped = LeaseFails::new(inner.clone(), true, fails);
            let drains = Arc::clone(&wrapped.drains);
            let here = Arc::new(PlayHolds::new(wrapped));
            assert!(drains.set(Arc::downgrade(&here)).is_ok());
            let grant = here.acquire(id);
            here.lease_minted(&grant, "file").await;
            assert_eq!(grant.epoch(), Some(1), "fails={fails}: the epoch is kept");
            assert!(!here.is_renewing(id), "fails={fails}: and not renewed");
            let taken = take_over(&inner, id, user)
                .await
                .expect("let go, so the replacing replica takes it over");
            assert!(taken > 1);
            let stale = SessionHolder {
                user_id: user,
                token_id: "old-grant".into(),
            };
            assert!(
                !inner
                    .update_transcode_session_holder(
                        id,
                        &stale,
                        super::now_unix_secs(),
                        grant.epoch()
                    )
                    .await
                    .unwrap(),
                "fails={fails}: the old grant's rebind is fenced"
            );
            assert!(
                !inner
                    .remove_transcode_session_at(id, grant.epoch().unwrap())
                    .await
                    .unwrap(),
                "fails={fails}: the old grant's delayed stop is fenced"
            );
            assert!(
                inner.get_transcode_session(id, 0).await.unwrap().is_some(),
                "fails={fails}: the new owner's session runs on"
            );
            drop(grant);
        }
    }

    /// A draining replica lets every lease go at once, so the replica
    /// replacing it can take the streams over, keeps its local holds, and
    /// neither reserves nor leases anything after; a lease attached once the
    /// drain began is let go again and refused. Disarm check: with `drain`
    /// keeping the leases, the first takeover is refused.
    /// B1490: a renewal stalled on the store holds up no other session's:
    /// the healthy lease is renewed while the stalled writes are still
    /// pending, and they count as failed once their timeout passes, well
    /// inside the lease. The store records how many stalled writes were
    /// pending when the healthy one committed, so the check holds whatever
    /// order the sweep takes the sessions in. Disarm check: with `renew_all`
    /// renewing one after another, the healthy renewal commits beside none
    /// (it came first, before any stall began) or after the sweep gave up
    /// on them; with no timeout, the sweep never returns.
    #[tokio::test]
    async fn a_stalled_renewal_holds_up_no_other() {
        const STALLED: &[&str] = &["stalled-a", "stalled-b", "stalled-c"];
        let sqlite = store().await;
        let user = UserId::new();
        for id in STALLED.iter().chain(&["flowing"]) {
            stored(&sqlite, id, user).await;
        }
        let mut store = LeaseFails::new(sqlite.clone(), true, false);
        store.stalls = STALLED;
        let renewed_beside = Arc::clone(&store.renewed_beside);
        let here = Arc::new(PlayHolds::with(
            store,
            super::now_unix_secs,
            std::time::Duration::from_secs(2),
        ));
        let mut holds = Vec::new();
        for id in STALLED.iter().chain(&["flowing"]) {
            let hold = here.acquire(id);
            here.lease_minted(&hold, "file").await;
            holds.push(hold);
        }
        let epoch = here.epoch("flowing").expect("leased");
        // Lapse the flowing lease in the store: only a renewal brings it back.
        assert!(sqlite
            .renew_transcode_session_lease("flowing", epoch, 0)
            .await
            .unwrap());
        let sweep = tokio::spawn({
            let here = Arc::clone(&here);
            async move { here.renew_all().await }
        });
        let mut renewed = false;
        for _ in 0..100 {
            let lease = sqlite.transcode_session_grant("flowing").await.unwrap();
            if lease
                .and_then(|g| g.lease_until_unix_secs)
                .is_some_and(|u| u > 0)
            {
                renewed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(renewed, "the healthy lease is renewed");
        tokio::time::timeout(std::time::Duration::from_secs(10), sweep)
            .await
            .expect("the stalled renewals time out instead of holding the sweep")
            .unwrap();
        assert_eq!(
            renewed_beside.lock().unwrap().as_slice(),
            &[STALLED.len()],
            "the healthy renewal committed with every stalled one pending"
        );
        assert_eq!(
            take_over(&sqlite, "flowing", user).await,
            None,
            "the other session stays renewed"
        );
        assert!(
            here.is_renewing("stalled-a"),
            "a timed-out renewal is retried"
        );
        drop(holds);
    }

    /// B1490: a drain that begins while the last hold's release is still on
    /// its way waits for it, so the replica replacing this one never finds
    /// the lease still running once the drain is done. Disarm check: with
    /// `drain` leaving `releasing` out, it returns at once and the lease is
    /// still held.
    #[tokio::test]
    async fn a_drain_waits_for_a_release_already_on_its_way() {
        let sqlite = store().await;
        let user = UserId::new();
        let id = "released-at-drain";
        stored(&sqlite, id, user).await;
        let mut store = LeaseFails::new(sqlite.clone(), true, false);
        store.release_delay = Some(std::time::Duration::from_millis(300));
        let here = Arc::new(PlayHolds::new(store));
        let hold = here.acquire(id);
        here.lease_minted(&hold, "file").await;
        assert!(here.epoch(id).is_some(), "the control: it is leased");
        drop(hold);
        here.drain().await;
        let grant = sqlite.transcode_session_grant(id).await.unwrap();
        assert!(
            grant.is_some_and(|g| g.lease_until_unix_secs.is_none()),
            "the lease is let go before the drain finishes"
        );
    }

    /// A clock that moves a whole lease on each time it is read, as a store
    /// write that waited longer than a lease would see it.
    fn leaping() -> i64 {
        static NOW: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1_000_000_000);
        NOW.fetch_add(super::LEASE_SECS + 1, std::sync::atomic::Ordering::SeqCst)
    }

    /// B1490: a renewal that committed only after its own deadline passed
    /// is not counted as holding the lease: another replica may already have
    /// taken the session. Disarm check: with `renew` answering `Renewed` for
    /// any committed write, the outcome is `Renewed` and `holds_lease` true.
    #[tokio::test]
    async fn a_renewal_that_lands_after_its_deadline_holds_nothing() {
        let store = store().await;
        let user = UserId::new();
        let id = "late-renewal-session";
        stored(&store, id, user).await;
        let here = Arc::new(PlayHolds::with(
            store.clone(),
            leaping,
            super::RENEW_TIMEOUT,
        ));
        let grant = here.acquire(id);
        let epoch = store
            .lease_transcode_session(id, "file", 0)
            .await
            .unwrap()
            .expect("leased");
        here.attach(&grant, epoch, None).await.expect("attached");
        assert_eq!(here.renew(id, epoch).await, LeaseOutcome::RenewFailed);
        assert!(!here.holds_lease(id, epoch), "a late renewal holds nothing");
        assert!(here.is_renewing(id), "and is tried again on the next tick");
        drop(grant);
    }

    /// B1490: this replica holds a lease only while it renews it at that
    /// epoch with its last committed deadline ahead; a grant whose session
    /// row the store cannot read goes by it. Disarm check: with
    /// `holds_lease` ignoring `renewing`, the lost lease still reads held.
    #[tokio::test]
    async fn a_lease_is_held_only_while_it_runs_here() {
        let store = store().await;
        let user = UserId::new();
        let id = "held-lease-session";
        stored(&store, id, user).await;
        let here = Arc::new(PlayHolds::new(store.clone()));
        let grant = here.acquire(id);
        here.lease_minted(&grant, "file").await;
        let epoch = grant.epoch().expect("leased");
        assert!(here.holds_lease(id, epoch), "a fresh lease is held");
        assert!(!here.holds_lease(id, epoch + 1), "at its own epoch only");
        assert!(store
            .release_transcode_session_lease(id, epoch)
            .await
            .unwrap());
        let taken = take_over(&store, id, user)
            .await
            .expect("released, so taken");
        assert!(taken > epoch);
        here.renew_all().await;
        assert!(!here.holds_lease(id, epoch), "a lost lease is not held");
        drop(grant);
    }

    #[tokio::test]
    async fn draining_lets_every_lease_go_and_takes_none() {
        let store = store().await;
        let user = UserId::new();
        for id in ["draining-a", "draining-b", "draining-c"] {
            stored(&store, id, user).await;
        }
        let here = Arc::new(PlayHolds::new(store.clone()));
        let live = here.acquire("draining-a");
        here.lease_minted(&live, "file").await;
        let live_epoch = here.epoch("draining-a").expect("leased");
        assert!(
            here.keeps("draining-a", live_epoch),
            "kept before the drain"
        );
        let racing = here.acquire("draining-b");
        let racing_epoch = store
            .lease_transcode_session(
                "draining-b",
                "file",
                super::lease_until(super::now_unix_secs()),
            )
            .await
            .unwrap()
            .expect("leased before the drain");

        here.drain().await;

        assert!(
            take_over(&store, "draining-a", user).await.is_some(),
            "the lease is let go while the grant still lives"
        );
        assert!(here.is_held("draining-a"), "the local hold stays");
        assert!(
            !here.keeps("draining-a", live_epoch),
            "but no renewal keeps a session once the drain began"
        );
        let refused = here
            .attach(&racing, racing_epoch, None)
            .await
            .expect_err("a lease attached after the drain");
        assert_eq!(refused.outcome, ResumeOutcome::Live);
        assert_eq!(
            racing.epoch(),
            Some(racing_epoch),
            "the refused grant still keeps its epoch, so its stop stays fenced"
        );
        assert!(!here.is_renewing("draining-b"), "but it is not renewed");
        assert!(
            take_over(&store, "draining-b", user).await.is_some(),
            "and let go again"
        );
        assert!(here.reserve("draining-c").is_err(), "no renewal is kept");
        let minted = here.acquire("draining-c");
        here.lease_minted(&minted, "file").await;
        assert_eq!(minted.epoch(), None, "and no new lease is taken");
        drop((live, racing, minted));
    }

    /// Every claim and lease outcome is counted once under its own label.
    /// Disarm check: with either `increment(1)` removed, its counter is
    /// missing.
    #[test]
    fn every_claim_and_lease_outcome_is_counted() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _guard = metrics::set_default_local_recorder(&recorder);
        for o in ClaimOutcome::ALL {
            o.record("counted-session", "test");
        }
        for o in LeaseOutcome::ALL {
            o.record();
        }
        let counted = snapshotter.snapshot().into_vec();
        let count = |name: &str, label: &str| {
            counted
                .iter()
                .filter(|(k, ..)| {
                    k.key().name() == name
                        && k.key()
                            .labels()
                            .any(|l| l.key() == "outcome" && l.value() == label)
                })
                .map(|(.., v)| matches!(v, DebugValue::Counter(1)))
                .collect::<Vec<_>>()
        };
        for o in ClaimOutcome::ALL {
            assert_eq!(
                count("pharos_play_session_claim_total", o.label()),
                vec![true],
                "{}",
                o.label()
            );
        }
        for o in LeaseOutcome::ALL {
            assert_eq!(
                count("pharos_play_session_lease_total", o.label()),
                vec![true],
                "{}",
                o.label()
            );
        }
    }

    /// `pharos_play_session_claim_total{outcome}` and
    /// `pharos_play_session_lease_total{outcome}` are dashboard contracts.
    #[test]
    fn claim_and_lease_labels_are_distinct() {
        let mut claims: Vec<_> = ClaimOutcome::ALL.iter().map(|o| o.label()).collect();
        claims.sort_unstable();
        claims.dedup();
        assert_eq!(claims.len(), ClaimOutcome::ALL.len());
        let mut leases: Vec<_> = LeaseOutcome::ALL.iter().map(|o| o.label()).collect();
        leases.sort_unstable();
        leases.dedup();
        assert_eq!(leases.len(), LeaseOutcome::ALL.len());
    }
}
