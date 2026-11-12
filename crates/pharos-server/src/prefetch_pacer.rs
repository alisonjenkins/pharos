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
//! The pending map and the in-flight set belong to one walker task, spawned
//! when the pacer is created (V18: mutable runtime state is owned by exactly
//! one task and mutated by message passing). [`PrefetchPacer::retarget`] and
//! [`PrefetchPacer::extend`] only send it a message over an unbounded channel,
//! so the request handler that calls them never locks, waits or blocks. The
//! walker runs the in-flight set as a `FuturesUnordered`, and the pacer aborts
//! the walker when it is dropped, so every pending AND running prefetch future
//! dies with the registry entry that owns the pacer (the session-scoped
//! cancellation, see `prefetch_registry`).
//!
//! State per session is the pending map (at most the near window plus the burn
//! front-load, about 10 boxed futures) and an in-flight set of at most
//! [`MAX_PREFETCH_IN_FLIGHT`] segment numbers.

use futures_util::future::LocalBoxFuture;

/// A prefetch not yet started. A closure, not a future: the pacer lives in
/// shared state and is reached from every actix worker thread, while the
/// encode future itself is local to the worker that runs the walker.
pub type PrefetchJob = Box<dyn FnOnce() -> LocalBoxFuture<'static, ()> + Send>;

use futures_util::stream::FuturesUnordered;
use futures_util::StreamExt;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Prefetch encodes one play session may have running at once. Measured on the
/// Shield (2026-11-12, one viewer, VAAPI h264 CMAF): one encode took ~3.4 s for
/// a 6 s segment, 3 to 5 together took 4.5-7.6 s each. Two keep the GPU busy
/// (one finishing while the next starts) yet leave the client's own segment, an
/// interactive job outside this cap, sharing the device with at most two.
pub const MAX_PREFETCH_IN_FLIGHT: usize = 2;

/// How far past the client's segment a queued prefetch survives a retarget.
/// It must cover the deep subtitle-burn scan's reach (hls
/// `PREFETCH_BURN_HORIZON_SEGS`), else every request would discard the burn
/// jobs it front-loaded; it also bounds how long far-ahead work from before a
/// backward seek can linger.
pub const RETAIN_AHEAD_SEGS: u32 = 24;

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

enum Msg {
    Retarget {
        base: u32,
        jobs: Vec<(u32, PrefetchJob)>,
    },
    Extend {
        base: u32,
        jobs: Vec<(u32, PrefetchJob)>,
    },
}

/// One play session's prefetch queue; see the module docs. Dropping it aborts
/// the walker, and with it every pending and running prefetch.
pub struct PrefetchPacer {
    tx: mpsc::UnboundedSender<Msg>,
    walker: JoinHandle<()>,
    /// Jobs sent and not yet run to completion or dropped by the walker.
    outstanding: Arc<AtomicUsize>,
}

impl Drop for PrefetchPacer {
    fn drop(&mut self) {
        self.walker.abort();
    }
}

impl PrefetchPacer {
    /// Create a pacer and spawn its walker on the calling actix worker, so it
    /// must be called from inside the actix runtime.
    pub fn new(max_in_flight: usize, retain_ahead: u32) -> Arc<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        let outstanding = Arc::new(AtomicUsize::new(0));
        let walker = Walker {
            max_in_flight: max_in_flight.max(1),
            retain_ahead,
            pending: BTreeMap::new(),
            in_flight: BTreeSet::new(),
            outstanding: Arc::clone(&outstanding),
        };
        Arc::new(Self {
            tx,
            walker: actix_web::rt::spawn(walker.run(rx)),
            outstanding,
        })
    }

    /// Retarget the queue at `jobs` (segment, future), `base` being the segment
    /// the client asked for. Segments already running or already queued keep
    /// their place. Queued ones are dropped only when the client moved past
    /// them (behind `base`) or they lie more than the retain horizon ahead of
    /// it; anything between, such as a queued burn front-load, stays even when
    /// `jobs` omits it. Never blocks: it only hands the walker a message.
    pub fn retarget(&self, base: u32, jobs: Vec<(u32, PrefetchJob)>) {
        self.send(Msg::Retarget { base, jobs });
    }

    /// Add `jobs` to the wanted set without dropping anything (the deep
    /// subtitle-burn front-load, which arrives after the near window).
    pub fn extend(&self, base: u32, jobs: Vec<(u32, PrefetchJob)>) {
        self.send(Msg::Extend { base, jobs });
    }

    /// True once every job handed over has run or been dropped.
    pub fn is_idle(&self) -> bool {
        self.outstanding.load(Ordering::Acquire) == 0
    }

    fn send(&self, msg: Msg) {
        let n = match &msg {
            Msg::Retarget { jobs, .. } | Msg::Extend { jobs, .. } => jobs.len(),
        };
        self.outstanding.fetch_add(n, Ordering::AcqRel);
        if self.tx.send(msg).is_err() {
            self.outstanding.fetch_sub(n, Ordering::AcqRel);
            tracing::warn!(jobs = n, "prefetch walker is gone; jobs dropped");
        }
    }
}

/// The state only the walker task touches.
struct Walker {
    max_in_flight: usize,
    retain_ahead: u32,
    pending: BTreeMap<u32, Queued>,
    in_flight: BTreeSet<u32>,
    outstanding: Arc<AtomicUsize>,
}

impl Walker {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Msg>) {
        let mut running: FuturesUnordered<LocalBoxFuture<'static, u32>> = FuturesUnordered::new();
        loop {
            self.start_ready(&mut running);
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(msg) => self.apply(msg),
                    None => return,
                },
                Some(seg) = running.next() => {
                    self.in_flight.remove(&seg);
                    self.retire(1);
                }
            }
        }
    }

    fn retire(&self, n: usize) {
        if n > 0 {
            self.outstanding.fetch_sub(n, Ordering::AcqRel);
        }
    }

    fn superseded(&self, n: usize) {
        if n > 0 {
            metrics::counter!(
                "pharos_prefetch_paced_total",
                "outcome" => PacingOutcome::Superseded.label()
            )
            .increment(n as u64);
            self.retire(n);
        }
    }

    fn apply(&mut self, msg: Msg) {
        let (base, jobs) = match msg {
            Msg::Retarget { base, jobs } => {
                let last = base.saturating_add(self.retain_ahead);
                let before = self.pending.len();
                self.pending.retain(|seg, _| (base..=last).contains(seg));
                self.superseded(before - self.pending.len());
                (base, jobs)
            }
            Msg::Extend { base, jobs } => (base, jobs),
        };
        let now = Instant::now();
        for (seg, job) in jobs {
            if self.in_flight.contains(&seg) || self.pending.contains_key(&seg) {
                self.retire(1);
                continue;
            }
            self.pending.insert(
                seg,
                Queued {
                    job,
                    base,
                    queued_at: now,
                    deferred: false,
                },
            );
        }
    }

    fn start_ready(&mut self, running: &mut FuturesUnordered<LocalBoxFuture<'static, u32>>) {
        while running.len() < self.max_in_flight {
            let Some((seg, q)) = self.pending.pop_first() else {
                return;
            };
            let outcome = if q.deferred {
                PacingOutcome::Deferred
            } else {
                PacingOutcome::Started
            };
            let wait = q.queued_at.elapsed();
            metrics::counter!("pharos_prefetch_paced_total", "outcome" => outcome.label())
                .increment(1);
            metrics::histogram!("pharos_prefetch_slot_wait_seconds").record(wait.as_secs_f64());
            tracing::debug!(
                seg,
                distance_ahead = seg.saturating_sub(q.base),
                wait_ms = wait.as_millis() as u64,
                outcome = outcome.label(),
                in_flight = running.len(),
                "prefetch took a session slot"
            );
            self.in_flight.insert(seg);
            let job = (q.job)();
            running.push(Box::pin(async move {
                job.await;
                seg
            }));
        }
        for q in self.pending.values_mut() {
            q.deferred = true;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use futures_util::FutureExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;

    const TEST_RETAIN: u32 = 24;

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
            if pacer.is_idle() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("pacer did not drain");
    }

    #[actix_web::test]
    async fn runs_at_most_the_cap_at_once_nearest_first() {
        let pacer = PrefetchPacer::new(2, TEST_RETAIN);
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
        let pacer = PrefetchPacer::new(usize::MAX, TEST_RETAIN);
        let probe = Arc::new(Probe::default());
        pacer.retarget(0, (1..=6).map(|s| job(&probe, s, 30)).collect());
        idle(&pacer).await;
        assert_eq!(probe.peak.load(Ordering::SeqCst), 6);
    }

    #[actix_web::test]
    async fn a_later_request_dedupes_and_keeps_nearest_first() {
        let pacer = PrefetchPacer::new(2, TEST_RETAIN);
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
        let pacer = PrefetchPacer::new(1, TEST_RETAIN);
        let probe = Arc::new(Probe::default());
        pacer.retarget(0, (1..=4).map(|s| job(&probe, s, 40)).collect());
        tokio::time::sleep(Duration::from_millis(10)).await;
        // Seek far ahead: 2..=4 are no longer wanted.
        pacer.retarget(50, (51..=52).map(|s| job(&probe, s, 10)).collect());
        idle(&pacer).await;
        assert_eq!(*probe.started.lock().unwrap(), vec![1, 51, 52]);
    }

    /// Disarm: make retarget keep only the segments in its own set.
    #[actix_web::test]
    async fn a_queued_burn_job_ahead_of_the_client_survives_a_retarget() {
        let pacer = PrefetchPacer::new(1, TEST_RETAIN);
        let probe = Arc::new(Probe::default());
        pacer.retarget(0, vec![job(&probe, 1, 40)]);
        tokio::time::sleep(Duration::from_millis(10)).await;
        // The deep burn scan queues 10 behind the running 1...
        pacer.extend(0, vec![job(&probe, 10, 5)]);
        // ...and the next request's near window does not name it.
        pacer.retarget(1, vec![job(&probe, 2, 5)]);
        idle(&pacer).await;
        assert_eq!(*probe.started.lock().unwrap(), vec![1, 2, 10]);
    }

    /// Disarm: drop the `<= base + retain_ahead` bound from retarget.
    #[actix_web::test]
    async fn retarget_drops_a_queued_job_beyond_the_retain_horizon() {
        let pacer = PrefetchPacer::new(1, 5);
        let probe = Arc::new(Probe::default());
        pacer.retarget(0, vec![job(&probe, 1, 40)]);
        tokio::time::sleep(Duration::from_millis(10)).await;
        // 4 is within base + 5 = 6, 20 is far ahead of where the client is.
        pacer.extend(0, vec![job(&probe, 4, 5), job(&probe, 20, 5)]);
        pacer.retarget(1, vec![job(&probe, 2, 5)]);
        idle(&pacer).await;
        assert_eq!(*probe.started.lock().unwrap(), vec![1, 2, 4]);
    }

    #[actix_web::test]
    async fn enqueueing_never_waits_on_the_cap() {
        let pacer = PrefetchPacer::new(1, TEST_RETAIN);
        let probe = Arc::new(Probe::default());
        pacer.retarget(0, vec![job(&probe, 1, 100)]);
        tokio::time::sleep(Duration::from_millis(10)).await;
        // The saturated pacer takes more work synchronously, so the request
        // handler that calls it is never held up by prefetch.
        let t = Instant::now();
        pacer.extend(0, vec![job(&probe, 9, 1)]);
        assert!(t.elapsed() < Duration::from_millis(20));
    }

    /// Counts how many futures were dropped, finished or not.
    struct DropCount(Arc<AtomicUsize>);
    impl Drop for DropCount {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[actix_web::test]
    async fn dropping_the_pacer_aborts_running_and_pending_work() {
        let pacer = PrefetchPacer::new(1, TEST_RETAIN);
        let probe = Arc::new(Probe::default());
        let dropped = Arc::new(AtomicUsize::new(0));
        let jobs = (1..=3)
            .map(|seg| {
                let (probe, dropped) = (Arc::clone(&probe), Arc::clone(&dropped));
                let (_, inner) = job(&probe, seg, 200);
                let job: PrefetchJob = Box::new(move || {
                    let guard = DropCount(dropped);
                    let fut = inner();
                    async move {
                        fut.await;
                        drop(guard);
                    }
                    .boxed_local()
                });
                (seg, job)
            })
            .collect();
        pacer.retarget(0, jobs);
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(pacer);
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(probe.running.load(Ordering::SeqCst), 1, "1 never finished");
        assert_eq!(*probe.started.lock().unwrap(), vec![1], "2 and 3 never ran");
        assert_eq!(dropped.load(Ordering::SeqCst), 1, "1's future was dropped");
    }

    #[actix_web::test]
    async fn pacing_decisions_are_counted() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _guard = metrics::set_default_local_recorder(&recorder);

        let pacer = PrefetchPacer::new(1, TEST_RETAIN);
        let probe = Arc::new(Probe::default());
        pacer.retarget(0, (1..=3).map(|s| job(&probe, s, 20)).collect());
        tokio::time::sleep(Duration::from_millis(5)).await;
        // Client moved to 3: 2 is behind it and never runs.
        pacer.retarget(3, vec![job(&probe, 3, 1)]);
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
