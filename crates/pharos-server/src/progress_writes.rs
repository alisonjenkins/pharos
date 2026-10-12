//! Decides which playback-progress reports reach the database.
//!
//! A playing client reports its position every ~10 s, and each report used to
//! be a database write. `POST /Sessions/Playing/Progress` is the busiest route
//! in production and its latency tail was those writes stalling. Nothing is
//! lost by writing less often: the resume position only has to be right when
//! someone resumes, and a position 15 s stale is invisible.
//!
//! This keeps, per `(user, item, play session)`, when the row was last written
//! and the newest report not yet written. A report is written at once when
//! the write is the first for the session, the pause state flipped, the
//! position jumped (a seek), or the interval has elapsed; otherwise it is held
//! as `pending`. `take_due` (a background tick) writes a held report once it
//! is an interval old, so a client that goes silent without sending Stopped
//! still has its last position stored within about two intervals. `take_all`
//! drains everything for shutdown.
//!
//! The state is per process. A crash or SIGKILL loses at most the held report
//! of each live session, i.e. up to one interval of progress.
//!
//! Only the database write is deferred. Session registry updates and socket
//! fan-out stay on the request path.

use pharos_core::{MediaId, PlaybackProgress, UserDataStore, UserId};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Default seconds between writes of one session's position: above the ~10 s
/// report cadence so a steady client is written every other report, short
/// enough that the lost tail of a crash is invisible on resume.
pub const DEFAULT_INTERVAL_SECS: u64 = 15;

/// Jellyfin position ticks per second (100 ns units).
const TICKS_PER_SEC: u64 = 10_000_000;

/// A forward step larger than `FORWARD_SLACK_FACTOR` times the wall time since
/// the previous report, plus `SEEK_SLACK`, is a seek. 2x covers a client
/// playing at double speed.
const FORWARD_SLACK_FACTOR: u64 = 2;
/// Tolerance on top of expected motion, for report jitter and buffering.
const SEEK_SLACK: Duration = Duration::from_secs(5);
/// Playing backwards by more than this is a seek, not jitter.
const BACKWARD_SLACK: Duration = Duration::from_secs(2);

/// Tracked sessions above which new ones are written through untracked: the
/// bound never drops a held report, it only stops coalescing.
const DEFAULT_MAX_ENTRIES: usize = 10_000;
/// A session with nothing held and no report for this long is forgotten.
const DEFAULT_IDLE_TTL: Duration = Duration::from_secs(30 * 60);
/// One held-report write is given up on after this long, so one stalled
/// connection cannot hold the rest of a flush hostage.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Held reports written at the same time by a flush.
const FLUSH_CONCURRENCY: usize = 8;
/// The shutdown flush stops after this long. Chart default: the pod gets 60 s
/// to stop, of which `drainGraceSecs` (10) and actix's graceful stop (30) run
/// before the flush; 10 s keeps the sum under it.
pub const SHUTDOWN_FLUSH_DEADLINE: Duration = Duration::from_secs(10);
/// A full table is swept for idle sessions at most this often.
const MIN_SWEEP_GAP: Duration = Duration::from_secs(1);
/// Longest `PlaySessionId` kept verbatim in a tracking key. The id is chosen
/// by the client; a longer one is replaced by a fixed-size digest so the
/// table's memory is bounded by its entry count, not by request content.
const MAX_SESSION_KEY_BYTES: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProgressKey {
    pub user: UserId,
    pub item: MediaId,
    /// The client's `PlaySessionId`; empty when it sent none.
    pub session: String,
}

impl ProgressKey {
    /// Build a key from the client's `PlaySessionId` (`None` when absent). An
    /// id over `MAX_SESSION_KEY_BYTES` becomes `#` and a 64-bit digest of it.
    /// A collision only merges the coalescing of two sessions of one user and
    /// item, which costs at most an extra or a missed early write.
    pub fn new(user: UserId, item: MediaId, session: Option<&str>) -> Self {
        use std::hash::{Hash, Hasher};
        let session = session.unwrap_or_default();
        let session = if session.len() > MAX_SESSION_KEY_BYTES {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            session.hash(&mut h);
            format!("#{:016x}", h.finish())
        } else {
            session.to_owned()
        };
        Self {
            user,
            item,
            session,
        }
    }
}

/// Why a report is being written now. A bounded label set for metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistReason {
    /// First report seen for this session.
    First,
    /// The write interval has elapsed.
    Interval,
    /// The client paused or resumed.
    PauseChange,
    /// The position jumped.
    Seek,
    /// Not tracked: coalescing is off or the tracking table is full.
    Uncoalesced,
}

impl PersistReason {
    pub fn label(self) -> &'static str {
        match self {
            PersistReason::First => "first",
            PersistReason::Interval => "interval",
            PersistReason::PauseChange => "pause_change",
            PersistReason::Seek => "seek",
            PersistReason::Uncoalesced => "uncoalesced",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Write it now; call [`ProgressCoalescer::persisted`] once it landed.
    Persist(PersistReason),
    /// Held; a later report, the background tick or shutdown will write it.
    Coalesce,
}

#[derive(Debug)]
struct Entry {
    last_persist: Instant,
    last_seen: Instant,
    last_position_ticks: u64,
    last_paused: bool,
    pending: Option<PlaybackProgress>,
}

#[derive(Debug)]
pub struct ProgressCoalescer {
    interval: Duration,
    max_entries: usize,
    idle_ttl: Duration,
    entries: Mutex<HashMap<ProgressKey, Entry>>,
    /// When the table was last swept for idle sessions. Only touched while
    /// `entries` is locked.
    last_sweep: Mutex<Option<Instant>>,
}

impl ProgressCoalescer {
    /// `interval` of zero turns coalescing off: every report is written.
    pub fn new(interval: Duration) -> Self {
        Self::with_limits(interval, DEFAULT_MAX_ENTRIES, DEFAULT_IDLE_TTL)
    }

    pub fn with_limits(interval: Duration, max_entries: usize, idle_ttl: Duration) -> Self {
        Self {
            interval,
            max_entries,
            idle_ttl,
            entries: Mutex::new(HashMap::new()),
            last_sweep: Mutex::new(None),
        }
    }

    /// Forget idle sessions. `rate_limited` skips the pass when one ran in
    /// the last `MIN_SWEEP_GAP`, so a full table does not cost every new
    /// session an O(n) scan under the lock. Returns whether it swept.
    fn sweep(
        &self,
        entries: &mut HashMap<ProgressKey, Entry>,
        now: Instant,
        rate_limited: bool,
    ) -> bool {
        let mut last = self
            .last_sweep
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if rate_limited && last.is_some_and(|t| now.saturating_duration_since(t) < MIN_SWEEP_GAP) {
            return false;
        }
        *last = Some(now);
        sweep_idle(entries, now, self.idle_ttl);
        true
    }

    /// How often [`flush_due`] should run, or `None` when nothing is ever
    /// held. A third of the interval bounds the lag on top of it to ~33%.
    pub fn flush_period(&self) -> Option<Duration> {
        if self.interval.is_zero() {
            None
        } else {
            Some((self.interval / 3).max(Duration::from_secs(1)))
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<ProgressKey, Entry>> {
        // The map holds plain data; a panic elsewhere cannot leave it torn.
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Decide what to do with one report. The report is remembered either way.
    pub fn admit(
        &self,
        key: &ProgressKey,
        now: Instant,
        is_paused: bool,
        progress: PlaybackProgress,
    ) -> Decision {
        if self.interval.is_zero() {
            return Decision::Persist(PersistReason::Uncoalesced);
        }
        let mut entries = self.lock();
        let Some(entry) = entries.get_mut(key) else {
            if entries.len() >= self.max_entries {
                self.sweep(&mut entries, now, true);
                if entries.len() >= self.max_entries {
                    return Decision::Persist(PersistReason::Uncoalesced);
                }
            }
            entries.insert(
                key.clone(),
                Entry {
                    last_persist: now,
                    last_seen: now,
                    last_position_ticks: progress.position_ticks,
                    last_paused: is_paused,
                    pending: None,
                },
            );
            return Decision::Persist(PersistReason::First);
        };

        let reason = if is_paused != entry.last_paused {
            Some(PersistReason::PauseChange)
        } else if is_seek(entry, now, progress.position_ticks) {
            Some(PersistReason::Seek)
        } else if now.saturating_duration_since(entry.last_persist) >= self.interval {
            Some(PersistReason::Interval)
        } else {
            None
        };
        entry.last_seen = now;
        entry.last_position_ticks = progress.position_ticks;
        entry.last_paused = is_paused;
        match reason {
            Some(r) => {
                // The caller writes this one now. Holding it too would let
                // the background tick write it a second time; a caller whose
                // write fails hands it back with `hold`. It also supersedes
                // anything older that was held.
                entry.pending = None;
                Decision::Persist(r)
            }
            None => {
                entry.pending = Some(progress);
                Decision::Coalesce
            }
        }
    }

    /// `written` reached the database. A newer report that arrived while the
    /// write was in flight stays held.
    pub fn persisted(&self, key: &ProgressKey, now: Instant, written: &PlaybackProgress) {
        if let Some(entry) = self.lock().get_mut(key) {
            entry.last_persist = now;
            if entry.pending.as_ref() == Some(written) {
                entry.pending = None;
            }
        }
    }

    /// The session ended (Stopped). Drops what is held: the stop writes the
    /// final state itself, and a held older position must not follow it.
    pub fn forget(&self, key: &ProgressKey) {
        self.lock().remove(key);
    }

    /// Held reports that are at least an interval old, removed from the hold.
    /// Also forgets sessions that have been silent for the idle TTL with
    /// nothing held. A caller whose write fails hands the report back with
    /// [`ProgressCoalescer::hold`].
    pub fn take_due(&self, now: Instant) -> Vec<(ProgressKey, PlaybackProgress)> {
        let mut entries = self.lock();
        let mut due = Vec::new();
        for (key, entry) in entries.iter_mut() {
            if entry.pending.is_some()
                && now.saturating_duration_since(entry.last_persist) >= self.interval
            {
                if let Some(p) = entry.pending.take() {
                    due.push((key.clone(), p));
                }
            }
        }
        self.sweep(&mut entries, now, false);
        due
    }

    /// Every held report, for shutdown.
    pub fn take_all(&self) -> Vec<(ProgressKey, PlaybackProgress)> {
        self.lock()
            .iter_mut()
            .filter_map(|(key, entry)| entry.pending.take().map(|p| (key.clone(), p)))
            .collect()
    }

    /// Put a report back after a failed write, unless a newer one arrived.
    pub fn hold(&self, key: &ProgressKey, progress: PlaybackProgress) {
        if let Some(entry) = self.lock().get_mut(key) {
            entry.pending.get_or_insert(progress);
        }
    }

    /// Sessions currently tracked.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Write the held reports that are an interval old. A session that went
/// silent without a Stopped (a crashed player, a closed tab) would otherwise
/// keep its last held position out of the database. A failed write is handed
/// back to be retried on the next tick. Returns how many were written.
pub async fn flush_due<S: UserDataStore>(
    coalescer: &ProgressCoalescer,
    store: &S,
    now: Instant,
) -> usize {
    let written = AtomicUsize::new(0);
    write_held(
        coalescer,
        store,
        coalescer.take_due(now),
        now,
        "tick",
        &written,
    )
    .await;
    written.into_inner()
}

/// Write every held report, for shutdown, within [`SHUTDOWN_FLUSH_DEADLINE`].
/// Returns how many were written.
pub async fn flush_all<S: UserDataStore>(coalescer: &ProgressCoalescer, store: &S) -> usize {
    flush_all_within(coalescer, store, SHUTDOWN_FLUSH_DEADLINE)
        .await
        .written
}

/// What a shutdown flush did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlushOutcome {
    pub written: usize,
    /// Reports still unwritten when the deadline hit or a write failed.
    pub abandoned: usize,
}

/// Write every held report, giving up after `deadline` so a stalled database
/// cannot keep the pod from exiting before SIGKILL. Writes run a few at a time
/// and each is bounded by [`WRITE_TIMEOUT`]. What did not make it is counted
/// in `abandoned`.
pub async fn flush_all_within<S: UserDataStore>(
    coalescer: &ProgressCoalescer,
    store: &S,
    deadline: Duration,
) -> FlushOutcome {
    let written = AtomicUsize::new(0);
    let held = coalescer.take_all();
    let flush = write_held(coalescer, store, held, Instant::now(), "shutdown", &written);
    if tokio::time::timeout(deadline, flush).await.is_err() {
        tracing::warn!(
            ?deadline,
            "shutdown flush of held playback progress hit its deadline"
        );
    }
    // Cancelled and failed writes were put back by `Taken`.
    FlushOutcome {
        written: written.into_inner(),
        abandoned: coalescer.take_all().len(),
    }
}

/// A report that was not written now, for `pharos_progress_writes_total`.
pub fn count_held() {
    metrics::counter!("pharos_progress_writes_total", "outcome" => "coalesced", "reason" => "held")
        .increment(1);
}

/// One database write of a progress report, timed and counted.
/// `pharos_progress_writes_total{outcome,reason}` splits the reports into
/// written (and why), held and failed; `pharos_progress_persist_seconds` is
/// the write's latency, the tail the route's p99 came from.
///
/// `held` marks a report written late (the background tick, shutdown): the
/// store then drops it when the row already holds something at least as new,
/// and the write is counted as `dropped_stale` rather than `persisted`.
pub async fn persist<S: UserDataStore>(
    store: &S,
    key: &ProgressKey,
    progress: PlaybackProgress,
    reason: &'static str,
    held: bool,
) -> pharos_core::DomainResult<pharos_core::UserItemData> {
    let started = Instant::now();
    let sent = PlaybackProgress {
        only_if_newer: held,
        ..progress
    };
    let result = store
        .record_playback_progress(key.user, key.item, sent)
        .await;
    metrics::histogram!("pharos_progress_persist_seconds").record(started.elapsed().as_secs_f64());
    let outcome = match &result {
        Err(_) => "failed",
        // The store returns the stored row when it drops the report.
        Ok(row)
            if held
                && (row.last_played_at != progress.at
                    || row.last_played_position_ticks != progress.position_ticks) =>
        {
            "dropped_stale"
        }
        Ok(_) => "persisted",
    };
    metrics::counter!("pharos_progress_writes_total", "outcome" => outcome, "reason" => reason)
        .increment(1);
    result
}

/// A report taken out of the hold that has not been written yet. Dropping it
/// without [`Taken::written`] (the write failed, timed out, or the whole
/// flush was cancelled mid-way) puts the report back in the hold, so a taken
/// report is never lost between `take_due`/`take_all` and the database.
struct Taken<'a> {
    coalescer: &'a ProgressCoalescer,
    key: ProgressKey,
    progress: PlaybackProgress,
    done: bool,
}

impl Taken<'_> {
    fn mark_written(&mut self) {
        self.done = true;
    }
}

impl Drop for Taken<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.coalescer.hold(&self.key, self.progress);
        }
    }
}

async fn write_held<S: UserDataStore>(
    coalescer: &ProgressCoalescer,
    store: &S,
    held: Vec<(ProgressKey, PlaybackProgress)>,
    now: Instant,
    reason: &'static str,
    written: &AtomicUsize,
) {
    use futures_util::StreamExt;
    // Every report is wrapped before the first await, so cancelling this
    // future drops the not yet started ones as well and they go back too.
    let batch: Vec<Taken<'_>> = held
        .into_iter()
        .map(|(key, progress)| Taken {
            coalescer,
            key,
            progress,
            done: false,
        })
        .collect();
    futures_util::stream::iter(batch)
        .for_each_concurrent(FLUSH_CONCURRENCY, |mut taken| async move {
            let write = persist(store, &taken.key, taken.progress, reason, true);
            match tokio::time::timeout(WRITE_TIMEOUT, write).await {
                Ok(Ok(_)) => {
                    coalescer.persisted(&taken.key, now, &taken.progress);
                    taken.mark_written();
                    written.fetch_add(1, Ordering::Relaxed);
                }
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, item = taken.key.item, "held playback progress not written");
                }
                Err(_) => {
                    metrics::counter!(
                        "pharos_progress_writes_total",
                        "outcome" => "failed",
                        "reason" => reason
                    )
                    .increment(1);
                    tracing::warn!(item = taken.key.item, "held playback progress write timed out");
                }
            }
        })
        .await;
}

fn sweep_idle(entries: &mut HashMap<ProgressKey, Entry>, now: Instant, idle_ttl: Duration) {
    entries.retain(|_, e| {
        e.pending.is_some() || now.saturating_duration_since(e.last_seen) < idle_ttl
    });
}

fn ticks(d: Duration) -> u64 {
    u64::try_from(d.as_millis())
        .unwrap_or(u64::MAX)
        .saturating_mul(TICKS_PER_SEC / 1000)
}

/// Did the position move by more than playback explains since the previous
/// report? Unpaused playback advances about 1x (up to 2x), paused not at all.
fn is_seek(entry: &Entry, now: Instant, position_ticks: u64) -> bool {
    if position_ticks.saturating_add(ticks(BACKWARD_SLACK)) < entry.last_position_ticks {
        return true;
    }
    let elapsed = ticks(now.saturating_duration_since(entry.last_seen));
    let allowed = if entry.last_paused {
        0
    } else {
        elapsed.saturating_mul(FORWARD_SLACK_FACTOR)
    }
    .saturating_add(ticks(SEEK_SLACK));
    position_ticks.saturating_sub(entry.last_position_ticks) > allowed
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    const SEC: u64 = TICKS_PER_SEC;

    fn key(session: &str) -> ProgressKey {
        ProgressKey {
            user: UserId(uuid::Uuid::from_u128(1)),
            item: 7,
            session: session.into(),
        }
    }

    fn report(position_secs: u64, at: i64) -> PlaybackProgress {
        PlaybackProgress {
            position_ticks: position_secs * SEC,
            at,
            audio_stream_index: None,
            subtitle_stream_index: None,
            only_if_newer: false,
        }
    }

    fn coalescer() -> (ProgressCoalescer, Instant) {
        (
            ProgressCoalescer::new(Duration::from_secs(15)),
            Instant::now(),
        )
    }

    fn at(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    /// Admit a report and, when told to write, confirm the write.
    fn tick(
        c: &ProgressCoalescer,
        k: &ProgressKey,
        t: Instant,
        secs: u64,
        paused: bool,
    ) -> Decision {
        let d = c.admit(k, t, paused, report(secs, secs as i64));
        if matches!(d, Decision::Persist(_)) {
            c.persisted(k, t, &report(secs, secs as i64));
        }
        d
    }

    #[test]
    fn first_report_is_written_and_the_next_one_inside_the_interval_is_held() {
        let (c, t0) = coalescer();
        let k = key("s");
        assert_eq!(
            tick(&c, &k, t0, 100, false),
            Decision::Persist(PersistReason::First)
        );
        assert_eq!(tick(&c, &k, at(t0, 10), 110, false), Decision::Coalesce);
    }

    #[test]
    fn the_interval_elapsing_writes_the_held_report() {
        let (c, t0) = coalescer();
        let k = key("s");
        tick(&c, &k, t0, 100, false);
        tick(&c, &k, at(t0, 10), 110, false);
        assert_eq!(
            tick(&c, &k, at(t0, 20), 120, false),
            Decision::Persist(PersistReason::Interval)
        );
        assert_eq!(tick(&c, &k, at(t0, 30), 130, false), Decision::Coalesce);
    }

    #[test]
    fn pausing_and_resuming_each_write_at_once() {
        let (c, t0) = coalescer();
        let k = key("s");
        tick(&c, &k, t0, 100, false);
        assert_eq!(
            tick(&c, &k, at(t0, 2), 102, true),
            Decision::Persist(PersistReason::PauseChange)
        );
        assert_eq!(tick(&c, &k, at(t0, 4), 102, true), Decision::Coalesce);
        assert_eq!(
            tick(&c, &k, at(t0, 6), 102, false),
            Decision::Persist(PersistReason::PauseChange)
        );
    }

    #[test]
    fn a_position_jump_writes_at_once_in_either_direction() {
        let (c, t0) = coalescer();
        let k = key("s");
        tick(&c, &k, t0, 100, false);
        assert_eq!(
            tick(&c, &k, at(t0, 2), 3_000, false),
            Decision::Persist(PersistReason::Seek)
        );
        assert_eq!(
            tick(&c, &k, at(t0, 4), 50, false),
            Decision::Persist(PersistReason::Seek)
        );
    }

    #[test]
    fn ordinary_and_double_speed_playback_is_not_a_seek() {
        let (c, t0) = coalescer();
        let k = key("s");
        tick(&c, &k, t0, 100, false);
        assert_eq!(tick(&c, &k, at(t0, 10), 110, false), Decision::Coalesce);
        assert_eq!(tick(&c, &k, at(t0, 11), 116, false), Decision::Coalesce);
    }

    #[test]
    fn a_move_while_paused_is_a_seek() {
        let (c, t0) = coalescer();
        let k = key("s");
        tick(&c, &k, t0, 100, true);
        assert_eq!(
            tick(&c, &k, at(t0, 3), 160, true),
            Decision::Persist(PersistReason::Seek)
        );
    }

    #[test]
    fn sessions_are_tracked_independently() {
        let (c, t0) = coalescer();
        tick(&c, &key("a"), t0, 100, false);
        assert_eq!(
            tick(&c, &key("b"), at(t0, 1), 100, false),
            Decision::Persist(PersistReason::First)
        );
    }

    #[test]
    fn zero_interval_writes_every_report_and_tracks_nothing() {
        let c = ProgressCoalescer::new(Duration::ZERO);
        let t0 = Instant::now();
        let k = key("s");
        for i in 0..3 {
            assert_eq!(
                c.admit(&k, at(t0, i), false, report(i, 0)),
                Decision::Persist(PersistReason::Uncoalesced)
            );
        }
        assert!(c.is_empty());
    }

    #[test]
    fn held_report_comes_due_after_an_interval_and_only_once() {
        let (c, t0) = coalescer();
        let k = key("s");
        tick(&c, &k, t0, 100, false);
        tick(&c, &k, at(t0, 10), 110, false);
        assert!(c.take_due(at(t0, 14)).is_empty(), "not an interval old yet");
        let due = c.take_due(at(t0, 15));
        assert_eq!(due, vec![(k.clone(), report(110, 110))]);
        assert!(c.take_due(at(t0, 40)).is_empty(), "already taken");
    }

    #[test]
    fn a_failed_write_can_be_handed_back_but_never_over_a_newer_report() {
        let (c, t0) = coalescer();
        let k = key("s");
        tick(&c, &k, t0, 100, false);
        tick(&c, &k, at(t0, 10), 110, false);
        let (key_due, p) = c.take_due(at(t0, 15)).remove(0);
        c.hold(&key_due, p);
        assert_eq!(c.take_all(), vec![(k.clone(), report(110, 110))]);

        tick(&c, &k, at(t0, 20), 120, false);
        c.admit(&k, at(t0, 21), false, report(121, 121));
        c.hold(&k, report(110, 110));
        assert_eq!(c.take_all(), vec![(k, report(121, 121))]);
    }

    #[test]
    fn a_report_written_now_is_not_also_left_for_the_tick() {
        let (c, t0) = coalescer();
        let k = key("s");
        tick(&c, &k, t0, 100, false);
        // Held, then the interval elapses: this report is written by its
        // request and supersedes the held one.
        c.admit(&k, at(t0, 10), false, report(110, 110));
        assert_eq!(
            c.admit(&k, at(t0, 20), false, report(120, 120)),
            Decision::Persist(PersistReason::Interval)
        );
        assert!(
            c.take_due(at(t0, 40)).is_empty(),
            "the tick must not write the same report again"
        );
        assert!(c.take_all().is_empty());
    }

    #[test]
    fn a_report_whose_write_failed_is_held_for_the_tick() {
        let (c, t0) = coalescer();
        let k = key("s");
        tick(&c, &k, t0, 100, false);
        let p = report(120, 120);
        assert_eq!(
            c.admit(&k, at(t0, 20), false, p),
            Decision::Persist(PersistReason::Interval)
        );
        // The write failed: nothing was confirmed, the caller hands it back.
        c.hold(&k, p);
        assert_eq!(c.take_due(at(t0, 40)), vec![(k, p)]);
    }

    #[test]
    fn confirming_an_older_write_does_not_clear_a_newer_held_report() {
        let (c, t0) = coalescer();
        let k = key("s");
        tick(&c, &k, t0, 100, false);
        let older = report(110, 110);
        c.admit(&k, at(t0, 10), false, older);
        c.admit(&k, at(t0, 11), false, report(111, 111));
        c.persisted(&k, at(t0, 12), &older);
        assert_eq!(c.take_all(), vec![(k, report(111, 111))]);
    }

    #[test]
    fn flush_period_is_a_third_of_the_interval_and_absent_when_off() {
        assert_eq!(
            ProgressCoalescer::new(Duration::from_secs(15)).flush_period(),
            Some(Duration::from_secs(5))
        );
        assert_eq!(
            ProgressCoalescer::new(Duration::from_secs(1)).flush_period(),
            Some(Duration::from_secs(1))
        );
        assert_eq!(ProgressCoalescer::new(Duration::ZERO).flush_period(), None);
    }

    #[test]
    fn forgetting_a_session_drops_what_was_held() {
        let (c, t0) = coalescer();
        let k = key("s");
        tick(&c, &k, t0, 100, false);
        tick(&c, &k, at(t0, 10), 110, false);
        c.forget(&k);
        assert!(c.take_all().is_empty());
        assert_eq!(
            tick(&c, &k, at(t0, 20), 120, false),
            Decision::Persist(PersistReason::First)
        );
    }

    #[test]
    fn take_all_drains_every_session() {
        let (c, t0) = coalescer();
        for s in ["a", "b", "c"] {
            tick(&c, &key(s), t0, 100, false);
            tick(&c, &key(s), at(t0, 10), 110, false);
        }
        assert_eq!(c.take_all().len(), 3);
        assert!(c.take_all().is_empty());
    }

    #[test]
    fn a_full_table_writes_through_instead_of_dropping_held_reports() {
        let c =
            ProgressCoalescer::with_limits(Duration::from_secs(15), 2, Duration::from_secs(600));
        let t0 = Instant::now();
        tick(&c, &key("a"), t0, 100, false);
        tick(&c, &key("a"), at(t0, 10), 110, false);
        tick(&c, &key("b"), t0, 100, false);
        assert_eq!(
            c.admit(&key("c"), at(t0, 11), false, report(5, 5)),
            Decision::Persist(PersistReason::Uncoalesced)
        );
        assert_eq!(c.len(), 2, "the table did not grow past its bound");
        assert_eq!(c.take_all(), vec![(key("a"), report(110, 110))]);
    }

    #[test]
    fn idle_sessions_with_nothing_held_make_room() {
        let c =
            ProgressCoalescer::with_limits(Duration::from_secs(15), 1, Duration::from_secs(600));
        let t0 = Instant::now();
        tick(&c, &key("a"), t0, 100, false);
        assert_eq!(
            tick(&c, &key("b"), at(t0, 601), 100, false),
            Decision::Persist(PersistReason::First)
        );
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn an_oversized_session_id_is_replaced_by_a_fixed_size_digest() {
        let user = UserId(uuid::Uuid::from_u128(1));
        let long_a = "a".repeat(100_000);
        let long_b = "b".repeat(100_000);
        let ka = ProgressKey::new(user, 7, Some(&long_a));
        assert!(ka.session.len() <= 17, "bounded, got {}", ka.session.len());
        assert_eq!(ka, ProgressKey::new(user, 7, Some(&long_a)), "stable");
        assert_ne!(ka, ProgressKey::new(user, 7, Some(&long_b)));
        let at_limit = "c".repeat(MAX_SESSION_KEY_BYTES);
        assert_eq!(
            ProgressKey::new(user, 7, Some(&at_limit)).session,
            at_limit,
            "an id within the limit is kept verbatim"
        );
        assert_eq!(ProgressKey::new(user, 7, None).session, "");
    }

    #[test]
    fn a_full_table_is_swept_at_most_once_a_second() {
        // Idle after 100 ms, so a session can be idle again right after a sweep.
        let c =
            ProgressCoalescer::with_limits(Duration::from_secs(15), 1, Duration::from_millis(100));
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        tick(&c, &key("a"), t0, 100, false);
        // The first full-table admit sweeps: a is idle, so x gets its room.
        assert_eq!(
            tick(&c, &key("x"), ms(200), 100, false),
            Decision::Persist(PersistReason::First)
        );
        // x is idle by now, but a sweep ran 300 ms ago: no second scan, the
        // new session is written through untracked.
        assert_eq!(
            c.admit(&key("y"), ms(500), false, report(1, 1)),
            Decision::Persist(PersistReason::Uncoalesced)
        );
        assert_eq!(c.len(), 1);
        // Once the gap has passed the sweep runs again and frees x.
        assert_eq!(
            tick(&c, &key("z"), ms(1_300), 100, false),
            Decision::Persist(PersistReason::First)
        );
    }

    #[test]
    fn persist_reason_labels_are_distinct() {
        let all = [
            PersistReason::First,
            PersistReason::Interval,
            PersistReason::PauseChange,
            PersistReason::Seek,
            PersistReason::Uncoalesced,
        ];
        let labels: std::collections::HashSet<_> = all.iter().map(|r| r.label()).collect();
        assert_eq!(labels.len(), all.len());
    }
}
