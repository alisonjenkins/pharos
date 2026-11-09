//! On-demand deep checks: an admin asks for one file to be fully decoded, and
//! the answer is kept in memory for the admin page to show.
//!
//! Never in the background (Alison, 2026-11-09: "on demand only"). One check
//! runs at a time, behind a background-I/O permit so it stands down while
//! someone is watching (V34), with two decoder threads and a time bound.
//!
//! Results are not persisted. A deep check is a question an admin asks about a
//! file that plays badly, answered while they wait; losing the answers on a
//! restart costs one more click, and keeping them would mean a second memo
//! whose invalidation rules nobody needs yet.
//!
//! # Signals
//!
//! ```text
//! pharos_media_deep_check_total{outcome}   clean | damaged | failed
//! pharos_media_deep_check_seconds          per-file decode time
//! ```
//!
//! The log line for a finished check carries the file, the error count and
//! the decoder's own words for the first error.

use crate::bg_io::BgPermit;
use pharos_core::MediaItem;
use pharos_transcode::deep_check::{decode_check, DeepReport};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

/// Results kept, newest first. The page lists them; older ones are dropped.
pub const MAX_RESULTS: usize = 50;

/// The longest one decode may run. A 4K film at two threads decodes in well
/// under this; a file that takes longer is not one an admin is waiting on.
pub const DECODE_TIMEOUT: Duration = Duration::from_secs(3 * 3600);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeepState {
    Queued,
    Running,
    Clean,
    Damaged,
    Failed,
}

impl DeepState {
    /// Stable wire word and metric label.
    pub fn label(self) -> &'static str {
        match self {
            DeepState::Queued => "queued",
            DeepState::Running => "running",
            DeepState::Clean => "clean",
            DeepState::Damaged => "damaged",
            DeepState::Failed => "failed",
        }
    }

    fn in_progress(self) -> bool {
        matches!(self, DeepState::Queued | DeepState::Running)
    }
}

/// One requested check and, once it finishes, what it found.
#[derive(Debug, Clone)]
pub struct DeepEntry {
    pub id: u64,
    pub state: DeepState,
    pub report: DeepReport,
    /// Why a `Failed` check produced no verdict.
    pub detail: Option<String>,
    pub requested_at: i64,
    pub finished_at: Option<i64>,
    pub elapsed_secs: Option<u64>,
}

/// Whether a request was taken.
#[derive(Debug, PartialEq, Eq)]
pub enum Requested {
    Queued,
    AlreadyInProgress,
}

#[derive(Debug)]
pub struct DeepChecks {
    ffmpeg: String,
    bg_io: Arc<Semaphore>,
    /// One decode at a time, whatever the queue holds.
    one_at_a_time: Arc<Semaphore>,
    results: Mutex<VecDeque<DeepEntry>>,
}

impl DeepChecks {
    pub fn new(ffmpeg: impl Into<String>, bg_io: Arc<Semaphore>) -> Self {
        Self {
            ffmpeg: ffmpeg.into(),
            bg_io,
            one_at_a_time: Arc::new(Semaphore::new(1)),
            results: Mutex::new(VecDeque::new()),
        }
    }

    /// Every result, newest first.
    pub fn list(&self) -> Vec<DeepEntry> {
        self.results
            .lock()
            .map(|r| r.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Queue `item` for a full decode, unless one is already waiting or
    /// running for it.
    pub fn request(self: &Arc<Self>, item: &MediaItem) -> Requested {
        let id = item.id;
        {
            let Ok(mut r) = self.results.lock() else {
                return Requested::AlreadyInProgress;
            };
            if r.iter().any(|e| e.id == id && e.state.in_progress()) {
                return Requested::AlreadyInProgress;
            }
            r.push_front(DeepEntry {
                id,
                state: DeepState::Queued,
                report: DeepReport::default(),
                detail: None,
                requested_at: now_secs(),
                finished_at: None,
                elapsed_secs: None,
            });
            // Drop the oldest finished results past the cap; one still in
            // progress is never dropped, or its answer would have nowhere to go.
            while r.len() > MAX_RESULTS {
                let Some(pos) = r.iter().rposition(|e| !e.state.in_progress()) else {
                    break;
                };
                r.remove(pos);
            }
        }
        let this = Arc::clone(self);
        let path = item.path.clone();
        tokio::spawn(async move { this.run(id, path).await });
        Requested::Queued
    }

    async fn run(&self, id: u64, path: PathBuf) {
        let Ok(_turn) = self.one_at_a_time.acquire().await else {
            return;
        };
        let _permit = BgPermit::acquire(&self.bg_io).await;
        self.update(id, |e| e.state = DeepState::Running);
        let started = Instant::now();
        let outcome = decode_check(&self.ffmpeg, &path, DECODE_TIMEOUT).await;
        let elapsed = started.elapsed();
        metrics::histogram!("pharos_media_deep_check_seconds").record(elapsed.as_secs_f64());
        let (state, report, detail) = match outcome {
            Ok(r) if r.is_damaged() => (DeepState::Damaged, r, None),
            Ok(r) => (DeepState::Clean, r, None),
            Err(e) => (
                DeepState::Failed,
                DeepReport::default(),
                Some(e.to_string()),
            ),
        };
        metrics::counter!("pharos_media_deep_check_total", "outcome" => state.label()).increment(1);
        match state {
            DeepState::Damaged => tracing::warn!(
                item = id,
                path = %path.display(),
                errors = report.errors,
                first_error_ms = report.first_error_ms,
                first_error = report.first_error.as_deref().unwrap_or("unspecified"),
                decoded_ms = report.decoded_ms,
                elapsed_s = elapsed.as_secs_f64(),
                "deep check: decode errors found",
            ),
            DeepState::Failed => tracing::warn!(
                item = id,
                path = %path.display(),
                error = detail.as_deref().unwrap_or("unspecified"),
                elapsed_s = elapsed.as_secs_f64(),
                "deep check: could not run",
            ),
            _ => tracing::info!(
                item = id,
                path = %path.display(),
                decoded_ms = report.decoded_ms,
                elapsed_s = elapsed.as_secs_f64(),
                "deep check: clean",
            ),
        }
        self.update(id, |e| {
            e.state = state;
            e.report = report;
            e.detail = detail;
            e.finished_at = Some(now_secs());
            e.elapsed_secs = Some(elapsed.as_secs());
        });
    }

    /// Apply `f` to this id's in-progress entry.
    fn update(&self, id: u64, f: impl FnOnce(&mut DeepEntry)) {
        if let Ok(mut r) = self.results.lock() {
            if let Some(e) = r.iter_mut().find(|e| e.id == id && e.state.in_progress()) {
                f(e);
            }
        }
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn item(id: u64, path: &str) -> MediaItem {
        MediaItem {
            id,
            path: path.into(),
            ..Default::default()
        }
    }

    #[test]
    fn state_labels_are_distinct() {
        let all = [
            DeepState::Queued,
            DeepState::Running,
            DeepState::Clean,
            DeepState::Damaged,
            DeepState::Failed,
        ];
        let set: std::collections::HashSet<_> = all.iter().map(|s| s.label()).collect();
        assert_eq!(set.len(), all.len());
    }

    /// A second request while the first is in flight must not start a second
    /// decode of the same file.
    #[tokio::test]
    async fn a_file_is_decoded_once_at_a_time() {
        let checks = Arc::new(DeepChecks::new(
            "/nonexistent/ffmpeg",
            Arc::new(Semaphore::new(4)),
        ));
        // Hold the turn so the first request stays queued.
        let hold = checks.one_at_a_time.clone().acquire_owned().await.unwrap();
        assert_eq!(checks.request(&item(1, "/a.mkv")), Requested::Queued);
        assert_eq!(
            checks.request(&item(1, "/a.mkv")),
            Requested::AlreadyInProgress
        );
        assert_eq!(checks.request(&item(2, "/b.mkv")), Requested::Queued);
        assert_eq!(checks.list().len(), 2);
        assert!(checks.list().iter().all(|e| e.state == DeepState::Queued));
        drop(hold);
    }

    /// A check that cannot run says why, and the file can be asked about again.
    #[tokio::test]
    async fn a_check_that_cannot_run_is_failed_with_its_reason() {
        let checks = Arc::new(DeepChecks::new(
            "/nonexistent/ffmpeg",
            Arc::new(Semaphore::new(4)),
        ));
        checks.request(&item(1, "/a.mkv"));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let e = checks.list().into_iter().next().unwrap();
            if e.state == DeepState::Failed {
                assert!(e.detail.unwrap().contains("could not start"));
                assert!(e.finished_at.is_some());
                break;
            }
            assert!(Instant::now() < deadline, "the check never finished: {e:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(checks.request(&item(1, "/a.mkv")), Requested::Queued);
    }

    #[tokio::test]
    async fn old_results_are_dropped_but_in_progress_ones_are_kept() {
        let checks = Arc::new(DeepChecks::new(
            "/nonexistent/ffmpeg",
            Arc::new(Semaphore::new(4)),
        ));
        let hold = checks.one_at_a_time.clone().acquire_owned().await.unwrap();
        for id in 0..(MAX_RESULTS as u64 + 5) {
            checks.request(&item(id, "/x.mkv"));
        }
        assert_eq!(
            checks.list().len(),
            MAX_RESULTS + 5,
            "nothing has finished, so nothing may be dropped"
        );
        drop(hold);
    }
}
