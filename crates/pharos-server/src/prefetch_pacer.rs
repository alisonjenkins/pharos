//! Per-play-session pacing of speculative segment prefetch.
//!
//! hls.js asks for one segment at a time, so the only concurrency a single
//! viewer produces is the server's own read-ahead. Spawning every target of a
//! request at once put 3, rising to 5, encodes on the GPU together (measured on
//! the Shield, 2026-11-12, VAAPI h264 CMAF): each took 4.5-7.6 s against ~3.4 s
//! alone, and the segment the client needed next was one of them, so its
//! request coalesced onto a slow in-flight prefetch and waited 4.5 s.
//!
//! A [`PrefetchPacer`] holds one session's wanted segments and runs at most
//! [`MAX_PREFETCH_IN_FLIGHT`] of them at once, nearest segment first. Every
//! prefetch of the session goes through it, so successive requests share the
//! one ordering and the one cap. It never touches an interactive request: a
//! client's own segment goes straight to the segment cache, and the pacer only
//! ever sees speculative work.
//!
//! One walker task per pacer runs the in-flight set as a `FuturesUnordered`, so
//! aborting the walker drops every pending AND running prefetch future with it
//! (the registry's session-scoped cancellation, see `prefetch_registry`).
//!
//! State per session is the pending map (at most the near window plus the burn
//! front-load, about 10 boxed futures) and an in-flight set of at most
//! [`MAX_PREFETCH_IN_FLIGHT`] segment numbers; it is dropped with the
//! registry entry when the session stops or swaps media.

use futures_util::future::LocalBoxFuture;

/// A prefetch not yet started. A closure, not a future: the pacer lives in
/// shared state and is reached from every actix worker thread, while the
/// encode future itself is local to the worker that runs the walker.
pub type PrefetchJob = Box<dyn FnOnce() -> LocalBoxFuture<'static, ()> + Send>;

use futures_util::stream::FuturesUnordered;
use futures_util::StreamExt;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// Prefetch encodes one play session may have running at once. Measured on the
/// Shield (2026-11-12, one viewer, VAAPI h264 CMAF): one encode took ~3.4 s for
/// a 6 s segment, 3 to 5 together took 4.5-7.6 s each. Two keep the GPU busy
/// (one finishing while the next starts) yet leave the client's own segment, an
/// interactive job outside this cap, sharing the device with at most two.
pub const MAX_PREFETCH_IN_FLIGHT: usize = 2;

/// How a queued prefetch left the pacer. Labels are a dashboard contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacingOutcome {
    /// A slot was free when the segment's turn came.
    Started,
    /// The segment waited behind a full set of running prefetches first.
    Deferred,
    /// A newer request no longer wanted it, so it never ran.
    Superseded,
}

impl PacingOutcome {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Deferred => "deferred",
            Self::Superseded => "superseded",
        }
    }
}

struct Queued {
    job: PrefetchJob,
    base: u32,
    queued_at: Instant,
    /// Set once the walker found every slot busy while this was waiting.
    deferred: bool,
}

#[derive(Default)]
struct State {
    pending: BTreeMap<u32, Queued>,
    in_flight: BTreeSet<u32>,
    walking: bool,
}

/// One play session's prefetch queue; see the module docs.
pub struct PrefetchPacer {
    max_in_flight: usize,
    state: Mutex<State>,
    wake: Notify,
}

/// Clears `walking` if the walker task ends any way but a clean exit (an abort
/// or a panicking job), so a later request can start a fresh walker.
struct WalkGuard<'a> {
    pacer: &'a PrefetchPacer,
    clean: bool,
}

impl Drop for WalkGuard<'_> {
    fn drop(&mut self) {
        if !self.clean {
            self.pacer.lock().walking = false;
        }
    }
}

impl PrefetchPacer {
    pub fn new(max_in_flight: usize) -> Arc<Self> {
        Arc::new(Self {
            max_in_flight: max_in_flight.max(1),
            state: Mutex::new(State::default()),
            wake: Notify::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Replace the wanted set with `jobs` (segment, future), `base` being the
    /// segment the client asked for. Segments already running or already queued
    /// keep their place; queued ones absent from `jobs` are dropped, since the
    /// client moved on. Returns the walker's handle when this call started one,
    /// for the caller to register for cancellation.
    pub fn retarget(
        self: &Arc<Self>,
        base: u32,
        jobs: Vec<(u32, PrefetchJob)>,
    ) -> Option<JoinHandle<()>> {
        self.enqueue(base, jobs, true)
    }

    /// Add `jobs` to the wanted set without dropping anything (the deep
    /// subtitle-burn front-load, which arrives after the near window).
    pub fn extend(
        self: &Arc<Self>,
        base: u32,
        jobs: Vec<(u32, PrefetchJob)>,
    ) -> Option<JoinHandle<()>> {
        self.enqueue(base, jobs, false)
    }

    fn enqueue(
        self: &Arc<Self>,
        base: u32,
        jobs: Vec<(u32, PrefetchJob)>,
        replace: bool,
    ) -> Option<JoinHandle<()>> {
        let mut st = self.lock();
        if replace {
            let keep: BTreeSet<u32> = jobs.iter().map(|(s, _)| *s).collect();
            let before = st.pending.len();
            st.pending.retain(|seg, _| keep.contains(seg));
            let dropped = before - st.pending.len();
            if dropped > 0 {
                metrics::counter!(
                    "pharos_prefetch_paced_total",
                    "outcome" => PacingOutcome::Superseded.label()
                )
                .increment(dropped as u64);
            }
        }
        let now = Instant::now();
        for (seg, job) in jobs {
            if st.in_flight.contains(&seg) || st.pending.contains_key(&seg) {
                continue;
            }
            st.pending.insert(
                seg,
                Queued {
                    job,
                    base,
                    queued_at: now,
                    deferred: false,
                },
            );
        }
        if st.walking {
            drop(st);
            self.wake.notify_one();
            return None;
        }
        st.walking = true;
        drop(st);
        Some(actix_web::rt::spawn(Arc::clone(self).walk()))
    }

    async fn walk(self: Arc<Self>) {
        let mut guard = WalkGuard {
            pacer: &self,
            clean: false,
        };
        let mut running: FuturesUnordered<LocalBoxFuture<'static, u32>> = FuturesUnordered::new();
        loop {
            {
                let mut st = self.lock();
                while running.len() < self.max_in_flight {
                    let Some((seg, q)) = st.pending.pop_first() else {
                        break;
                    };
                    let outcome = if q.deferred {
                        PacingOutcome::Deferred
                    } else {
                        PacingOutcome::Started
                    };
                    let wait = q.queued_at.elapsed();
                    metrics::counter!("pharos_prefetch_paced_total", "outcome" => outcome.label())
                        .increment(1);
                    metrics::histogram!("pharos_prefetch_slot_wait_seconds")
                        .record(wait.as_secs_f64());
                    tracing::debug!(
                        seg,
                        distance_ahead = seg.saturating_sub(q.base),
                        wait_ms = wait.as_millis() as u64,
                        outcome = outcome.label(),
                        in_flight = running.len(),
                        "prefetch took a session slot"
                    );
                    st.in_flight.insert(seg);
                    let job = (q.job)();
                    running.push(Box::pin(async move {
                        job.await;
                        seg
                    }));
                }
                if running.is_empty() {
                    st.walking = false;
                    guard.clean = true;
                    return;
                }
                for q in st.pending.values_mut() {
                    q.deferred = true;
                }
            }
            tokio::select! {
                Some(seg) = running.next() => {
                    self.lock().in_flight.remove(&seg);
                }
                () = self.wake.notified() => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use futures_util::FutureExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Shared probe: start order, current and peak concurrency.
    #[derive(Default)]
    struct Probe {
        started: Mutex<Vec<u32>>,
        running: AtomicUsize,
        peak: AtomicUsize,
    }

    fn job(probe: &Arc<Probe>, seg: u32, ms: u64) -> (u32, PrefetchJob) {
        let probe = Arc::clone(probe);
        (
            seg,
            Box::new(move || {
                async move {
                    probe.started.lock().unwrap().push(seg);
                    let now = probe.running.fetch_add(1, Ordering::SeqCst) + 1;
                    probe.peak.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    probe.running.fetch_sub(1, Ordering::SeqCst);
                }
                .boxed_local()
            }),
        )
    }

    async fn idle(pacer: &PrefetchPacer) {
        for _ in 0..200 {
            if !pacer.lock().walking {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("pacer did not drain");
    }

    #[actix_web::test]
    async fn runs_at_most_the_cap_at_once_nearest_first() {
        let pacer = PrefetchPacer::new(2);
        let probe = Arc::new(Probe::default());
        // Handed over out of order: the pacer, not the caller, orders them.
        let jobs = [5, 3, 1, 4, 2, 6]
            .iter()
            .map(|s| job(&probe, *s, 30))
            .collect();
        pacer.retarget(0, jobs);
        idle(&pacer).await;
        assert_eq!(probe.peak.load(Ordering::SeqCst), 2, "never above the cap");
        assert_eq!(*probe.started.lock().unwrap(), vec![1, 2, 3, 4, 5, 6]);
    }

    /// Disarm check: with no cap all six run together.
    #[actix_web::test]
    async fn an_uncapped_pacer_would_run_them_all_together() {
        let pacer = PrefetchPacer::new(usize::MAX);
        let probe = Arc::new(Probe::default());
        pacer.retarget(0, (1..=6).map(|s| job(&probe, s, 30)).collect());
        idle(&pacer).await;
        assert_eq!(probe.peak.load(Ordering::SeqCst), 6);
    }

    #[actix_web::test]
    async fn a_later_request_dedupes_and_keeps_nearest_first() {
        let pacer = PrefetchPacer::new(2);
        let probe = Arc::new(Probe::default());
        pacer.retarget(0, (1..=4).map(|s| job(&probe, s, 40)).collect());
        // Client advanced one segment while 1 and 2 run, 3 and 4 wait.
        tokio::time::sleep(Duration::from_millis(10)).await;
        pacer.retarget(1, (2..=5).map(|s| job(&probe, s, 40)).collect());
        idle(&pacer).await;
        let started = probe.started.lock().unwrap().clone();
        assert_eq!(started, vec![1, 2, 3, 4, 5], "each segment once, in order");
        assert_eq!(probe.peak.load(Ordering::SeqCst), 2);
    }

    #[actix_web::test]
    async fn retarget_drops_queued_segments_the_client_moved_past() {
        let pacer = PrefetchPacer::new(1);
        let probe = Arc::new(Probe::default());
        pacer.retarget(0, (1..=4).map(|s| job(&probe, s, 40)).collect());
        tokio::time::sleep(Duration::from_millis(10)).await;
        // Seek far ahead: 2..=4 are no longer wanted.
        pacer.retarget(50, (51..=52).map(|s| job(&probe, s, 10)).collect());
        idle(&pacer).await;
        assert_eq!(*probe.started.lock().unwrap(), vec![1, 51, 52]);
    }

    #[actix_web::test]
    async fn enqueueing_never_waits_on_the_cap() {
        let pacer = PrefetchPacer::new(1);
        let probe = Arc::new(Probe::default());
        pacer.retarget(0, vec![job(&probe, 1, 100)]);
        tokio::time::sleep(Duration::from_millis(10)).await;
        // The saturated pacer takes more work synchronously, so the request
        // handler that calls it is never held up by prefetch.
        let t = Instant::now();
        pacer.extend(0, vec![job(&probe, 9, 1)]);
        assert!(t.elapsed() < Duration::from_millis(20));
    }

    #[actix_web::test]
    async fn aborting_the_walker_drops_running_and_pending_work() {
        let pacer = PrefetchPacer::new(1);
        let probe = Arc::new(Probe::default());
        let h = pacer
            .retarget(0, (1..=3).map(|s| job(&probe, s, 200)).collect())
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        h.abort();
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(probe.running.load(Ordering::SeqCst), 1, "1 never finished");
        assert_eq!(*probe.started.lock().unwrap(), vec![1], "2 and 3 never ran");
        // A later request starts a fresh walker.
        assert!(pacer.extend(0, vec![job(&probe, 7, 1)]).is_some());
    }

    #[actix_web::test]
    async fn pacing_decisions_are_counted() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _guard = metrics::set_default_local_recorder(&recorder);

        let pacer = PrefetchPacer::new(1);
        let probe = Arc::new(Probe::default());
        pacer.retarget(0, (1..=3).map(|s| job(&probe, s, 20)).collect());
        tokio::time::sleep(Duration::from_millis(5)).await;
        pacer.retarget(0, vec![job(&probe, 1, 1), job(&probe, 2, 1)]);
        idle(&pacer).await;

        let rows = snapshotter.snapshot().into_vec();
        let count = |outcome: &str| {
            rows.iter()
                .find_map(|(ck, _, _, v)| {
                    let k = ck.key();
                    let hit = k.name() == "pharos_prefetch_paced_total"
                        && k.labels()
                            .any(|l| l.key() == "outcome" && l.value() == outcome);
                    match (hit, v) {
                        (true, DebugValue::Counter(n)) => Some(*n),
                        _ => None,
                    }
                })
                .unwrap_or(0)
        };
        assert_eq!(count("started"), 1, "segment 1 took the free slot");
        assert_eq!(count("deferred"), 1, "segment 2 waited for it");
        assert_eq!(count("superseded"), 1, "segment 3 was dropped unrun");
        assert!(rows
            .iter()
            .any(|(ck, _, _, _)| ck.key().name() == "pharos_prefetch_slot_wait_seconds"));
    }
}
