//! Live scheduled-task progress registry — the server-side state behind
//! pharos's Jellyfin-compatible `ScheduledTasks` surface.
//!
//! jellyfin-web (10.11.x) drives a library scan by starting the `RefreshLibrary`
//! scheduled task (`POST /ScheduledTasks/Running/{id}`), then renders a live
//! progress bar from `TaskInfo.State` + `CurrentProgressPercentage` — polled via
//! `GET /ScheduledTasks` and pushed over `/socket` as `ScheduledTasksInfo`. This
//! registry is the single source of truth those three surfaces read: a scan
//! `try_start`s its task, streams `set_progress` from the scanner's
//! [`pharos_scanner::ScanProgress`] callback, and `finish`es with a terminal
//! [`CompletionStatus`]. Idle tasks report their last run so the panel shows a
//! "last ran / result" line.
//!
//! Only `RefreshLibrary` is actively driven today; the trickplay + subtitle
//! pre-generators are advertised (so the panel lists them) but run on their own
//! internal schedules and stay `Idle` here until wired to drive the registry.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// A root a coalesced re-run scans, and whether any request for it asked for
/// a forced re-probe.
pub type QueuedRoot = (PathBuf, bool);

/// Group a pass's roots by `force`, forced first, so the pass builds one
/// scanner (and so one prober worker pool) per group rather than per root. A
/// root inside another queued root is dropped when that root's walk already
/// covers it: the outer one is forced, or neither is.
#[must_use]
pub fn plan_pass(roots: &[QueuedRoot]) -> Vec<(bool, Vec<PathBuf>)> {
    let covered = |(root, force): &QueuedRoot| {
        roots.iter().any(|(outer, outer_force)| {
            outer != root && root.starts_with(outer) && (*outer_force || !*force)
        })
    };
    [true, false]
        .into_iter()
        .filter_map(|group| {
            let members: Vec<PathBuf> = roots
                .iter()
                .filter(|q| q.1 == group && !covered(q))
                .map(|(root, _)| root.clone())
                .collect();
            (!members.is_empty()).then_some((group, members))
        })
        .collect()
}

/// Task id for the media-library scan. Jellyfin advertises this task under
/// `Key == "RefreshLibrary"`; jellyfin-web's "Scan All Libraries" button looks
/// it up by that key and starts it.
pub const TASK_REFRESH_LIBRARY: &str = "refresh-library";
/// Trickplay sprite pre-generator task id.
pub const TASK_TRICKPLAY: &str = "trickplay-images";
/// Embedded-subtitle pre-extraction task id.
pub const TASK_EXTRACT_SUBTITLES: &str = "extract-subtitles";

/// Every task pharos advertises, in the order the dashboard lists them.
pub const ALL_TASK_IDS: [&str; 3] = [TASK_REFRESH_LIBRARY, TASK_TRICKPLAY, TASK_EXTRACT_SUBTITLES];

/// Jellyfin `TaskState` — the SDK enum defines exactly these three members
/// (confirmed against the deployed jellyfin-web bundle).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RunState {
    Idle,
    Cancelling,
    Running,
}

impl RunState {
    /// The exact wire string jellyfin-web compares against (`State !== "Idle"`
    /// keeps the "Scan" button spinning).
    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            RunState::Idle => "Idle",
            RunState::Cancelling => "Cancelling",
            RunState::Running => "Running",
        }
    }
}

/// Terminal status of the last run — Jellyfin `TaskCompletionStatus`. Only the
/// values jellyfin-web branches on (`Failed`/`Cancelled`, else success).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CompletionStatus {
    Completed,
    Failed,
    Cancelled,
}

impl CompletionStatus {
    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            CompletionStatus::Completed => "Completed",
            CompletionStatus::Failed => "Failed",
            CompletionStatus::Cancelled => "Cancelled",
        }
    }
}

/// How one scan pass ended: the `outcome` label of `pharos_scan_total`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScanOutcome {
    /// Every root scanned.
    Completed,
    /// Some roots scanned and at least one failed.
    Partial,
    /// Every root failed.
    Failed,
    /// Stopped by a cancel request.
    Cancelled,
}

impl ScanOutcome {
    pub const ALL: [ScanOutcome; 4] = [
        ScanOutcome::Completed,
        ScanOutcome::Partial,
        ScanOutcome::Failed,
        ScanOutcome::Cancelled,
    ];

    #[must_use]
    pub fn of(cancelled: bool, roots: usize, failed_roots: usize) -> Self {
        if cancelled {
            ScanOutcome::Cancelled
        } else if failed_roots == 0 {
            ScanOutcome::Completed
        } else if failed_roots >= roots {
            ScanOutcome::Failed
        } else {
            ScanOutcome::Partial
        }
    }

    /// The `TaskProgress.state` a pass that ended this way is sent with.
    #[must_use]
    pub fn task_state(self) -> &'static str {
        match self {
            ScanOutcome::Completed => "done",
            ScanOutcome::Partial => "partial",
            ScanOutcome::Failed => "failed",
            ScanOutcome::Cancelled => "cancelled",
        }
    }

    /// Stable metric label; dashboards and alerts key on these strings.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            ScanOutcome::Completed => "completed",
            ScanOutcome::Partial => "partial",
            ScanOutcome::Failed => "failed",
            ScanOutcome::Cancelled => "cancelled",
        }
    }
}

/// What one scan pass did, summed over its roots.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct ScanTally {
    pub roots: usize,
    pub failed_roots: usize,
    pub added: usize,
    pub changed: usize,
    pub removed: usize,
}

impl ScanTally {
    #[must_use]
    pub fn outcome(&self, cancelled: bool) -> ScanOutcome {
        ScanOutcome::of(cancelled, self.roots, self.failed_roots)
    }
}

/// Counts the pass in `pharos_scan_total{outcome}` and writes its end line,
/// failures and all, so a scan that found nothing can be told from one that
/// read nothing.
pub fn record_scan_end(tally: &ScanTally, outcome: ScanOutcome, elapsed: std::time::Duration) {
    metrics::counter!("pharos_scan_total", "outcome" => outcome.label()).increment(1);
    let ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
    if outcome == ScanOutcome::Completed || outcome == ScanOutcome::Cancelled {
        tracing::info!(
            outcome = outcome.label(),
            roots = tally.roots,
            failed_roots = tally.failed_roots,
            added = tally.added,
            changed = tally.changed,
            removed = tally.removed,
            elapsed_ms = ms,
            "scan: finished"
        );
    } else {
        tracing::warn!(
            outcome = outcome.label(),
            roots = tally.roots,
            failed_roots = tally.failed_roots,
            added = tally.added,
            changed = tally.changed,
            removed = tally.removed,
            elapsed_ms = ms,
            "scan: finished"
        );
    }
}

/// Result of the most recent completed run of a task — feeds
/// `TaskInfo.LastExecutionResult`.
#[derive(Clone, Copy, Debug)]
pub struct LastRun {
    pub start_ms: i64,
    pub end_ms: i64,
    pub status: CompletionStatus,
}

/// Immutable per-task view handed to the DTO layer (HTTP `GET /ScheduledTasks`
/// and the `ScheduledTasksInfo` socket push share it).
#[derive(Clone, Debug)]
pub struct TaskSnapshot {
    pub id: &'static str,
    pub state: RunState,
    /// Percent `0.0..=100.0` while `Running`; `None` when `Idle`/`Cancelling`
    /// with no active pass.
    pub progress: Option<f64>,
    pub last: Option<LastRun>,
}

struct Runtime {
    state: RunState,
    progress: Option<f64>,
    last: Option<LastRun>,
    started_ms: i64,
    /// Cancellation flag handed to the running job. Replaced with a fresh flag
    /// on each `try_start` so a stale cancel never bleeds into the next run.
    cancel: Arc<AtomicBool>,
    /// B100 — a re-run was requested (a second `/Library/Refresh`) while this
    /// task was already Running. Rather than start a concurrent scan, the
    /// running job re-runs exactly once more on completion; further requests
    /// during the run coalesce into this single pending flag.
    pending: bool,
    /// The roots every request coalesced into `pending` asked for, each with the
    /// OR of its requests' `force`. The re-run scans exactly these, so a forced
    /// one-folder item refresh queued behind another scan re-probes that folder
    /// and nothing else, rather than growing into a forced whole-library pass.
    pending_roots: Vec<QueuedRoot>,
}

impl Default for Runtime {
    fn default() -> Self {
        Self {
            state: RunState::Idle,
            progress: None,
            last: None,
            started_ms: 0,
            cancel: Arc::new(AtomicBool::new(false)),
            pending: false,
            pending_roots: Vec::new(),
        }
    }
}

/// Registry of every advertised scheduled task's live run-state. Cheap to
/// `Arc`-share; all mutation goes through a single `Mutex` (updates are
/// coarse — one per integer-percent step, plus start/finish).
pub struct ScanTasks {
    inner: Mutex<HashMap<&'static str, Runtime>>,
}

impl Default for ScanTasks {
    fn default() -> Self {
        Self::new()
    }
}

impl ScanTasks {
    #[must_use]
    pub fn new() -> Self {
        let mut map = HashMap::new();
        for id in ALL_TASK_IDS {
            map.insert(id, Runtime::default());
        }
        Self {
            inner: Mutex::new(map),
        }
    }

    /// Transition a task to `Running` at 0%. Returns the run's cancellation
    /// flag on success, or `None` if the id is unknown or the task is already
    /// active (guards against a double-start racing two scans of the same
    /// library). The caller polls the returned flag to honour a later
    /// [`request_cancel`](Self::request_cancel).
    pub fn try_start(&self, id: &str) -> Option<Arc<AtomicBool>> {
        let mut map = self.inner.lock().ok()?;
        let rt = map.get_mut(id)?;
        if rt.state != RunState::Idle {
            return None;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        rt.state = RunState::Running;
        rt.progress = Some(0.0);
        rt.started_ms = now_ms();
        rt.cancel = cancel.clone();
        Some(cancel)
    }

    /// B100 — start the task if it is `Idle`, otherwise fold this request into a
    /// single pending re-run and return `None`. This is the scan entry point:
    /// many concurrent `/Library/Refresh`s collapse to **one** running scan plus
    /// **at most one** queued re-run (over the union of their roots, `force`
    /// OR'd per root), instead of spawning N concurrent catalog walks — the
    /// stacked-scan NFS I/O storm that starved the node and stalled the control
    /// plane during the B98 recovery.
    /// A `Some(cancel)` return means the caller owns the run and must eventually
    /// call [`finish_or_rerun`](Self::finish_or_rerun); `None` means "already
    /// running, re-run queued — nothing to spawn".
    pub fn start_or_mark_pending(
        &self,
        id: &str,
        roots: &[PathBuf],
        force: bool,
    ) -> Option<Arc<AtomicBool>> {
        let mut map = self.inner.lock().ok()?;
        let rt = map.get_mut(id)?;
        if rt.state == RunState::Idle {
            let cancel = Arc::new(AtomicBool::new(false));
            rt.state = RunState::Running;
            rt.progress = Some(0.0);
            rt.started_ms = now_ms();
            rt.cancel = cancel.clone();
            rt.pending = false;
            rt.pending_roots.clear();
            Some(cancel)
        } else {
            rt.pending = true;
            for root in roots {
                match rt.pending_roots.iter_mut().find(|(r, _)| r == root) {
                    Some((_, f)) => *f |= force,
                    None => rt.pending_roots.push((root.clone(), force)),
                }
            }
            None
        }
    }

    /// B100 — end one scan pass. If a re-run was coalesced during the pass (and
    /// it was not cancelled), keep the task `Running` for another pass and return
    /// the fresh cancel flag and the queued `(root, force)` pairs to drive it; otherwise finish to `Idle`,
    /// recording the result, and return `None`. Holding the slot `Running` across
    /// the re-run leaves no `Idle` window in which a concurrent request could
    /// slip in and start a second scan. A cancelled pass never re-runs (an
    /// operator cancel must actually stop).
    pub fn finish_or_rerun(
        &self,
        id: &str,
        status: CompletionStatus,
    ) -> Option<(Arc<AtomicBool>, Vec<QueuedRoot>)> {
        let mut map = self.inner.lock().ok()?;
        let rt = map.get_mut(id)?;
        if rt.pending && status != CompletionStatus::Cancelled {
            let roots = std::mem::take(&mut rt.pending_roots);
            rt.pending = false;
            let cancel = Arc::new(AtomicBool::new(false));
            rt.state = RunState::Running;
            rt.progress = Some(0.0);
            rt.started_ms = now_ms();
            rt.cancel = cancel.clone();
            Some((cancel, roots))
        } else {
            rt.last = Some(LastRun {
                start_ms: rt.started_ms,
                end_ms: now_ms(),
                status,
            });
            rt.state = RunState::Idle;
            rt.progress = None;
            rt.pending = false;
            rt.pending_roots.clear();
            None
        }
    }

    /// Update a running task's percent (`0..=100`, clamped). No-op if the task
    /// is not currently running (a late callback after `finish`).
    pub fn set_progress(&self, id: &str, percent: f64) {
        if let Ok(mut map) = self.inner.lock() {
            if let Some(rt) = map.get_mut(id) {
                if rt.state == RunState::Running {
                    rt.progress = Some(percent.clamp(0.0, 100.0));
                }
            }
        }
    }

    /// Signal cancellation of a running task: flips it to `Cancelling` and sets
    /// its flag so the job stops at its next checkpoint. Returns `true` if a
    /// running task was signalled.
    pub fn request_cancel(&self, id: &str) -> bool {
        if let Ok(mut map) = self.inner.lock() {
            if let Some(rt) = map.get_mut(id) {
                if rt.state == RunState::Running {
                    rt.state = RunState::Cancelling;
                    rt.cancel.store(true, Ordering::SeqCst);
                    return true;
                }
            }
        }
        false
    }

    /// Complete a run: back to `Idle`, clear live progress, record the outcome
    /// (start/end/status) as the task's `LastExecutionResult`.
    pub fn finish(&self, id: &str, status: CompletionStatus) {
        if let Ok(mut map) = self.inner.lock() {
            if let Some(rt) = map.get_mut(id) {
                rt.last = Some(LastRun {
                    start_ms: rt.started_ms,
                    end_ms: now_ms(),
                    status,
                });
                rt.state = RunState::Idle;
                rt.progress = None;
            }
        }
    }

    /// Snapshot every task in dashboard order.
    #[must_use]
    pub fn snapshot(&self) -> Vec<TaskSnapshot> {
        let map = match self.inner.lock() {
            Ok(m) => m,
            Err(_) => return Vec::new(),
        };
        ALL_TASK_IDS
            .iter()
            .filter_map(|id| map.get(id).map(|rt| snapshot_of(id, rt)))
            .collect()
    }

    /// Snapshot a single task by id.
    #[must_use]
    pub fn snapshot_one(&self, id: &str) -> Option<TaskSnapshot> {
        let map = self.inner.lock().ok()?;
        // Recover the `'static` key so the snapshot can borrow it.
        let key = ALL_TASK_IDS.iter().find(|k| **k == id)?;
        map.get(*key).map(|rt| snapshot_of(key, rt))
    }
}

fn snapshot_of(id: &'static str, rt: &Runtime) -> TaskSnapshot {
    TaskSnapshot {
        id,
        state: rt.state,
        progress: rt.progress,
        last: rt.last,
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn scan_outcome_follows_root_failures() {
        assert_eq!(ScanOutcome::of(false, 3, 0), ScanOutcome::Completed);
        assert_eq!(ScanOutcome::of(false, 3, 1), ScanOutcome::Partial);
        assert_eq!(ScanOutcome::of(false, 3, 3), ScanOutcome::Failed);
        assert_eq!(ScanOutcome::of(true, 3, 3), ScanOutcome::Cancelled);
        assert_eq!(ScanOutcome::of(false, 0, 0), ScanOutcome::Completed);
    }

    #[test]
    fn scan_outcome_labels_are_distinct() {
        let mut labels: Vec<_> = ScanOutcome::ALL.iter().map(|o| o.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), ScanOutcome::ALL.len());
    }

    /// The counter is the dashboard contract. Fails if `record_scan_end`
    /// stops emitting it.
    #[test]
    fn record_scan_end_counts_each_outcome() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _guard = metrics::set_default_local_recorder(&recorder);
        let tally = ScanTally {
            roots: 2,
            failed_roots: 2,
            ..ScanTally::default()
        };
        record_scan_end(&tally, tally.outcome(false), std::time::Duration::ZERO);
        record_scan_end(
            &ScanTally::default(),
            ScanOutcome::Completed,
            std::time::Duration::ZERO,
        );
        let mut seen = Vec::new();
        for (ck, _, _, v) in snapshotter.snapshot().into_vec() {
            if ck.key().name() != "pharos_scan_total" {
                continue;
            }
            let outcome = ck
                .key()
                .labels()
                .find(|l| l.key() == "outcome")
                .map(|l| l.value().to_string())
                .unwrap_or_default();
            if let DebugValue::Counter(c) = v {
                seen.push((outcome, c));
            }
        }
        seen.sort();
        assert_eq!(seen, vec![("completed".into(), 1), ("failed".into(), 1)]);
    }

    #[test]
    fn start_progress_finish_cycle() {
        let tasks = ScanTasks::new();
        // Fresh registry: all idle, no progress, no last result.
        let s = tasks.snapshot();
        assert_eq!(s.len(), 3);
        assert!(s.iter().all(|t| t.state == RunState::Idle));
        assert!(s.iter().all(|t| t.progress.is_none() && t.last.is_none()));

        let cancel = tasks
            .try_start(TASK_REFRESH_LIBRARY)
            .expect("idle task starts");
        assert!(!cancel.load(Ordering::SeqCst));
        let one = tasks.snapshot_one(TASK_REFRESH_LIBRARY).unwrap();
        assert_eq!(one.state, RunState::Running);
        assert_eq!(one.progress, Some(0.0));

        tasks.set_progress(TASK_REFRESH_LIBRARY, 42.5);
        assert_eq!(
            tasks.snapshot_one(TASK_REFRESH_LIBRARY).unwrap().progress,
            Some(42.5)
        );

        tasks.finish(TASK_REFRESH_LIBRARY, CompletionStatus::Completed);
        let done = tasks.snapshot_one(TASK_REFRESH_LIBRARY).unwrap();
        assert_eq!(done.state, RunState::Idle);
        assert!(
            done.progress.is_none(),
            "running progress cleared on finish"
        );
        let last = done.last.expect("finish records a last run");
        assert_eq!(last.status, CompletionStatus::Completed);
        assert!(last.end_ms >= last.start_ms);
    }

    #[test]
    fn double_start_is_rejected() {
        let tasks = ScanTasks::new();
        assert!(tasks.try_start(TASK_REFRESH_LIBRARY).is_some());
        // Already running → a second start must not hand out a second flag.
        assert!(
            tasks.try_start(TASK_REFRESH_LIBRARY).is_none(),
            "an already-running task must not start again"
        );
    }

    #[test]
    fn unknown_task_never_starts() {
        let tasks = ScanTasks::new();
        assert!(tasks.try_start("no-such-task").is_none());
        assert!(tasks.snapshot_one("no-such-task").is_none());
    }

    #[test]
    fn cancel_flips_state_and_flag_then_records_cancelled() {
        let tasks = ScanTasks::new();
        let cancel = tasks.try_start(TASK_REFRESH_LIBRARY).unwrap();
        assert!(tasks.request_cancel(TASK_REFRESH_LIBRARY));
        assert!(cancel.load(Ordering::SeqCst), "job's flag is set");
        assert_eq!(
            tasks.snapshot_one(TASK_REFRESH_LIBRARY).unwrap().state,
            RunState::Cancelling
        );
        // Cancelling a task that isn't running is a no-op.
        assert!(!tasks.request_cancel(TASK_TRICKPLAY));

        tasks.finish(TASK_REFRESH_LIBRARY, CompletionStatus::Cancelled);
        let done = tasks.snapshot_one(TASK_REFRESH_LIBRARY).unwrap();
        assert_eq!(done.state, RunState::Idle);
        assert_eq!(done.last.unwrap().status, CompletionStatus::Cancelled);
    }

    #[test]
    fn a_fresh_start_resets_a_prior_cancel_flag() {
        let tasks = ScanTasks::new();
        let first = tasks.try_start(TASK_REFRESH_LIBRARY).unwrap();
        tasks.request_cancel(TASK_REFRESH_LIBRARY);
        tasks.finish(TASK_REFRESH_LIBRARY, CompletionStatus::Cancelled);
        assert!(first.load(Ordering::SeqCst));
        // Next run gets a distinct, un-cancelled flag.
        let second = tasks.try_start(TASK_REFRESH_LIBRARY).unwrap();
        assert!(
            !second.load(Ordering::SeqCst),
            "new run starts un-cancelled"
        );
    }

    // ---- B100: one scan at a time, concurrent requests coalesce ----

    #[test]
    fn concurrent_starts_coalesce_into_one_pending_rerun() {
        let t = ScanTasks::new();
        let id = TASK_REFRESH_LIBRARY;
        let lib = [PathBuf::from("/media/tv")];
        // First request wins the slot and runs.
        assert!(t.start_or_mark_pending(id, &lib, false).is_some());
        assert_eq!(t.snapshot_one(id).unwrap().state, RunState::Running);
        // Concurrent requests do NOT start a second scan — they coalesce.
        assert!(t.start_or_mark_pending(id, &lib, false).is_none());
        assert!(t.start_or_mark_pending(id, &lib, true).is_none()); // force OR'd in
                                                                    // Pass ends → exactly one re-run is queued, carrying the OR'd force.
        let (_c2, roots) = t
            .finish_or_rerun(id, CompletionStatus::Completed)
            .expect("a coalesced re-run must be pending");
        assert_eq!(
            roots,
            vec![(PathBuf::from("/media/tv"), true)],
            "one entry per root, force OR'd across the coalesced requests"
        );
        assert_eq!(
            t.snapshot_one(id).unwrap().state,
            RunState::Running,
            "task stays Running across the coalesced re-run (no Idle gap)"
        );
        // No further requests during the re-run → it finishes to Idle.
        assert!(t.finish_or_rerun(id, CompletionStatus::Completed).is_none());
        assert_eq!(t.snapshot_one(id).unwrap().state, RunState::Idle);
    }

    #[test]
    fn a_coalesced_item_refresh_reruns_only_its_own_folder() {
        let t = ScanTasks::new();
        let id = TASK_REFRESH_LIBRARY;
        let first = [PathBuf::from("/media/tv/Show.A.S01E03-GRP")];
        let second = [PathBuf::from("/media/tv/Show.B.S04E21-GRP")];
        let library = [PathBuf::from("/media/movies")];
        assert!(t.start_or_mark_pending(id, &first, true).is_some());
        assert!(t.start_or_mark_pending(id, &second, true).is_none());
        assert!(t.start_or_mark_pending(id, &library, false).is_none());
        let (_c, roots) = t
            .finish_or_rerun(id, CompletionStatus::Completed)
            .expect("re-run queued");
        // The forced folder stays forced; the incremental library scan queued
        // beside it stays incremental and nothing else is added.
        assert_eq!(
            roots,
            vec![
                (PathBuf::from("/media/tv/Show.B.S04E21-GRP"), true),
                (PathBuf::from("/media/movies"), false),
            ]
        );
    }

    #[test]
    fn a_pass_groups_roots_by_force_and_drops_covered_ones() {
        let q = |p: &str, f: bool| (PathBuf::from(p), f);
        let plan = plan_pass(&[
            q("/media/tv/Show.A-GRP", true),
            q("/media/tv", false),
            q("/media/tv/Show.B-GRP", false),
            q("/media/movies", true),
            q("/media/movies/Film (2001)", true),
        ]);
        // Show.A stays: an incremental walk of /media/tv would skip its
        // unchanged file. Show.B and Film are covered by their outer roots.
        assert_eq!(
            plan,
            vec![
                (
                    true,
                    vec![
                        PathBuf::from("/media/tv/Show.A-GRP"),
                        PathBuf::from("/media/movies")
                    ]
                ),
                (false, vec![PathBuf::from("/media/tv")]),
            ]
        );
        assert!(plan_pass(&[]).is_empty());
    }

    #[test]
    fn a_lone_run_with_no_pending_finishes_to_idle() {
        let t = ScanTasks::new();
        let id = TASK_REFRESH_LIBRARY;
        t.start_or_mark_pending(id, &[], false).unwrap();
        assert!(t.finish_or_rerun(id, CompletionStatus::Completed).is_none());
        let s = t.snapshot_one(id).unwrap();
        assert_eq!(s.state, RunState::Idle);
        assert_eq!(s.last.unwrap().status, CompletionStatus::Completed);
    }

    #[test]
    fn a_cancelled_pass_does_not_rerun_even_with_pending() {
        let t = ScanTasks::new();
        let id = TASK_REFRESH_LIBRARY;
        t.start_or_mark_pending(id, &[], false).unwrap();
        t.start_or_mark_pending(id, &[], false); // queue a re-run
                                                 // An operator cancel must actually stop — no coalesced re-run fires.
        assert!(t.finish_or_rerun(id, CompletionStatus::Cancelled).is_none());
        let s = t.snapshot_one(id).unwrap();
        assert_eq!(s.state, RunState::Idle);
        assert_eq!(s.last.unwrap().status, CompletionStatus::Cancelled);
    }
}
