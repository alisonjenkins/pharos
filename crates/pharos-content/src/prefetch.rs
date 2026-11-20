//! The background downloader behind the audio cache.
//!
//! One dedicated thread runs one download at a time, in the order the player
//! will need the tracks. A new [`Downloader::prefetch`] replaces the pending
//! list (the track already downloading carries on if it is still wanted), so
//! skipping ahead never queues stale work. Nothing here blocks a caller: the
//! public methods only touch a lock and wake the thread.
//!
//! Black spots: a network error, a stalled read or a 5xx is retried with
//! exponential backoff, resuming with `Range` when the server answers 206
//! and restarting when it does not (a live remux has no length to resume).
//! A 401 means the media token in the URL expired; the URL is asked for
//! again once, then the track is given up on until the next prefetch.
//! Downloading pauses while [`Downloader::set_online`] says `false`.
//!
//! Low disk: before a download and every [`FREE_CHECK_BYTES`] while it
//! writes, the cache must be able to keep [`super::cache::MIN_FREE_BYTES`]
//! free after the rest of the file lands (evicting old tracks if it must);
//! when it cannot, the track is skipped until the next prefetch.
//!
//! Every attempt logs `cache download id=.. bytes=.. ms=.. outcome=ok|retry|
//! failed|skipped reason=..`. URLs and tokens are never logged.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use super::cache::{Cache, NoRoom};
use super::error::ContentError;
use super::http::{self, Request, Url};

/// One request queues at most this many tracks (an album, not a library).
pub const MAX_TRACKS_PER_REQUEST: usize = 50;
/// One request downloads at most this many bytes; the track in flight when it
/// is reached still finishes.
pub const MAX_BYTES_PER_REQUEST: u64 = 1024 * 1024 * 1024;
/// Free space is read again after this many bytes are written: a statvfs
/// per 8 MiB costs nothing next to the download, and the guard cannot be
/// overrun by more than this between readings.
pub const FREE_CHECK_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Tuning {
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub max_tracks: usize,
    pub max_bytes: u64,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            // A tunnel is seconds to minutes; 2 s retries soon after a brief
            // drop, 30 s keeps a long black spot from burning battery.
            backoff_min: Duration::from_secs(2),
            backoff_max: Duration::from_secs(30),
            connect_timeout: http::CONNECT_TIMEOUT,
            read_timeout: http::READ_TIMEOUT,
            max_tracks: MAX_TRACKS_PER_REQUEST,
            max_bytes: MAX_BYTES_PER_REQUEST,
        }
    }
}

/// Why a stream URL could not be had.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// The network or server is unreachable right now: back off and retry.
    Transient(String),
    /// Asking again will not help (bad id, signed out, no stream).
    Permanent(String),
}

/// Fresh stream URL for a track id. Blocking; called off the async thread.
pub type Resolver = Arc<dyn Fn(&str) -> Result<String, ResolveError> + Send + Sync>;

struct Queue {
    /// Playback read-ahead: replaced by each [`Downloader::prefetch`].
    wanted: VecDeque<String>,
    /// Pin downloads: kept across prefetches, never spent from `budget`.
    pinned: VecDeque<String>,
    /// Where a pin that yielded to playback left off, so its resume still
    /// sends `If-Range` for the partial file it kept.
    deferred: HashMap<String, JobState>,
    /// Pins refused for low disk with a part kept: still in `pinned`, but
    /// not started again before `until`, so the worker moves on to other
    /// work meanwhile. `backoff` is the wait the next refusal takes.
    parked: HashMap<String, Parked>,
    generation: u64,
    budget: u64,
    busy: bool,
    /// The track `busy` is about: off the queues, so a request that lists it
    /// again must still see it as queued work.
    current: Option<String>,
    /// Pins of the track in flight that `enqueue` folded into its job. A
    /// job that gives up for good queues the pin again (`give_up`), under
    /// the same lock: one that failed before the pin landed would
    /// otherwise lose it.
    pin_requested: HashSet<String>,
}

struct Gate {
    cache: Arc<Cache>,
    resolve: Resolver,
    tuning: Tuning,
    online: AtomicBool,
    /// Bumped each time the network comes back, so backoff restarts at once.
    online_epoch: AtomicU64,
    closed: AtomicBool,
    /// Test signal: pins that yielded to playback.
    #[cfg(test)]
    yields: AtomicU64,
    /// Test signal: each backoff wait, in order.
    #[cfg(test)]
    waits: Mutex<Vec<Duration>>,
    /// Test signal: what jobs decided, in order (`parked:<id>`,
    /// `replacement_low_disk:<id>`, ...), for a test to assert an event
    /// rather than sample state a later attempt may change.
    #[cfg(test)]
    events: Mutex<Vec<String>>,
    /// Test hook: runs as a refused pin is about to be parked.
    #[cfg(test)]
    before_park: Mutex<Option<Box<dyn FnMut() + Send>>>,
    queue: Mutex<Queue>,
    idle: Condvar,
    wake: Notify,
}

pub struct Downloader {
    gate: Arc<Gate>,
    thread: Option<std::thread::JoinHandle<()>>,
}

enum Fail {
    Cancelled,
    /// Writing on would leave less than the minimum free; carries free bytes.
    LowDisk(u64),
    /// A whole new body would not fit over the partial file it replaces:
    /// the part and its validator stay, and the job tries again later.
    ReplacementLowDisk(u64),
    /// The cap shared with other downloads cannot be checked yet: wait and
    /// try again, keeping the partial file.
    AccountingUnknown,
    /// A replacement's old part could not be emptied: the part and its
    /// validator stay, and the job tries again later.
    PartNotTruncated,
    Unauthorized,
    Transient(String),
    Permanent(String),
}

impl Fail {
    fn no_room(no: NoRoom) -> Self {
        match no {
            NoRoom::LowDisk(room) => Self::LowDisk(room),
            NoRoom::AccountingUnknown => Self::AccountingUnknown,
            NoRoom::PartNotTruncated => Self::PartNotTruncated,
        }
    }
}

struct Parked {
    until: Instant,
    backoff: Duration,
    /// The network epoch it was parked in: a reconnect since makes it due.
    epoch: u64,
}

#[derive(Default, Clone)]
struct JobState {
    etag: Option<String>,
    total: Option<u64>,
    /// A 2xx arrived since the last reset: the URL's token was good.
    got_ok: bool,
    /// The body was admitted and its part opened since the last reset:
    /// progress, so the retry delay starts again from the minimum. A 2xx
    /// that is then refused for space is not.
    progressed: bool,
}

/// What a job that gave up does with its partial file.
enum PartFate {
    /// The bytes cannot be resumed: the part goes.
    Discard,
    /// The bytes are valid under this state's validator, and the job only
    /// stopped for want of space: the part stays, and the state is kept
    /// for the next job of the track to resume it with.
    Keep(JobState),
}

enum Outcome {
    Ok,
    Cancelled,
    LowDisk(u64),
    Failed(String),
    /// A retrying pin download stepped aside for playback read-ahead; the pin
    /// is queued again.
    Yielded,
    /// A pin refused for low disk with its part kept: parked with its
    /// state for this long, then tried again; the worker moves on.
    Parked(Duration),
    /// Read-ahead that cannot go on over its part (`why`): it ends with
    /// the part and its validator kept, and the next request resumes it.
    KeptPart(&'static str),
}

impl Downloader {
    pub fn start(
        cache: Arc<Cache>,
        resolve: Resolver,
        tuning: Tuning,
    ) -> Result<Self, ContentError> {
        let gate = Arc::new(Gate {
            cache,
            resolve,
            tuning,
            online: AtomicBool::new(true),
            online_epoch: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            #[cfg(test)]
            yields: AtomicU64::new(0),
            #[cfg(test)]
            waits: Mutex::new(Vec::new()),
            #[cfg(test)]
            events: Mutex::new(Vec::new()),
            #[cfg(test)]
            before_park: Mutex::new(None),
            queue: Mutex::new(Queue {
                wanted: VecDeque::new(),
                pinned: VecDeque::new(),
                deferred: HashMap::new(),
                parked: HashMap::new(),
                generation: 0,
                budget: 0,
                busy: false,
                current: None,
                pin_requested: HashSet::new(),
            }),
            idle: Condvar::new(),
            wake: Notify::new(),
        });
        let for_thread = gate.clone();
        let thread = std::thread::Builder::new()
            .name("pharos-cache-dl".into())
            .spawn(move || {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => {
                        rt.block_on(for_thread.run());
                        // A resolver call may still be waiting on the content
                        // worker; do not make a drop wait for it.
                        rt.shutdown_background();
                    }
                    Err(e) => log::error!("cache downloader: no runtime: {e}"),
                }
            })
            .map_err(|e| ContentError::Cache {
                op: "start the downloader".into(),
                why: e.to_string(),
            })?;
        Ok(Self {
            gate,
            thread: Some(thread),
        })
    }

    /// Replace the pending downloads with `ids`, current track first. Returns
    /// how many will actually be fetched (not already cached, within the cap).
    pub fn prefetch(&self, ids: &[String]) -> usize {
        let g = &self.gate;
        let mut seen = std::collections::HashSet::new();
        let unique: Vec<String> = ids
            .iter()
            .filter(|id| seen.insert((*id).clone()))
            .cloned()
            .collect();
        let kept: Vec<String> = unique.iter().take(g.tuning.max_tracks).cloned().collect();
        let over_cap = unique.len().saturating_sub(kept.len());
        g.cache.protect(&kept);
        let missing: VecDeque<String> = kept
            .iter()
            .filter(|id| !g.cache.is_complete(id))
            .cloned()
            .collect();
        let queued = missing.len();
        log::info!(
            "cache prefetch queued={queued} skipped_cached={} dropped_over_cap={over_cap}",
            kept.len().saturating_sub(queued)
        );
        {
            let mut q = g.lock();
            q.wanted = missing;
            q.generation = q.generation.wrapping_add(1);
            q.budget = g.tuning.max_bytes;
        }
        g.wake.notify_one();
        queued
    }

    /// Add `ids` behind the playback queue, for pins: the playback queue and
    /// its eviction protection are left alone, and a later [`Downloader::prefetch`]
    /// or the byte budget does not drop them. Returns how many were added.
    pub fn enqueue(&self, ids: &[String]) -> usize {
        let g = &self.gate;
        let mut q = g.lock();
        let mut added = 0usize;
        for id in ids {
            if g.cache.is_complete(id) || q.pinned.iter().any(|w| w == id) {
                continue;
            }
            // The track being downloaded is off both queues but is still
            // queued work. A pin of it is not cancelled by a newer request
            // (see `still_wanted`), so there is nothing to add; read-ahead
            // can be dropped, so a pin of it is still queued behind it.
            let in_flight = q.current.as_deref() == Some(id.as_str());
            if in_flight && g.is_pin_work(id) {
                // Folded into the running job, and remembered: if that job
                // has already failed for good, `give_up` queues it again.
                q.pin_requested.insert(id.clone());
                log::info!("cache enqueue id={id} verdict=folded reason=in_flight");
                continue;
            }
            // Kept even when read-ahead already lists it: the budget may drop
            // that entry, and the pin must still download.
            if !in_flight && !q.wanted.iter().any(|w| w == id) {
                added = added.saturating_add(1);
            }
            q.pinned.push_back(id.clone());
        }
        drop(q);
        log::info!("cache enqueue added={added} requested={}", ids.len());
        g.wake.notify_one();
        added
    }

    /// Queue every pinned track still missing, in container order, after a
    /// restart. Call once the online state is set: downloads wait for it.
    pub fn resume_pins(&self) -> usize {
        let missing = self.gate.cache.pending_pins();
        let n = self.enqueue(&missing);
        log::info!("cache resume pins missing={} queued={n}", missing.len());
        n
    }

    /// Pause (`false`) or resume (`true`) downloading, from the network
    /// callback. Resuming skips any backoff wait.
    pub fn set_online(&self, online: bool) {
        let was = self.gate.online.swap(online, Ordering::SeqCst);
        if online && !was {
            self.gate.online_epoch.fetch_add(1, Ordering::SeqCst);
        }
        if was != online {
            log::info!(
                "cache downloads {}",
                if online { "resumed" } else { "paused" }
            );
        }
        self.gate.wake.notify_one();
    }

    /// Whether downloading is allowed now.
    #[cfg(test)]
    pub fn is_online(&self) -> bool {
        self.gate.online.load(Ordering::SeqCst)
    }

    /// Whether state to resume `id`'s part is kept.
    #[cfg(test)]
    pub fn has_deferred(&self, id: &str) -> bool {
        self.gate.lock().deferred.contains_key(id)
    }

    /// Each backoff wait the jobs took, in order.
    #[cfg(test)]
    pub fn waits(&self) -> Vec<Duration> {
        self.gate
            .waits
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// What the jobs decided, in order (see `Gate::events`).
    #[cfg(test)]
    pub fn events(&self) -> Vec<String> {
        self.gate
            .events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// How many times a pin has yielded to playback.
    #[cfg(test)]
    pub fn yields(&self) -> u64 {
        self.gate.yields.load(Ordering::SeqCst)
    }

    /// The track the downloader is working on (or holding for the network).
    #[cfg(test)]
    pub fn in_flight(&self) -> Option<String> {
        self.gate.lock().current.clone()
    }

    /// Jobs queued or running.
    #[cfg(test)]
    pub fn pending(&self) -> usize {
        let q = self.gate.lock();
        q.wanted.len() + q.pinned.len() + usize::from(q.busy)
    }

    /// Wait until nothing is queued or downloading; false on timeout.
    #[cfg(test)]
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let q = self.gate.lock();
        let (q, res) = self
            .gate
            .idle
            .wait_timeout_while(q, timeout, |q| {
                q.busy || !q.wanted.is_empty() || !q.pinned.is_empty()
            })
            .unwrap_or_else(PoisonError::into_inner);
        drop(q);
        !res.timed_out()
    }
}

impl Drop for Downloader {
    fn drop(&mut self) {
        self.gate.closed.store(true, Ordering::SeqCst);
        self.gate.wake.notify_one();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Gate {
    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    async fn run(self: Arc<Self>) {
        while let Some((id, generation)) = self.next_job().await {
            self.job(&id, generation).await;
            let mut q = self.lock();
            self.job_over(&mut q, &id);
            if q.budget == 0 && !q.wanted.is_empty() {
                log::info!(
                    "cache prefetch stopped: byte budget spent, dropping {} queued",
                    q.wanted.len()
                );
                q.wanted.clear();
            }
            q.busy = false;
            q.current = None;
            self.idle.notify_all();
        }
    }

    async fn next_job(&self) -> Option<(String, u64)> {
        loop {
            let wait = self.wake.notified();
            tokio::pin!(wait);
            // Register before checking so a prefetch in between is not lost.
            wait.as_mut().enable();
            if self.closed() {
                return None;
            }
            let earliest = {
                let mut q = self.lock();
                let now = Instant::now();
                let epoch = self.online_epoch.load(Ordering::SeqCst);
                while let Some(id) = q.wanted.pop_front() {
                    if self.cache.is_complete(&id) {
                        continue;
                    }
                    // A parked pin waits out its park here too: its retry
                    // stays scheduled in `pinned`, and other playback
                    // tracks go meanwhile.
                    // A park whose pin is gone delays nothing: it is
                    // dropped, and the track goes as playback work.
                    if q.parked.contains_key(&id) && !self.is_pin_work(&id) {
                        q.parked.remove(&id);
                        log::info!(
                            "cache download id={id} verdict=dispatched reason=park_cleared_unpinned queue=wanted"
                        );
                        #[cfg(test)]
                        self.event(format!("park_cleared:{id}"));
                    }
                    if let Some(p) = q.parked.get(&id) {
                        if p.epoch == epoch && p.until > now {
                            log::info!(
                                "cache download id={id} verdict=skipped reason=parked_not_due queue=wanted left_ms={}",
                                p.until.saturating_duration_since(now).as_millis()
                            );
                            if !q.pinned.iter().any(|x| *x == id) {
                                q.pinned.push_back(id);
                            }
                            continue;
                        }
                    }
                    q.busy = true;
                    q.current = Some(id.clone());
                    return Some((id, q.generation));
                }
                // A parked pin waits out its backoff at the back of the
                // queue; the others go meanwhile.
                let mut earliest: Option<Instant> = None;
                for _ in 0..q.pinned.len() {
                    let Some(id) = q.pinned.pop_front() else {
                        break;
                    };
                    if self.cache.is_complete(&id) || !self.is_pin_work(&id) {
                        q.parked.remove(&id);
                        continue;
                    }
                    // A reconnect since the park makes it due at once.
                    let parked = q.parked.get(&id).map(|p| (p.until, p.epoch));
                    if let Some((until, at)) = parked.filter(|&(u, at)| at != epoch && u > now) {
                        log::info!(
                            "cache download id={id} park verdict=ended reason=reconnect parked_epoch={at} epoch={epoch} left_ms={}",
                            until.saturating_duration_since(now).as_millis()
                        );
                    }
                    if let Some((until, _)) = parked.filter(|&(_, at)| at == epoch) {
                        if until > now {
                            earliest = Some(earliest.map_or(until, |e| e.min(until)));
                            q.pinned.push_back(id);
                            continue;
                        }
                    }
                    q.busy = true;
                    q.current = Some(id.clone());
                    return Some((id, q.generation));
                }
                q.busy = false;
                q.current = None;
                self.idle.notify_all();
                earliest
            };
            match earliest {
                Some(at) => tokio::select! {
                    () = tokio::time::sleep_until(tokio::time::Instant::from_std(at)) => {}
                    () = &mut wait => {}
                },
                None => wait.await,
            }
        }
    }

    /// `id` belongs to a pinned container that still needs it.
    /// The job for `id` is over: a pin folded into it was served if the
    /// track is whole now. One whose track is still missing (a trim evicted
    /// it after its commit, or the job stopped short) is queued again, so
    /// the pin is never lost to the fold; a track that survived is not
    /// fetched twice. Under the queue lock.
    fn job_over(&self, q: &mut Queue, id: &str) {
        if !q.pin_requested.remove(id) {
            return;
        }
        if self.cache.is_complete(id) || !self.is_pin_work(id) {
            return;
        }
        if !q.pinned.iter().any(|p| p == id) {
            q.pinned.push_back(id.to_owned());
        }
        log::info!("cache download id={id} verdict=requeued reason=pinned_and_missing_after_job");
        self.wake.notify_one();
    }

    fn is_pin_work(&self, id: &str) -> bool {
        self.cache.entry(id).is_some_and(|e| e.pinned)
    }

    /// True while `id` is still wanted by the latest request. A newer request
    /// that still lists `id` adopts the running download.
    fn still_wanted(&self, id: &str, generation: &mut u64) -> bool {
        if self.closed() {
            return false;
        }
        // The verdict and the release of `current` are one step under the
        // queue lock. A pin that arrives after a "no" must find the track
        // queueable: were `current` still set, `enqueue` would take it for
        // in-flight work this job is about to abandon and the pin would be
        // lost (B1477).
        let mut q = self.lock();
        if self.is_pin_work(id) {
            return true;
        }
        if q.generation == *generation {
            return true;
        }
        if let Some(pos) = q.wanted.iter().position(|w| w == id) {
            q.wanted.remove(pos);
            *generation = q.generation;
            return true;
        }
        if q.current.as_deref() == Some(id) {
            q.current = None;
        }
        false
    }

    async fn pause_until_online(&self, id: &str, generation: &mut u64) -> bool {
        loop {
            let wait = self.wake.notified();
            tokio::pin!(wait);
            wait.as_mut().enable();
            if !self.still_wanted(id, generation) {
                return false;
            }
            if self.online.load(Ordering::SeqCst) {
                return true;
            }
            wait.await;
        }
    }

    /// A pin download that is only waiting to retry gives the worker to
    /// playback read-ahead and queues itself again behind it.
    fn yield_to_playback(&self, id: &str, st: &JobState) -> bool {
        if !self.is_pin_work(id) {
            return false;
        }
        let mut q = self.lock();
        if q.wanted.is_empty() {
            return false;
        }
        if !q.pinned.iter().any(|w| w == id) {
            q.pinned.push_back(id.to_owned());
        }
        q.deferred.insert(id.to_owned(), st.clone());
        #[cfg(test)]
        self.yields.fetch_add(1, Ordering::SeqCst);
        true
    }

    async fn sleep_backoff(&self, backoff: &mut Duration, epoch: &mut u64) {
        #[cfg(test)]
        self.waits
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(*backoff);
        let wait = self.wake.notified();
        tokio::pin!(wait);
        wait.as_mut().enable();
        tokio::select! {
            () = tokio::time::sleep(*backoff) => {}
            () = &mut wait => {}
        }
        let now = self.online_epoch.load(Ordering::SeqCst);
        if now != *epoch {
            *epoch = now;
            *backoff = self.tuning.backoff_min;
        } else {
            *backoff = backoff.saturating_mul(2).min(self.tuning.backoff_max);
        }
    }

    /// The cap shared with other downloads cannot be checked yet (their
    /// size is unknown): a wait, never a failure. Logs the wait, backs off
    /// and asks again; the partial file stays to resume from. True when a
    /// waiting pin gave the worker to playback read-ahead instead.
    async fn wait_for_accounting(
        &self,
        id: &str,
        st: &JobState,
        attempt: u32,
        started: Instant,
        backoff: &mut Duration,
        epoch: &mut u64,
    ) -> bool {
        log::info!(
            "cache download id={id} bytes={} ms={} outcome=retry reason=accounting_unknown attempt={attempt} wait_ms={}",
            std::fs::metadata(self.cache.part_path(id)).map_or(0, |m| m.len()),
            started.elapsed().as_millis(),
            backoff.as_millis()
        );
        self.sleep_backoff(backoff, epoch).await;
        self.yield_to_playback(id, st)
    }

    async fn resolve(&self, id: &str) -> Result<String, ResolveError> {
        let f = self.resolve.clone();
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || f(&id))
            .await
            .unwrap_or_else(|e| Err(ResolveError::Transient(format!("resolver task: {e}"))))
    }

    async fn job(&self, id: &str, mut generation: u64) {
        let started = Instant::now();
        let mut url: Option<String> = None;
        let mut epoch = self.online_epoch.load(Ordering::SeqCst);
        let mut refreshed = false;
        let mut attempt = 0u32;
        // Held before the deferred state is read: from here no eviction
        // takes this track's part, and one that took it before is named.
        let _active = Active::start(&self.cache, id);
        // A parked pin carries its backoff across the park.
        let (mut st, mut backoff) = {
            let mut q = self.lock();
            self.drop_evicted_states(&mut q);
            let backoff = q
                .parked
                .remove(id)
                .map_or(self.tuning.backoff_min, |p| p.backoff);
            (q.deferred.remove(id).unwrap_or_default(), backoff)
        };
        let part_len = || std::fs::metadata(self.cache.part_path(id)).map_or(0, |m| m.len());
        self.cache.clear_failed(id);
        // Whatever the job reserved is released however it ends.
        let _reserved = Reserved {
            cache: &self.cache,
            id,
        };
        // The network epoch the current attempt runs in: a refusal parks
        // under it, so a reconnect after the attempt makes the park due.
        let mut attempt_epoch = epoch;
        let outcome = loop {
            // Online and still wanted first: the preflight below may evict
            // other retained parts, which a job that cannot run yet (or is
            // no longer wanted) must not do.
            if !self.pause_until_online(id, &mut generation).await {
                break Outcome::Cancelled;
            }
            attempt_epoch = self.online_epoch.load(Ordering::SeqCst);
            // A retained part always gets its response inspected: the
            // preflight counts its bytes as room (a whole body's admission
            // reclaims them) and authorizes no write. The headers decide:
            // a 206 goes through the normal admission, a 200 through the
            // replacement one, and no body byte is written before one of
            // them passes.
            let replacing = part_len() > 0;
            let preflight = if replacing {
                self.cache.preflight_replacing_part(id)
            } else {
                self.cache.ensure_free(id, 0)
            };
            match preflight {
                // A preflight replaces no part, so it never truncates one.
                Ok(_) | Err(NoRoom::PartNotTruncated) => {}
                // A pin with a part on disk is parked with its part and
                // state, so the worker moves on; anything else stops here
                // (its part, if any, is kept).
                Err(NoRoom::LowDisk(free)) if part_len() > 0 && self.is_pin_work(id) => {
                    log::info!(
                        "cache download id={id} bytes={} ms={} outcome=kept_part reason=low_disk step=preflight replacing={replacing} free_bytes={free} attempt={attempt} wait_ms={}",
                        part_len(),
                        started.elapsed().as_millis(),
                        backoff.as_millis()
                    );
                    break Outcome::Parked(backoff);
                }
                Err(NoRoom::LowDisk(free)) => break Outcome::LowDisk(free),
                Err(NoRoom::AccountingUnknown) => {
                    if self
                        .wait_for_accounting(id, &st, attempt, started, &mut backoff, &mut epoch)
                        .await
                    {
                        break Outcome::Yielded;
                    }
                    continue;
                }
            }
            attempt = attempt.saturating_add(1);
            let u = match url.clone() {
                Some(u) => u,
                None => match self.resolve(id).await {
                    Ok(u) => {
                        url = Some(u.clone());
                        u
                    }
                    Err(ResolveError::Transient(why)) => {
                        log::info!(
                            "cache download id={id} bytes={} ms={} outcome=retry reason=resolve: {why} attempt={attempt}",
                            part_len(),
                            started.elapsed().as_millis()
                        );
                        self.sleep_backoff(&mut backoff, &mut epoch).await;
                        if self.yield_to_playback(id, &st) {
                            break Outcome::Yielded;
                        }
                        continue;
                    }
                    Err(ResolveError::Permanent(why)) => {
                        break Outcome::Failed(format!("resolve: {why}"))
                    }
                },
            };
            let res = self.attempt(id, &u, &mut st, &mut generation).await;
            if st.got_ok {
                st.got_ok = false;
                refreshed = false;
            }
            if st.progressed {
                st.progressed = false;
                backoff = self.tuning.backoff_min;
            }
            match res {
                Ok(()) => break Outcome::Ok,
                Err(Fail::Cancelled) => break Outcome::Cancelled,
                // A pin's resume (or any write over a kept part) refused for
                // room parks, as at the preflight: the part and its state
                // stay, and the worker moves on.
                Err(Fail::LowDisk(free)) if part_len() > 0 && self.is_pin_work(id) => {
                    log::info!(
                        "cache download id={id} bytes={} ms={} outcome=kept_part reason=low_disk step=admission free_bytes={free} attempt={attempt} wait_ms={}",
                        part_len(),
                        started.elapsed().as_millis(),
                        backoff.as_millis()
                    );
                    break Outcome::Parked(backoff);
                }
                Err(Fail::LowDisk(free)) => break Outcome::LowDisk(free),
                // Read-ahead does not hold the worker for a replacement that
                // cannot fit: it stops with the part and its validator kept,
                // and the next request for the track resumes it.
                Err(Fail::ReplacementLowDisk(free)) if !self.is_pin_work(id) => {
                    #[cfg(test)]
                    self.event(format!("replacement_low_disk:{id}"));
                    break Outcome::LowDisk(free);
                }
                Err(Fail::ReplacementLowDisk(free)) => {
                    // The old bytes are on disk and counted already: deleting
                    // them frees nothing an eviction would not, and keeping
                    // them lets a later try resume. The pin is parked, so it
                    // does not hold the worker.
                    log::info!(
                        "cache download id={id} bytes={} ms={} outcome=kept_part reason=low_disk step=replacement free_bytes={free} attempt={attempt} wait_ms={}",
                        part_len(),
                        started.elapsed().as_millis(),
                        backoff.as_millis()
                    );
                    break Outcome::Parked(backoff);
                }
                Err(Fail::AccountingUnknown) => {
                    if self
                        .wait_for_accounting(id, &st, attempt, started, &mut backoff, &mut epoch)
                        .await
                    {
                        break Outcome::Yielded;
                    }
                }
                // A pin is parked with its part, validator and backoff, as
                // for room, so the worker moves on to other pins. Read-ahead
                // retries in place.
                Err(Fail::PartNotTruncated) if self.is_pin_work(id) => {
                    log::info!(
                        "cache download id={id} bytes={} ms={} outcome=kept_part reason=part_not_truncated step=replacement attempt={attempt} wait_ms={}",
                        part_len(),
                        started.elapsed().as_millis(),
                        backoff.as_millis()
                    );
                    break Outcome::Parked(backoff);
                }
                // Read-ahead does not hold the worker over a part it cannot
                // empty: it ends with the part and validator kept, as for a
                // replacement that does not fit, and the next track goes.
                Err(Fail::PartNotTruncated) => break Outcome::KeptPart("part_not_truncated"),
                Err(Fail::Unauthorized) if !refreshed => {
                    refreshed = true;
                    url = None;
                    log::info!(
                        "cache download id={id} bytes={} ms={} outcome=retry reason=unauthorized, fetching a fresh url attempt={attempt}",
                        part_len(),
                        started.elapsed().as_millis()
                    );
                }
                Err(Fail::Unauthorized) => {
                    break Outcome::Failed("unauthorized again after a fresh url".into())
                }
                Err(Fail::Transient(why)) => {
                    log::info!(
                        "cache download id={id} bytes={} ms={} outcome=retry reason={why} attempt={attempt} wait_ms={}",
                        part_len(),
                        started.elapsed().as_millis(),
                        backoff.as_millis()
                    );
                    self.sleep_backoff(&mut backoff, &mut epoch).await;
                    if self.yield_to_playback(id, &st) {
                        break Outcome::Yielded;
                    }
                }
                Err(Fail::Permanent(why)) => break Outcome::Failed(why),
            }
        };
        let ms = started.elapsed().as_millis();
        match outcome {
            Outcome::Ok => {
                let bytes = self.cache.entry(id).map_or(0, |e| e.size);
                log::info!("cache download id={id} bytes={bytes} ms={ms} outcome=ok reason=- attempts={attempt}");
            }
            Outcome::Parked(wait) => {
                #[cfg(test)]
                self.waits
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(wait);
                #[cfg(test)]
                if let Some(hook) = self
                    .before_park
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_mut()
                {
                    hook();
                }
                let next = wait.saturating_mul(2).min(self.tuning.backoff_max);
                let mut q = self.lock();
                q.deferred.insert(id.to_owned(), st);
                if !q.pinned.iter().any(|p| p == id) {
                    q.pinned.push_back(id.to_owned());
                }
                q.parked.insert(
                    id.to_owned(),
                    Parked {
                        until: Instant::now() + wait,
                        backoff: next,
                        epoch: attempt_epoch,
                    },
                );
                #[cfg(test)]
                self.event(format!("parked:{id}"));
                log::info!(
                    "cache download id={id} bytes={} ms={ms} outcome=parked reason=low_disk retry_in_ms={} next_wait_ms={}; part and validator kept, worker moves on",
                    part_len(),
                    wait.as_millis(),
                    next.as_millis()
                );
            }
            Outcome::Yielded => {
                log::info!(
                    "cache download id={id} bytes={} ms={ms} outcome=yielded reason=playback read-ahead is waiting; pin queued again",
                    part_len()
                );
            }
            Outcome::Cancelled => {
                // The part stays, and so does the validator its bytes were
                // fetched under: a later request resumes them with
                // `If-Range`, never appends another version's suffix.
                let kept = part_len();
                if kept > 0 {
                    self.lock().deferred.insert(id.to_owned(), st);
                }
                log::info!(
                    "cache download id={id} bytes={kept} ms={ms} outcome=failed reason=cancelled (no longer wanted; partial kept) validator={}",
                    if kept > 0 { "kept" } else { "none" }
                );
            }
            Outcome::KeptPart(why) => {
                log::info!(
                    "cache download id={id} bytes={} ms={ms} outcome=skipped reason={why} part=kept attempts={attempt}",
                    part_len()
                );
                self.give_up(id, PartFate::Keep(st));
            }
            Outcome::LowDisk(free) => {
                // A refusal for space, from the preflight, the admission or
                // a write: the bytes on disk are still good, so they stay.
                log::warn!(
                    "cache download id={id} bytes={} ms={ms} outcome=skipped reason=low_disk free_bytes={free} part=kept",
                    part_len()
                );
                self.give_up(id, PartFate::Keep(st));
            }
            Outcome::Failed(why) => {
                log::warn!(
                    "cache download id={id} bytes={} ms={ms} outcome=failed reason={why} attempts={attempt}",
                    part_len()
                );
                self.give_up(id, PartFate::Discard);
            }
        }
        // What this job's admissions evicted goes now, not at the next job.
        self.drop_evicted_states(&mut self.lock());
    }

    /// A part an eviction took cannot be resumed: the state kept for it
    /// goes, and the track starts again from zero. Run as each job starts
    /// and ends, under the queue lock.
    fn drop_evicted_states(&self, q: &mut Queue) {
        for gone in self.cache.take_evicted_parts() {
            if q.deferred.remove(&gone).is_some() {
                log::info!("cache download id={gone} verdict=dropped_state reason=part_evicted");
            }
        }
    }

    /// Ends a job that gave up: its partial file goes or stays as `part`
    /// says, the track is marked failed and the job stops being the track
    /// in flight, in one step under the queue lock. A pin of the track that `enqueue` sees after this is
    /// queued as new work; one it saw before was folded into a job that was
    /// still running. Were `current` left set until the worker cleared it,
    /// a pin in between would be taken for in-flight work and lost.
    #[cfg(test)]
    fn event(&self, what: String) {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(what);
    }

    fn give_up(&self, id: &str, part: PartFate) {
        #[cfg(test)]
        tests::before_give_up();
        let mut q = self.lock();
        match part {
            PartFate::Discard => self.cache.discard_part(id),
            // Only bytes that can be resumed need their validator: a track
            // refused before it wrote anything keeps no state, which no
            // eviction notice would ever drop.
            PartFate::Keep(st) => match std::fs::metadata(self.cache.part_path(id)) {
                Ok(m) if m.len() == 0 => {
                    log::info!("cache download id={id} verdict=state_dropped reason=empty_part");
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    log::info!("cache download id={id} verdict=state_dropped reason=no_part");
                }
                // Bytes, or a part that cannot be read: kept, never taken
                // as empty.
                _ => {
                    q.deferred.insert(id.to_owned(), st);
                }
            },
        }
        self.cache.mark_failed(id);
        if q.current.as_deref() == Some(id) {
            q.current = None;
        }
        // A pin that landed after this job had already failed, while it was
        // still the track in flight, was folded into it: it is queued now.
        if q.pin_requested.remove(id) && self.is_pin_work(id) {
            if !q.pinned.iter().any(|p| p == id) {
                q.pinned.push_back(id.to_owned());
            }
            log::info!("cache download id={id} verdict=requeued reason=pinned_while_giving_up");
            drop(q);
            self.wake.notify_one();
        }
    }

    /// Fetch until the file is whole, following range windows. An error leaves
    /// the `.part` in place so the next attempt can resume.
    async fn attempt(
        &self,
        id: &str,
        url: &str,
        st: &mut JobState,
        generation: &mut u64,
    ) -> Result<(), Fail> {
        let parsed = Url::parse(url).map_err(|e| Fail::Permanent(e.redacted()))?;
        let part = self.cache.part_path(id);
        loop {
            if !self.still_wanted(id, generation) {
                return Err(Fail::Cancelled);
            }
            let pos = std::fs::metadata(&part).map_or(0, |m| m.len());
            // A part resumes only under a strong ETag (`If-Range` takes no
            // weak one). With none, a bare `Range` could append another
            // version's suffix: the whole file is asked for instead, and a
            // whole body replaces the part from byte zero under the
            // replacement's reservation rules.
            let validator = st.etag.as_deref().filter(|e| !e.starts_with("W/"));
            let resume = pos > 0 && validator.is_some();
            if pos > 0 && !resume {
                log::info!(
                    "cache download id={id} bytes={pos} outcome=restart reason=no_validator etag={}",
                    if st.etag.is_some() { "weak" } else { "none" }
                );
            }
            let req = Request {
                range_from: resume.then_some(pos),
                if_range: validator.filter(|_| resume),
            };
            let mut resp = http::get(
                &parsed,
                &req,
                self.tuning.connect_timeout,
                self.tuning.read_timeout,
            )
            .await
            .map_err(|e| Fail::Transient(e.redacted()))?;
            // The response's length and validator are taken into `st` only
            // once the part is open in the mode they belong to: a refusal
            // before then (no room, accounting unknown) leaves the bytes on
            // disk paired with the validator they were fetched under, so a
            // retry never resumes them with another version's tag.
            let mut validator = st.etag.clone();
            let total = match resp.status {
                200 => {
                    if resume {
                        log::info!("cache download id={id} server sent the whole file for a range request; restarting at 0");
                    }
                    validator = resp.etag.clone();
                    resp.content_length
                }
                206 => {
                    let Some(cr) = resp.content_range else {
                        return Err(Fail::Transient("206 without Content-Range".into()));
                    };
                    if cr.first != pos {
                        self.cache.discard_part(id);
                        return Err(Fail::Transient(format!(
                            "server resumed at {} but {pos} bytes are on disk; restarting",
                            cr.first
                        )));
                    }
                    cr.total
                }
                401 | 403 => return Err(Fail::Unauthorized),
                416 => {
                    if pos > 0 && st.total == Some(pos) {
                        return self.finish(id, st.total);
                    }
                    self.cache.discard_part(id);
                    return Err(Fail::Transient(
                        "416 range not satisfiable; restarting".into(),
                    ));
                }
                s @ (404 | 410) => return Err(Fail::Permanent(format!("http {s}"))),
                s @ (408 | 425 | 429 | 500..=599) => {
                    return Err(Fail::Transient(format!("http {s}")))
                }
                s => return Err(Fail::Permanent(format!("http {s}"))),
            };
            let append = resp.status == 206;
            st.got_ok = true;
            let start = if append { pos } else { 0 };
            // With no length to go by, the next free-space check's window is
            // what the write may still add before it is looked at again: it
            // is reserved, and rolled forward at each check.
            let left = |written: u64| total.map_or(FREE_CHECK_BYTES, |t| t.saturating_sub(written));
            let unknown_len = total.is_none();
            // What the last check reserved, from where it was made: a chunk
            // that would pass it is reserved for before it is written, not
            // after, so the bytes a host admits against never lag the disk.
            // A whole body over a partial file is admitted with the old
            // bytes as room: the approval empties the part and publishes
            // the new reservation in one step. A refusal keeps them and
            // their validator.
            let replacing = !append && pos > 0;
            // A replacement of unknown length whose speculative window does
            // not fit is not refused for it: the first chunk is read, and
            // admitted with the part counted as room; only then does the
            // part go. One that does not fit even so is refused with the
            // part kept.
            let mut first_chunk = None;
            let mut reserved_ahead = if replacing && unknown_len {
                match self
                    .cache
                    .ensure_free_replacing_part(id, left(start), false)
                {
                    Ok(_) => left(start),
                    Err(NoRoom::LowDisk(room)) => {
                        let chunk = resp
                            .next_chunk()
                            .await
                            .map_err(|e| Fail::Transient(e.redacted()))?
                            .unwrap_or_default();
                        let len = chunk.len() as u64;
                        log::info!(
                            "cache download id={id} reserve verdict=first_chunk reason=window_does_not_fit replacing=true window={} chunk={len} room={room}",
                            left(start)
                        );
                        match self.cache.ensure_free_replacing_part(id, len, true) {
                            Ok(_) => {}
                            Err(NoRoom::LowDisk(free)) => {
                                return Err(Fail::ReplacementLowDisk(free))
                            }
                            Err(no) => return Err(Fail::no_room(no)),
                        }
                        first_chunk = Some(chunk);
                        len
                    }
                    Err(no) => return Err(Fail::no_room(no)),
                }
            } else {
                match self.reserve(id, left(start), 0, unknown_len, replacing) {
                    Err(Fail::LowDisk(free)) if replacing => {
                        return Err(Fail::ReplacementLowDisk(free))
                    }
                    other => other?,
                }
            };
            let mut written = start;
            let mut since_check = 0u64;
            let mut file = self
                .cache
                .open_part(id, append)
                .map_err(|e| Fail::Permanent(format!("open part: {e}")))?;
            st.progressed = true;
            if !append && pos > 0 {
                // The room was approved with the old bytes still on disk, so
                // a refusal kept them; now they are gone, and what the
                // reservation owes counts from zero.
                self.cache.restart_reservation(id);
                log::info!(
                    "cache download id={id} reserve verdict=rebased reason=whole_body_replaced_part old_len={pos}"
                );
            }
            if validator != st.etag {
                log::info!(
                    "cache download id={id} validator verdict=replaced reason=whole_body_opened"
                );
            }
            st.total = total;
            st.etag = validator;
            loop {
                // The first chunk, when its admission was taken above, is
                // written first, within what it reserved.
                let chunk = match first_chunk.take() {
                    Some(c) => c,
                    None => match resp
                        .next_chunk()
                        .await
                        .map_err(|e| Fail::Transient(e.redacted()))?
                    {
                        Some(c) => c,
                        None => break,
                    },
                };
                let len = chunk.len() as u64;
                if since_check.saturating_add(len) > reserved_ahead {
                    reserved_ahead =
                        self.reserve(id, len.max(left(written)), len, unknown_len, false)?;
                    since_check = 0;
                }
                file.write_all(&chunk)
                    .map_err(|e| Fail::Permanent(format!("write part: {e}")))?;
                written = written.saturating_add(len);
                since_check = since_check.saturating_add(len);
                // Spent as soon as it is on disk, before a check below can
                // end the attempt: the budget counts what the disk took.
                if !self.is_pin_work(id) {
                    let mut q = self.lock();
                    q.budget = q.budget.saturating_sub(len);
                }
                // With no length, the window is renewed when another chunk
                // arrives (above, before it is written), never to read the
                // end of the body: a download that has all it will get must
                // not be refused room for bytes that are not coming.
                // Once the advertised length has landed there is nothing
                // left to admit: the body's end is checked below and the
                // file finished, never refused because accounting is
                // unknown for a moment (a restart would need room for a
                // second copy beside the whole part).
                if since_check >= FREE_CHECK_BYTES && st.total.is_some() && left(written) > 0 {
                    since_check = 0;
                    reserved_ahead = left(written);
                    self.cache
                        .ensure_free(id, reserved_ahead)
                        .map_err(Fail::no_room)?;
                }
                if !self.still_wanted(id, generation) {
                    return Err(Fail::Cancelled);
                }
            }
            drop(file);
            let have = std::fs::metadata(&part).map_or(0, |m| m.len());
            match st.total {
                Some(t) if have < t => {}
                Some(t) if have > t => {
                    self.cache.discard_part(id);
                    return Err(Fail::Permanent(format!(
                        "file is {have} bytes but the server advertised {t}"
                    )));
                }
                _ => return self.finish(id, st.total),
            }
        }
    }

    /// Reserves `want` bytes of room for the write. With no length to go by
    /// `want` is a whole check window, which a small track may not need:
    /// when the window does not fit, only `at_least` (the chunk about to be
    /// written; none before the first) is reserved, and checked again at
    /// the next chunk. Returns what was reserved. `replacing`: a whole body
    /// is about to truncate the part, whose bytes then count as room.
    fn reserve(
        &self,
        id: &str,
        want: u64,
        at_least: u64,
        unknown_len: bool,
        replacing: bool,
    ) -> Result<u64, Fail> {
        // A window of unknown length is speculative: the track may need far
        // less of it. It is taken only from room that is free now, never by
        // evicting a cached track; what the write does need (the chunk) may
        // evict, below.
        let speculative = unknown_len && at_least < want;
        let first = if replacing {
            self.cache
                .ensure_free_replacing_part(id, want, !speculative)
        } else if speculative {
            self.cache.ensure_free_without_eviction(id, want)
        } else {
            self.cache.ensure_free(id, want)
        };
        match first {
            Ok(_) => Ok(want),
            Err(NoRoom::LowDisk(room)) if unknown_len && at_least < want => {
                log::info!(
                    "cache download id={id} reserve verdict=chunk reason=window_does_not_fit window={want} chunk={at_least} room={room}"
                );
                self.cache
                    .ensure_free(id, at_least)
                    .map(|_| at_least)
                    .map_err(Fail::no_room)
            }
            Err(no) => Err(Fail::no_room(no)),
        }
    }

    fn finish(&self, id: &str, total: Option<u64>) -> Result<(), Fail> {
        self.cache
            .commit(id, total)
            .map(|_| ())
            .map_err(|e| Fail::Permanent(e.to_string()))
    }
}

/// Releases a track's reserved free space when its job ends.
/// Marks a job's track active in the cache, so eviction leaves its
/// `.part` alone, until dropped.
struct Active<'a> {
    cache: &'a Cache,
    id: &'a str,
}

impl<'a> Active<'a> {
    fn start(cache: &'a Cache, id: &'a str) -> Self {
        cache.job_started(id);
        Self { cache, id }
    }
}

impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.cache.job_ended(self.id);
    }
}

struct Reserved<'a> {
    cache: &'a Cache,
    id: &'a str,
}

impl Drop for Reserved<'_> {
    fn drop(&mut self) {
        self.cache.release_reservation(self.id);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::atomic::AtomicUsize;

    use super::*;
    use crate::cache::{FreeProbe, MIN_FREE_BYTES};
    use crate::testserver::{Script, Seen, TestServer};

    thread_local! {
        /// Runs in `give_up` before it takes the queue lock: after the job
        /// found its permanent failure.
        pub(super) static BEFORE_GIVE_UP: std::cell::RefCell<Option<Box<dyn Fn()>>> =
            const { std::cell::RefCell::new(None) };
    }

    pub(super) fn before_give_up() {
        BEFORE_GIVE_UP.with(|h| {
            if let Some(f) = h.borrow().as_ref() {
                f();
            }
        });
    }

    fn body(n: usize, salt: usize) -> Vec<u8> {
        (0..n)
            .map(|i| u8::try_from((i + salt) % 251).unwrap())
            .collect()
    }

    fn fast() -> Tuning {
        Tuning {
            backoff_min: Duration::from_millis(10),
            backoff_max: Duration::from_millis(40),
            connect_timeout: Duration::from_secs(2),
            read_timeout: Duration::from_secs(2),
            ..Tuning::default()
        }
    }

    // A test's cache folder of its own, made fresh and deleted when the
    // test ends.
    use crate::testdir::TmpDir;

    /// Fields drop in order: the downloader is stopped (its thread joined)
    /// before the folder it writes into is deleted.
    struct Rig {
        srv: Arc<TestServer>,
        cache: Arc<Cache>,
        dl: Downloader,
        resolved: Arc<AtomicUsize>,
        _dir: TmpDir,
    }

    /// `track/N` is served at `/audio/N`. The resolver hands out `tokens[i]`
    /// on its i-th call (the last one repeats).
    fn rig(name: &str, script: Script, cap: u64, tokens: &[&str], tuning: Tuning) -> Rig {
        rig_with_free(
            name,
            script,
            cap,
            tokens,
            tuning,
            Box::new(|_| Ok(u64::MAX)),
        )
    }

    /// [`rig`] with the cache's free-space reading supplied.
    fn rig_with_free(
        name: &str,
        script: Script,
        cap: u64,
        tokens: &[&str],
        tuning: Tuning,
        free: FreeProbe,
    ) -> Rig {
        let srv = Arc::new(TestServer::start(script));
        let dir = TmpDir::new(&format!("auto-dl-{name}"));
        let cache = Arc::new(Cache::open_with_probe(&dir, cap, free).unwrap());
        let resolved = Arc::new(AtomicUsize::new(0));
        let tokens: Vec<String> = tokens.iter().map(|s| (*s).to_owned()).collect();
        let (s2, r2) = (srv.clone(), resolved.clone());
        let resolve: Resolver = Arc::new(move |id: &str| {
            let n = r2.fetch_add(1, Ordering::SeqCst);
            let t = tokens.get(n).or(tokens.last()).cloned().unwrap_or_default();
            let num = id.rsplit('/').next().unwrap_or(id);
            Ok(s2.url_with_token(&format!("/audio/{num}"), &t))
        });
        let dl = Downloader::start(cache.clone(), resolve, tuning).unwrap();
        Rig {
            srv,
            cache,
            dl,
            resolved,
            _dir: dir,
        }
    }

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    fn read(c: &Cache, id: &str) -> Vec<u8> {
        std::fs::read(c.local_path(id).unwrap()).unwrap()
    }

    fn ranges(srv: &TestServer, path: &str) -> Vec<(Option<String>, u16)> {
        srv.seen()
            .into_iter()
            .filter(|s| s.path == path)
            .map(|Seen { range, status, .. }| (range, status))
            .collect()
    }

    #[test]
    fn tracks_download_in_order_and_become_local_files() {
        let r = rig(
            "order",
            Script::default()
                .file("/audio/1", body(5000, 1))
                .file("/audio/2", body(7000, 2)),
            1 << 20,
            &["t"],
            fast(),
        );
        assert_eq!(r.dl.prefetch(&ids(&["track/1", "track/2"])), 2);
        assert!(r.dl.wait_idle(Duration::from_secs(10)));
        assert_eq!(read(&r.cache, "track/1"), body(5000, 1));
        assert_eq!(read(&r.cache, "track/2"), body(7000, 2));
        let order: Vec<String> = r.srv.seen().into_iter().map(|s| s.path).collect();
        assert_eq!(order, ["/audio/1", "/audio/2"]);
        assert_eq!(
            r.dl.prefetch(&ids(&["track/1", "track/2"])),
            0,
            "already cached"
        );
    }

    #[test]
    fn a_dropped_connection_resumes_with_a_range_request() {
        let data = body(200_000, 3);
        let r = rig(
            "resume",
            Script::default()
                .file("/audio/1", data.clone())
                .drop_first_after(50_000),
            1 << 20,
            &["t"],
            fast(),
        );
        r.dl.prefetch(&ids(&["track/1"]));
        assert!(r.dl.wait_idle(Duration::from_secs(10)));
        assert_eq!(read(&r.cache, "track/1"), data);
        assert_eq!(
            ranges(&r.srv, "/audio/1"),
            [(None, 200), (Some("bytes=50000-".into()), 206)],
            "the second request asked only for the missing tail"
        );
    }

    #[test]
    fn a_stream_that_cannot_resume_restarts_and_still_completes() {
        let data = body(120_000, 4);
        let r = rig(
            "restart",
            Script::default()
                .file("/audio/1", data.clone())
                .chunked()
                .drop_first_after(30_000),
            1 << 20,
            &["t"],
            fast(),
        );
        r.dl.prefetch(&ids(&["track/1"]));
        assert!(r.dl.wait_idle(Duration::from_secs(10)));
        assert_eq!(read(&r.cache, "track/1"), data);
        let seen = ranges(&r.srv, "/audio/1");
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert_eq!(seen[1].1, 200, "range ignored, whole file again");
    }

    #[test]
    fn range_windows_are_followed_to_the_end() {
        let data = body(100_000, 5);
        let r = rig(
            "window",
            Script::default()
                .file("/audio/1", data.clone())
                .window(30_000)
                .drop_first_after(10_000),
            1 << 20,
            &["t"],
            fast(),
        );
        r.dl.prefetch(&ids(&["track/1"]));
        assert!(r.dl.wait_idle(Duration::from_secs(10)));
        assert_eq!(read(&r.cache, "track/1"), data);
        let got: Vec<Option<String>> = ranges(&r.srv, "/audio/1")
            .into_iter()
            .map(|(r, _)| r)
            .collect();
        assert_eq!(
            got,
            [
                None,
                Some("bytes=10000-".into()),
                Some("bytes=40000-".into()),
                Some("bytes=70000-".into()),
            ]
        );
    }

    #[test]
    fn an_expired_token_gets_a_fresh_url_and_the_download_succeeds() {
        let data = body(20_000, 6);
        let r = rig(
            "refresh",
            Script::default()
                .file("/audio/1", data.clone())
                .valid_token("fresh"),
            1 << 20,
            &["stale", "fresh"],
            fast(),
        );
        r.dl.prefetch(&ids(&["track/1"]));
        assert!(r.dl.wait_idle(Duration::from_secs(10)));
        assert_eq!(read(&r.cache, "track/1"), data);
        assert_eq!(r.resolved.load(Ordering::SeqCst), 2);
        let statuses: Vec<u16> = ranges(&r.srv, "/audio/1").iter().map(|x| x.1).collect();
        assert_eq!(statuses, [401, 200]);
    }

    #[test]
    fn a_token_that_stays_refused_gives_up_on_that_track_only() {
        let r = rig(
            "refused",
            Script::default()
                .file("/audio/1", body(1000, 1))
                .file("/audio/2", body(1000, 2))
                .valid_token("never"),
            1 << 20,
            &["bad"],
            fast(),
        );
        r.dl.prefetch(&ids(&["track/1", "track/2"]));
        assert!(r.dl.wait_idle(Duration::from_secs(10)));
        assert!(!r.cache.is_complete("track/1") && !r.cache.is_complete("track/2"));
        assert!(!r.cache.part_path("track/1").exists());
        assert_eq!(r.resolved.load(Ordering::SeqCst), 4, "two tries per track");
    }

    #[test]
    fn a_missing_track_does_not_hold_up_the_next() {
        let r = rig(
            "missing",
            Script::default().file("/audio/2", body(1000, 2)),
            1 << 20,
            &["t"],
            fast(),
        );
        r.cache.pin("album/9", &ids(&["track/1", "track/2"]));
        r.dl.prefetch(&ids(&["track/1", "track/2"]));
        assert!(r.dl.wait_idle(Duration::from_secs(10)));
        assert!(!r.cache.is_complete("track/1"));
        assert!(r.cache.is_complete("track/2"));
        let failed: Vec<bool> = r.cache.downloads()[0]
            .tracks
            .iter()
            .map(|t| t.failed)
            .collect();
        assert_eq!(failed, [true, false], "the missing track shows as failed");
    }

    #[test]
    fn enqueued_tracks_download_behind_the_playback_queue_without_displacing_it() {
        let mut script = Script::default();
        for n in 1..=3 {
            script = script.file(&format!("/audio/{n}"), body(1000, n));
        }
        let r = rig("enqueue", script, 1 << 20, &["t"], fast());
        r.dl.set_online(false);
        r.cache.pin("album/1", &ids(&["track/2", "track/3"]));
        r.dl.prefetch(&ids(&["track/1"]));
        assert_eq!(r.dl.enqueue(&ids(&["track/2", "track/3", "track/1"])), 2);
        r.dl.set_online(true);
        assert!(r.dl.wait_idle(Duration::from_secs(10)));
        let order: Vec<String> = r.srv.seen().into_iter().map(|s| s.path).collect();
        assert_eq!(order, ["/audio/1", "/audio/2", "/audio/3"]);
    }

    /// CR outside-diff prefetch.rs:685-689: a job waiting for the network
    /// evicts nothing. Its preflight, which may evict retained parts, runs
    /// only once it is online; then the other part goes and the job runs.
    #[test]
    fn a_job_waiting_for_the_network_evicts_no_retained_part() {
        let script = Script::default().file("/audio/1", body(100, 1));
        let r = rig_with_free(
            "offline-no-evict",
            script,
            1 << 20,
            &["t"],
            fast(),
            Box::new(|dir: &std::path::Path| {
                let parts: u64 = std::fs::read_dir(dir)
                    .map(|d| {
                        d.filter_map(Result::ok)
                            .filter(|e| e.file_name().to_string_lossy().ends_with(".part"))
                            .map(|e| e.metadata().map_or(0, |m| m.len()))
                            .sum()
                    })
                    .unwrap_or(0);
                Ok((MIN_FREE_BYTES + 500).saturating_sub(parts))
            }),
        );
        let other = r.cache.part_path("track/9");
        std::fs::write(&other, vec![7u8; 1000]).unwrap();
        r.dl.set_online(false);
        r.cache.pin("album/a", &ids(&["track/1"]));
        r.dl.enqueue(&ids(&["track/1"]));
        wait_in_flight(&r, "track/1");
        std::thread::sleep(Duration::from_millis(100));
        assert!(other.exists(), "an offline job evicted a retained part");
        r.dl.set_online(true);
        assert!(r.dl.wait_idle(Duration::from_secs(10)));
        assert!(r.cache.is_complete("track/1"));
        assert!(
            !other.exists(),
            "the online job's preflight evicted nothing"
        );
    }

    /// Waits until the downloader has taken a job off the queue.
    fn wait_in_flight(r: &Rig, id: &str) {
        let end = Instant::now() + Duration::from_secs(10);
        while r.dl.in_flight().as_deref() != Some(id) {
            assert!(Instant::now() < end, "{id} never started");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// A pin the downloader has taken off the queue (and is holding for the
    /// network) is still queued work: asking again adds nothing. This raced
    /// with the downloader thread before, so the test waits for it to take
    /// the job instead of hoping it has not.
    #[test]
    fn a_pin_already_downloading_is_not_queued_again() {
        let r = pin_rig("pin-in-flight", fast(), 3, 1000);
        r.dl.set_online(false);
        r.cache.pin("album/a", &ids(&["track/1"]));
        assert_eq!(r.dl.resume_pins(), 1);
        wait_in_flight(&r, "track/1");
        assert_eq!(r.dl.resume_pins(), 0, "in flight counts as queued");
        assert_eq!(r.dl.enqueue(&ids(&["track/1"])), 0);
        r.dl.set_online(true);
        assert!(r.dl.wait_idle(Duration::from_secs(10)));
        assert!(r.cache.is_complete("track/1"));
        assert_eq!(r.srv.seen().len(), 1, "fetched once");
    }

    /// Read-ahead the downloader is already fetching is listed by it, so a
    /// pin of the same track adds nothing new to fetch.
    #[test]
    fn a_pin_of_read_ahead_in_flight_adds_nothing_to_fetch() {
        let r = pin_rig("pin-readahead-in-flight", fast(), 3, 1000);
        r.dl.set_online(false);
        r.cache.pin("album/1", &ids(&["track/2", "track/3"]));
        r.dl.prefetch(&ids(&["track/1"]));
        wait_in_flight(&r, "track/1");
        assert_eq!(r.dl.enqueue(&ids(&["track/2", "track/3", "track/1"])), 2);
        r.dl.set_online(true);
        assert!(r.dl.wait_idle(Duration::from_secs(10)));
        for id in ["track/1", "track/2", "track/3"] {
            assert!(r.cache.is_complete(id), "{id}");
        }
    }

    /// A job decides to cancel, then a pin of its track arrives before the
    /// downloader clears the in-flight mark. The pin must be queued. There
    /// is no downloader thread here, so nothing races the test's stand-in
    /// for the job.
    #[test]
    fn a_pin_arriving_after_a_job_decided_to_cancel_is_not_lost() {
        // Declared first, so dropped last.
        let dir = TmpDir::new("auto-dl-pin-after-cancel");
        let cache = Arc::new(Cache::open(&dir, 1 << 20).unwrap());
        let gate = Arc::new(Gate {
            cache: cache.clone(),
            resolve: Arc::new(|_| Err(ResolveError::Transient("unused".into()))),
            tuning: fast(),
            online: AtomicBool::new(true),
            online_epoch: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            yields: AtomicU64::new(0),
            #[cfg(test)]
            waits: Mutex::new(Vec::new()),
            #[cfg(test)]
            events: Mutex::new(Vec::new()),
            #[cfg(test)]
            before_park: Mutex::new(None),
            queue: Mutex::new(Queue {
                wanted: VecDeque::new(),
                pinned: VecDeque::new(),
                deferred: HashMap::new(),
                parked: HashMap::new(),
                // A newer request has dropped the read-ahead being fetched.
                generation: 1,
                budget: 0,
                busy: true,
                current: Some("track/1".to_owned()),
                pin_requested: HashSet::new(),
            }),
            idle: Condvar::new(),
            wake: Notify::new(),
        });
        let dl = Downloader {
            gate: gate.clone(),
            thread: None,
        };
        let mut stale = 0u64;
        assert!(!gate.still_wanted("track/1", &mut stale), "cancelled");
        cache.pin("album/a", &ids(&["track/1"]));
        dl.enqueue(&ids(&["track/1"]));
        assert_eq!(
            gate.lock().pinned,
            VecDeque::from(ids(&["track/1"])),
            "the pin was lost"
        );
    }

    /// A pin that lands after a job has found a permanent failure but
    /// before it gives up is folded into the job (it is still in flight),
    /// and `give_up` queues it: the pin is never lost.
    #[test]
    fn a_folded_pin_whose_track_is_missing_after_the_job_is_queued() {
        let dir = TmpDir::new("auto-dl-pin-missing-after-job");
        let cache = Arc::new(Cache::open(&dir, 1 << 20).unwrap());
        let gate = Gate {
            cache: cache.clone(),
            resolve: Arc::new(|_| Err(ResolveError::Permanent("unused".into()))),
            tuning: fast(),
            online: AtomicBool::new(true),
            online_epoch: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            yields: AtomicU64::new(0),
            #[cfg(test)]
            waits: Mutex::new(Vec::new()),
            #[cfg(test)]
            events: Mutex::new(Vec::new()),
            #[cfg(test)]
            before_park: Mutex::new(None),
            queue: Mutex::new(Queue {
                wanted: VecDeque::new(),
                pinned: VecDeque::new(),
                deferred: HashMap::new(),
                parked: HashMap::new(),
                generation: 0,
                budget: u64::MAX,
                busy: true,
                current: Some("track/1".to_owned()),
                pin_requested: HashSet::from(["track/1".to_owned()]),
            }),
            idle: Condvar::new(),
            wake: Notify::new(),
        };
        cache.pin("album/a", &ids(&["track/1"]));
        // The job ended with the track not whole (evicted after commit).
        let mut q = gate.lock();
        gate.job_over(&mut q, "track/1");
        assert_eq!(
            q.pinned,
            VecDeque::from(ids(&["track/1"])),
            "the folded pin was dropped with its track missing"
        );
        assert!(q.pin_requested.is_empty());
        // A pin folded into a job whose track is whole asks nothing more.
        q.pinned.clear();
        q.pin_requested.insert("track/1".to_owned());
        drop(q);
        crate::cache::tests::put(&cache, "track/1", 5);
        let mut q = gate.lock();
        gate.job_over(&mut q, "track/1");
        assert!(q.pinned.is_empty(), "a whole track was queued again");
    }

    #[test]
    fn a_pin_between_a_permanent_failure_and_the_give_up_is_queued() {
        // Declared first, so dropped last.
        let dir = TmpDir::new("auto-dl-pin-before-give-up");
        // Ample free space: the job reaches the resolver, not the low-disk
        // preflight, whatever the host's disk holds.
        let cache =
            Arc::new(Cache::open_with_probe(&dir, 1 << 20, Box::new(|_| Ok(u64::MAX))).unwrap());
        let resolves = Arc::new(AtomicUsize::new(0));
        let counted = resolves.clone();
        let gate = Arc::new(Gate {
            cache: cache.clone(),
            resolve: Arc::new(move |_| {
                counted.fetch_add(1, Ordering::SeqCst);
                Err(ResolveError::Permanent("no stream".into()))
            }),
            tuning: fast(),
            online: AtomicBool::new(true),
            online_epoch: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            yields: AtomicU64::new(0),
            #[cfg(test)]
            waits: Mutex::new(Vec::new()),
            #[cfg(test)]
            events: Mutex::new(Vec::new()),
            #[cfg(test)]
            before_park: Mutex::new(None),
            queue: Mutex::new(Queue {
                wanted: VecDeque::new(),
                pinned: VecDeque::new(),
                deferred: HashMap::new(),
                parked: HashMap::new(),
                generation: 0,
                budget: u64::MAX,
                busy: true,
                current: Some("track/1".to_owned()),
                pin_requested: HashSet::new(),
            }),
            idle: Condvar::new(),
            wake: Notify::new(),
        });
        let dl = Arc::new(Downloader {
            gate: gate.clone(),
            thread: None,
        });
        let (c2, d2) = (cache.clone(), dl.clone());
        BEFORE_GIVE_UP.with(|h| {
            *h.borrow_mut() = Some(Box::new(move || {
                c2.pin("album/a", &ids(&["track/1"]));
                assert_eq!(d2.enqueue(&ids(&["track/1"])), 0, "not folded into the job");
            }));
        });
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(gate.job("track/1", 0));
        assert_eq!(
            resolves.load(Ordering::SeqCst),
            1,
            "the job ended before the permanent resolver"
        );
        BEFORE_GIVE_UP.with(|h| *h.borrow_mut() = None);
        assert_eq!(
            gate.lock().pinned,
            VecDeque::from(ids(&["track/1"])),
            "the pin was lost with the failed job"
        );
    }

    /// A pin of a track whose read-ahead job has just failed for good,
    /// before the worker clears the job, is queued: the failed job takes no
    /// more work, so the pin must not be folded into it.
    #[test]
    fn a_pin_arriving_after_a_job_failed_for_good_is_queued() {
        // Declared first, so dropped last.
        let dir = TmpDir::new("auto-dl-pin-after-fail");
        // Ample free space: the job reaches the resolver, not the low-disk
        // preflight, whatever the host's disk holds.
        let cache =
            Arc::new(Cache::open_with_probe(&dir, 1 << 20, Box::new(|_| Ok(u64::MAX))).unwrap());
        let resolves = Arc::new(AtomicUsize::new(0));
        let counted = resolves.clone();
        let gate = Arc::new(Gate {
            cache: cache.clone(),
            resolve: Arc::new(move |_| {
                counted.fetch_add(1, Ordering::SeqCst);
                Err(ResolveError::Permanent("no stream".into()))
            }),
            tuning: fast(),
            online: AtomicBool::new(true),
            online_epoch: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            yields: AtomicU64::new(0),
            #[cfg(test)]
            waits: Mutex::new(Vec::new()),
            #[cfg(test)]
            events: Mutex::new(Vec::new()),
            #[cfg(test)]
            before_park: Mutex::new(None),
            queue: Mutex::new(Queue {
                wanted: VecDeque::new(),
                pinned: VecDeque::new(),
                deferred: HashMap::new(),
                parked: HashMap::new(),
                generation: 0,
                budget: u64::MAX,
                busy: true,
                current: Some("track/1".to_owned()),
                pin_requested: HashSet::new(),
            }),
            idle: Condvar::new(),
            wake: Notify::new(),
        });
        let dl = Downloader {
            gate: gate.clone(),
            thread: None,
        };
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(gate.job("track/1", 0));
        assert_eq!(
            resolves.load(Ordering::SeqCst),
            1,
            "the job ended before the permanent resolver"
        );
        // The worker has not cleared the job yet.
        cache.pin("album/a", &ids(&["track/1"]));
        assert_eq!(dl.enqueue(&ids(&["track/1"])), 1, "the pin was not counted");
        assert_eq!(
            gate.lock().pinned,
            VecDeque::from(ids(&["track/1"])),
            "the pin was folded into the failed job"
        );
    }

    fn pin_rig(name: &str, tuning: Tuning, n: usize, size: usize) -> Rig {
        let mut script = Script::default();
        for i in 1..=n {
            script = script.file(&format!("/audio/{i}"), body(size, i));
        }
        rig(name, script, 1 << 20, &["t"], tuning)
    }

    #[test]
    fn pin_downloads_survive_a_playback_prefetch() {
        let r = pin_rig("pin-vs-prefetch", fast(), 3, 1000);
        r.dl.set_online(false);
        r.cache.pin("album/1", &ids(&["track/2", "track/3"]));
        r.dl.enqueue(&ids(&["track/2", "track/3"]));
        r.dl.prefetch(&ids(&["track/1"]));
        r.dl.set_online(true);
        assert!(r.dl.wait_idle(Duration::from_secs(10)));
        for id in ["track/1", "track/2", "track/3"] {
            assert!(r.cache.is_complete(id), "{id}");
        }
    }

    #[test]
    fn a_running_pin_download_is_not_cancelled_by_a_playback_prefetch() {
        let r = pin_rig("pin-running", fast(), 2, 1000);
        r.dl.set_online(false);
        r.cache.pin("album/1", &ids(&["track/2"]));
        r.dl.enqueue(&ids(&["track/2"]));
        r.dl.set_online(true);
        r.dl.prefetch(&ids(&["track/1"]));
        r.dl.prefetch(&ids(&["track/1"]));
        assert!(r.dl.wait_idle(Duration::from_secs(10)));
        assert!(r.cache.is_complete("track/2"));
    }

    #[test]
    fn the_byte_budget_does_not_drop_pinned_tracks() {
        let r = pin_rig(
            "pin-budget",
            Tuning {
                max_bytes: 15_000,
                ..fast()
            },
            3,
            10_000,
        );
        let all = ids(&["track/1", "track/2", "track/3"]);
        r.cache.pin("album/1", &all);
        r.dl.enqueue(&all);
        assert!(r.dl.wait_idle(Duration::from_secs(10)));
        for id in &all {
            assert!(r.cache.is_complete(id), "{id}");
        }
    }

    #[test]
    fn unfinished_pins_are_queued_again_in_container_order() {
        let r = pin_rig("pin-resume", fast(), 3, 1000);
        r.dl.set_online(false);
        r.cache.pin("album/b", &ids(&["track/3"]));
        r.cache.pin("album/a", &ids(&["track/1", "track/2"]));
        assert_eq!(r.dl.resume_pins(), 3);
        assert_eq!(r.dl.resume_pins(), 0, "already queued");
        assert!(r.srv.seen().is_empty(), "nothing before the network is up");
        r.dl.set_online(true);
        assert!(r.dl.wait_idle(Duration::from_secs(10)));
        let order: Vec<String> = r.srv.seen().into_iter().map(|s| s.path).collect();
        assert_eq!(order, ["/audio/1", "/audio/2", "/audio/3"]);
    }

    #[test]
    fn a_pinned_track_already_queued_for_playback_survives_the_budget_running_out() {
        let r = pin_rig(
            "pin-queued-budget",
            Tuning {
                max_bytes: 5_000,
                ..fast()
            },
            2,
            10_000,
        );
        r.dl.set_online(false);
        r.dl.prefetch(&ids(&["track/1", "track/2"]));
        r.cache.pin("album/1", &ids(&["track/2"]));
        r.dl.enqueue(&ids(&["track/2"]));
        r.dl.set_online(true);
        assert!(r.dl.wait_idle(Duration::from_secs(10)));
        assert!(r.cache.is_complete("track/1"));
        assert!(r.cache.is_complete("track/2"), "pinned, so not dropped");
    }

    #[test]
    fn a_pinned_track_already_queued_for_playback_survives_a_later_prefetch() {
        let r = pin_rig("pin-queued-replaced", fast(), 3, 1000);
        r.dl.set_online(false);
        // Offline, the worker takes track/1 and waits on it, so track/2 is
        // still in the playback queue when the pin is enqueued.
        r.dl.prefetch(&ids(&["track/1", "track/2"]));
        r.cache.pin("album/1", &ids(&["track/2"]));
        r.dl.enqueue(&ids(&["track/2"]));
        r.dl.prefetch(&ids(&["track/3"]));
        r.dl.set_online(true);
        assert!(r.dl.wait_idle(Duration::from_secs(10)));
        assert!(r.cache.is_complete("track/3"));
        assert!(r.cache.is_complete("track/2"), "pinned, so not replaced");
    }

    #[test]
    fn a_pin_that_keeps_failing_gives_way_to_playback_read_ahead() {
        let mut script = Script::default()
            .file("/audio/2", body(1000, 2))
            .busy("/audio/1");
        script = script.file("/audio/1", body(1000, 1));
        let r = rig("pin-yield", script, 1 << 20, &["t"], fast());
        r.cache.pin("album/1", &ids(&["track/1"]));
        r.dl.enqueue(&ids(&["track/1"]));
        // The pin is retrying its 503s when playback asks for another track.
        let until = Instant::now() + Duration::from_secs(5);
        while ranges(&r.srv, "/audio/1").len() < 2 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
        }
        r.dl.prefetch(&ids(&["track/2"]));
        let until = Instant::now() + Duration::from_secs(5);
        while !r.cache.is_complete("track/2") && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(r.cache.is_complete("track/2"), "playback was not starved");
        assert!(
            r.cache.entry("track/1").is_some_and(|e| e.pinned),
            "pin kept"
        );
    }

    #[test]
    fn a_pin_that_yielded_resumes_with_its_validator_so_a_changed_file_is_not_appended() {
        let script = Script::default()
            .file("/audio/1", body(1000, 1))
            .file("/audio/2", body(1000, 2))
            .drop_first_after(300);
        let r = rig(
            "pin-yield-etag",
            script,
            1 << 20,
            &["t"],
            Tuning {
                // Long enough that the ordinary retry cannot fire in the
                // test: only playback's wake can end the pin's backoff.
                backoff_min: Duration::from_secs(30),
                backoff_max: Duration::from_secs(30),
                ..fast()
            },
        );
        let wait = |what: &str, mut ok: Box<dyn FnMut() -> bool>| {
            let until = Instant::now() + Duration::from_secs(10);
            while !ok() {
                assert!(Instant::now() < until, "timed out waiting for {what}");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        r.cache.pin("album/1", &ids(&["track/1"]));
        r.dl.enqueue(&ids(&["track/1"]));
        // The first body drops after 300 bytes and the pin backs off with a
        // partial file on disk.
        let part = r.cache.part_path("track/1");
        wait(
            "the partial file",
            Box::new(move || std::fs::metadata(&part).is_ok_and(|m| m.len() == 300)),
        );
        // Playback asks: the pin must yield (its state is parked) before track/2 runs.
        r.dl.prefetch(&ids(&["track/2"]));
        let dl = &r.dl;
        wait("the pin to yield", Box::new(|| dl.yields() == 1));
        assert!(r.dl.wait_idle(Duration::from_secs(10)));
        assert!(r.cache.is_complete("track/2"));
        assert!(r.cache.is_complete("track/1"));
        let seen = r.srv.seen();
        let at = |path: &str, n: usize| {
            seen.iter()
                .enumerate()
                .filter(|(_, s)| s.path == path)
                .nth(n)
                .map(|(i, _)| i)
        };
        let (two, resumed) = (at("/audio/2", 0).unwrap(), at("/audio/1", 1).unwrap());
        assert!(
            two < resumed,
            "playback ran before the pin resumed: {seen:?}"
        );
        let second = seen.get(resumed).unwrap();
        assert_eq!(second.range.as_deref(), Some("bytes=300-"));
        assert_eq!(
            second.if_range.as_deref(),
            Some("\"v1\""),
            "the resume carries the fixture ETag so a changed file restarts"
        );
    }

    #[test]
    fn nothing_downloads_while_offline_and_it_resumes_when_back() {
        let r = rig(
            "offline",
            Script::default().file("/audio/1", body(5000, 1)),
            1 << 20,
            &["t"],
            fast(),
        );
        r.dl.set_online(false);
        r.dl.prefetch(&ids(&["track/1"]));
        assert!(
            !r.dl.wait_idle(Duration::from_millis(400)),
            "paused, not done"
        );
        assert!(r.srv.seen().is_empty(), "no request while offline");
        r.dl.set_online(true);
        assert!(r.dl.wait_idle(Duration::from_secs(10)));
        assert!(r.cache.is_complete("track/1"));
    }

    #[test]
    fn a_request_is_capped_in_tracks_and_the_rest_is_not_fetched() {
        let mut script = Script::default();
        for n in 1..=4 {
            script = script.file(&format!("/audio/{n}"), body(1000, n));
        }
        let r = rig(
            "cap",
            script,
            1 << 20,
            &["t"],
            Tuning {
                max_tracks: 3,
                ..fast()
            },
        );
        let all = ids(&["track/1", "track/2", "track/3", "track/4"]);
        assert_eq!(r.dl.prefetch(&all), 3);
        assert!(r.dl.wait_idle(Duration::from_secs(10)));
        assert!(r.cache.is_complete("track/3") && !r.cache.is_complete("track/4"));
    }

    #[test]
    fn the_byte_budget_stops_the_queue_after_the_track_in_flight() {
        let mut script = Script::default();
        for n in 1..=3 {
            script = script.file(&format!("/audio/{n}"), body(10_000, n));
        }
        let r = rig(
            "budget",
            script,
            1 << 20,
            &["t"],
            Tuning {
                max_bytes: 15_000,
                ..fast()
            },
        );
        r.dl.prefetch(&ids(&["track/1", "track/2", "track/3"]));
        assert!(r.dl.wait_idle(Duration::from_secs(10)));
        assert!(r.cache.is_complete("track/1") && r.cache.is_complete("track/2"));
        assert!(!r.cache.is_complete("track/3"));
    }

    /// A chunk that was written is spent from the read-ahead budget even
    /// when the free-space check after it fails: the budget counts what the
    /// disk took, not what was checked.
    #[test]
    fn a_written_chunk_is_spent_from_the_budget_before_a_failed_check() {
        let size = usize::try_from(FREE_CHECK_BYTES).unwrap() + 100_000;
        let max = 64 * FREE_CHECK_BYTES;
        let r = rig(
            "budget-before-check",
            Script::default()
                .file("/audio/1", body(size, 1))
                .chunk_bytes(4096),
            1 << 30,
            &["t"],
            Tuning {
                max_bytes: max,
                ..fast()
            },
        );
        let part = r.cache.part_path("track/1");
        let p2 = part.clone();
        // Unknown once the first check window has landed: the check after
        // that chunk fails.
        r.cache
            .set_outside_used(Arc::new(move || crate::cache::OutsideBytes {
                held: 0,
                outstanding: 0,
                unknown: std::fs::metadata(&p2).is_ok_and(|m| m.len() >= FREE_CHECK_BYTES),
            }));
        r.dl.prefetch(&ids(&["track/1"]));
        let on_disk = || std::fs::metadata(&part).map_or(0, |m| m.len());
        let spent = || max - r.dl.gate.lock().budget;
        let until = Instant::now() + Duration::from_secs(10);
        while on_disk() < FREE_CHECK_BYTES || spent() != on_disk() {
            assert!(
                Instant::now() < until,
                "spent {} of the budget for {} bytes on disk",
                spent(),
                on_disk()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!r.cache.is_complete("track/1"));
    }

    #[test]
    fn queued_tracks_survive_eviction_while_older_ones_go() {
        let mut script = Script::default();
        for n in 1..=3 {
            script = script.file(&format!("/audio/{n}"), body(1000, n));
        }
        let r = rig("evict", script, 2000, &["t"], fast());
        r.dl.prefetch(&ids(&["track/1"]));
        assert!(r.dl.wait_idle(Duration::from_secs(10)));
        r.dl.prefetch(&ids(&["track/2", "track/3"]));
        assert!(r.dl.wait_idle(Duration::from_secs(10)));
        assert!(!r.cache.is_complete("track/1"), "unqueued and oldest");
        assert!(r.cache.is_complete("track/2") && r.cache.is_complete("track/3"));
    }

    #[test]
    fn a_download_that_would_leave_under_two_gib_free_is_skipped() {
        let r = rig_with_free(
            "lowdisk",
            Script::default().file("/audio/1", body(1000, 1)),
            1 << 20,
            &["t"],
            fast(),
            Box::new(|_| Ok(MIN_FREE_BYTES + 999)),
        );
        r.dl.prefetch(&ids(&["track/1"]));
        assert!(r.dl.wait_idle(Duration::from_secs(10)));
        assert!(
            !r.cache.is_complete("track/1"),
            "1000 bytes would leave 1 short"
        );
        assert!(!r.cache.part_path("track/1").exists());
    }

    /// Installs an outside hook whose size is unknown while `unknown` is
    /// set, and counts how often it was asked while unknown.
    fn unknown_outside(c: &Cache) -> (Arc<AtomicBool>, Arc<AtomicUsize>) {
        let unknown = Arc::new(AtomicBool::new(true));
        let asked = Arc::new(AtomicUsize::new(0));
        let (u, a) = (unknown.clone(), asked.clone());
        c.set_outside_used(Arc::new(move || {
            let unknown = u.load(Ordering::SeqCst);
            if unknown {
                a.fetch_add(1, Ordering::SeqCst);
            }
            crate::cache::OutsideBytes {
                held: 0,
                outstanding: 0,
                unknown,
            }
        }));
        (unknown, asked)
    }

    /// Makes the outside size known once it has been asked `times` times
    /// while unknown: the download waited at least that long.
    fn known_after(unknown: Arc<AtomicBool>, asked: Arc<AtomicUsize>, times: usize) {
        std::thread::spawn(move || {
            for _ in 0..2000 {
                if asked.load(Ordering::SeqCst) >= times {
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            unknown.store(false, Ordering::SeqCst);
        });
    }

    /// A pin queued while the other downloads' size is not known yet (a
    /// resume at start-up before video accounting is ready) waits for it
    /// and downloads once it is known: it is not failed, and it does not
    /// stay stopped until the next launch.
    #[test]
    fn a_pin_waits_for_unknown_accounting_and_downloads_once_it_is_known() {
        let r = pin_rig("accounting-wait", fast(), 1, 1000);
        let (unknown, asked) = unknown_outside(&r.cache);
        known_after(unknown, asked.clone(), 3);
        r.cache.pin("album/1", &ids(&["track/1"]));
        r.dl.enqueue(&ids(&["track/1"]));
        assert!(r.dl.wait_idle(Duration::from_secs(10)));
        assert!(asked.load(Ordering::SeqCst) >= 3, "never waited");
        assert!(
            r.cache.is_complete("track/1"),
            "a pin stopped for accounting that was not ready yet"
        );
    }

    /// Accounting that becomes unknown in the middle of a download keeps
    /// the partial file: the job waits and resumes from it once the size
    /// is known again, rather than deleting it as if the disk were full.
    #[test]
    fn a_download_keeps_its_partial_file_while_accounting_is_unknown() {
        let size = usize::try_from(FREE_CHECK_BYTES * 3).unwrap();
        let (script, release, reached) = Script::default()
            .file("/audio/1", body(size, 1))
            .hold_after(usize::try_from(FREE_CHECK_BYTES).unwrap() + 1);
        let r = rig("accounting-mid", script, u64::MAX, &["t"], fast());
        r.dl.prefetch(&ids(&["track/1"]));
        for _ in 0..5000 {
            if reached.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            reached.load(Ordering::SeqCst),
            "the body never reached its hold"
        );
        let (unknown, asked) = unknown_outside(&r.cache);
        release.store(true, Ordering::SeqCst);
        for _ in 0..5000 {
            if asked.load(Ordering::SeqCst) >= 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            asked.load(Ordering::SeqCst) >= 2,
            "the job never asked while unknown"
        );
        assert!(
            r.cache.part_path("track/1").exists(),
            "the partial file was deleted while accounting was unknown"
        );
        unknown.store(false, Ordering::SeqCst);
        assert!(r.dl.wait_idle(Duration::from_secs(20)));
        assert!(r.cache.is_complete("track/1"));
        assert_eq!(read(&r.cache, "track/1"), body(size, 1));
    }

    /// CR 4638660904: a job cancelled by a newer prefetch while it waits
    /// for accounting keeps the validator with its part: the next request
    /// for the track resumes it with `If-Range`, so an origin's new
    /// version cannot supply a suffix stitched onto the old bytes.
    #[test]
    fn a_job_cancelled_while_accounting_is_unknown_keeps_its_validator() {
        let script = Script::default()
            .file("/audio/1", body(1000, 1))
            .etag("vA")
            .drop_first_after(600)
            .file("/audio/2", body(10, 2));
        let r = rig("cancel-accounting", script, 1 << 20, &["t"], fast());
        let part = r.cache.part_path("track/1");
        // Unknown once the first attempt has left its part, until lifted.
        let (lifted, asked) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicUsize::new(0)),
        );
        let (l2, a2, p2) = (lifted.clone(), asked.clone(), part.clone());
        r.cache.set_outside_used(Arc::new(move || {
            let held = std::fs::metadata(&p2).map_or(0, |m| m.len());
            let unknown = !l2.load(Ordering::SeqCst) && held >= 600;
            if unknown {
                a2.fetch_add(1, Ordering::SeqCst);
            }
            crate::cache::OutsideBytes {
                held: 0,
                outstanding: 0,
                unknown,
            }
        }));
        r.dl.prefetch(&ids(&["track/1"]));
        for _ in 0..5000 {
            if asked.load(Ordering::SeqCst) >= 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(asked.load(Ordering::SeqCst) >= 1, "the job never waited");
        // The newer prefetch cancels the waiting job: it is no longer
        // wanted at its next check, whatever accounting says by then, so
        // the wait is lifted for track 2's own admission.
        r.dl.prefetch(&ids(&["track/2"]));
        lifted.store(true, Ordering::SeqCst);
        assert!(r.dl.wait_idle(Duration::from_secs(10)), "never idle");
        assert_eq!(
            std::fs::metadata(&part).map(|m| m.len()).ok(),
            Some(600),
            "the cancelled job's part went"
        );
        r.dl.prefetch(&ids(&["track/1"]));
        assert!(
            r.dl.wait_idle(Duration::from_secs(10)),
            "the resume never ended"
        );
        let seen: Vec<_> = r
            .srv
            .seen()
            .into_iter()
            .filter(|s| s.path.starts_with("/audio/1"))
            .collect();
        let last = seen.last().expect("no request for track 1");
        assert_eq!(last.range.as_deref(), Some("bytes=600-"), "{seen:?}");
        assert_eq!(last.if_range.as_deref(), Some("\"vA\""), "{seen:?}");
        assert!(r.cache.is_complete("track/1"));
    }

    /// A whole new version arriving for a resume, refused room before the
    /// part is truncated (accounting unknown), leaves the old bytes paired
    /// with their own validator: the retry resumes them with the old tag,
    /// never the new version's, so the two are never stitched together.
    #[test]
    fn a_new_version_refused_before_truncation_keeps_the_old_validator() {
        let script = Script::default()
            .file("/audio/1", body(1000, 1))
            .etag("vA")
            .drop_first_after(300);
        let r = rig(
            "validator-refused",
            script,
            1 << 20,
            &["t"],
            Tuning {
                // Only a network change ends a wait in this test.
                backoff_min: Duration::from_secs(30),
                backoff_max: Duration::from_secs(30),
                ..fast()
            },
        );
        let wait = |what: &str, ok: &dyn Fn() -> bool| {
            let until = Instant::now() + Duration::from_secs(10);
            while !ok() {
                assert!(Instant::now() < until, "timed out waiting for {what}");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        r.dl.prefetch(&ids(&["track/1"]));
        let part = r.cache.part_path("track/1");
        wait("the partial file", &|| {
            std::fs::metadata(&part).is_ok_and(|m| m.len() == 300)
        });
        // Version B now, served whole whatever the range.
        r.srv.edit(|s| s.etag("vB").ignore_range());
        // The outside figure is unknown for one ask only: the second once
        // the figure is installed (installing it trims, which asks too),
        // which is the room check for version B's body (the first is the
        // retry's preflight).
        let asked = Arc::new(AtomicUsize::new(0));
        let armed = Arc::new(AtomicBool::new(false));
        let (a, on) = (asked.clone(), armed.clone());
        r.cache
            .set_outside_used(Arc::new(move || crate::cache::OutsideBytes {
                held: 0,
                outstanding: 0,
                unknown: on.load(Ordering::SeqCst) && a.fetch_add(1, Ordering::SeqCst) + 1 == 2,
            }));
        armed.store(true, Ordering::SeqCst);
        r.dl.set_online(false);
        r.dl.set_online(true);
        wait("the refusal", &|| asked.load(Ordering::SeqCst) >= 2);
        assert_eq!(r.srv.seen().len(), 2, "{:?}", r.srv.seen());
        assert_eq!(
            std::fs::metadata(&part).map(|m| m.len()).ok(),
            Some(300),
            "the old bytes were truncated before the room was approved"
        );
        // The wait ends on a network change.
        r.dl.set_online(false);
        r.dl.set_online(true);
        assert!(r.dl.wait_idle(Duration::from_secs(20)));
        let seen = r.srv.seen();
        let third = seen
            .get(2)
            .unwrap_or_else(|| panic!("no retry after the refusal: {seen:?}"));
        assert_eq!(
            third.if_range.as_deref(),
            Some("\"vA\""),
            "the retry resumed the old bytes with the new version's tag: {seen:?}"
        );
        assert_eq!(read(&r.cache, "track/1"), body(1000, 1));
    }

    /// CR 4639706368: a part fetched with no ETag has no validator to
    /// resume it under. The retry asks for the whole file, so a new version
    /// served meanwhile replaces the part from byte zero, never a bare
    /// `Range` stitching its suffix onto the old bytes.
    #[test]
    fn a_part_with_no_validator_restarts_from_zero_when_the_file_changed() {
        let script = Script::default()
            .file("/audio/1", body(1000, 1))
            .no_etag()
            .drop_first_after(300);
        let r = rig(
            "no-validator",
            script,
            1 << 20,
            &["t"],
            Tuning {
                // Only a network change ends a wait in this test.
                backoff_min: Duration::from_secs(30),
                backoff_max: Duration::from_secs(30),
                ..fast()
            },
        );
        r.dl.prefetch(&ids(&["track/1"]));
        let part = r.cache.part_path("track/1");
        let until = Instant::now() + Duration::from_secs(10);
        while !std::fs::metadata(&part).is_ok_and(|m| m.len() == 300) {
            assert!(Instant::now() < until, "timed out waiting for the part");
            std::thread::sleep(Duration::from_millis(5));
        }
        // A new version of the same length, still with no ETag.
        r.srv.edit(|s| s.file("/audio/1", body(1000, 2)));
        r.dl.set_online(false);
        r.dl.set_online(true);
        assert!(r.dl.wait_idle(Duration::from_secs(20)));
        let seen = r.srv.seen();
        let retry = seen.get(1).unwrap_or_else(|| panic!("no retry: {seen:?}"));
        assert_eq!(retry.range, None, "resumed with a bare Range: {seen:?}");
        assert_eq!(
            read(&r.cache, "track/1"),
            body(1000, 2),
            "versions were stitched"
        );
    }

    /// A body of known length whose last chunk reaches the check window is
    /// finished, not refused, when the accounting turns unknown just then:
    /// nothing is left to admit.
    #[test]
    fn a_body_whose_last_chunk_lands_is_finished_while_accounting_is_unknown() {
        let size = usize::try_from(FREE_CHECK_BYTES).unwrap();
        let script = Script::default().file("/audio/1", vec![5u8; size]);
        let r = rig("whole-then-unknown", script, 1 << 30, &["t"], fast());
        let part = r.cache.part_path("track/1");
        let whole = FREE_CHECK_BYTES;
        r.cache
            .set_outside_used(Arc::new(move || crate::cache::OutsideBytes {
                held: 0,
                outstanding: 0,
                unknown: std::fs::metadata(&part).is_ok_and(|m| m.len() == whole),
            }));
        r.dl.prefetch(&ids(&["track/1"]));
        assert!(r.dl.wait_idle(Duration::from_secs(20)));
        assert!(
            r.cache.is_complete("track/1"),
            "a whole body was refused for unknown accounting"
        );
        assert_eq!(r.srv.seen().len(), 1, "{:?}", r.srv.seen());
    }

    /// A whole new body that replaces a partial file owes what it has yet to
    /// write from zero: the old bytes it truncated are not counted as
    /// written against its reservation.
    #[test]
    fn a_replacement_body_owes_only_what_it_has_not_written() {
        let script = Script::default()
            .file("/audio/1", body(1000, 1))
            .drop_first_after(600);
        let r = rig("replacement-owed", script, 1 << 20, &["t"], fast());
        let wait = |what: &str, ok: &dyn Fn() -> bool| {
            let until = Instant::now() + Duration::from_secs(10);
            while !ok() {
                assert!(Instant::now() < until, "timed out waiting for {what}");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        let part = r.cache.part_path("track/1");
        type Flags = Option<(Arc<AtomicBool>, Arc<AtomicBool>)>;
        let flags: Arc<Mutex<Flags>> = Arc::default();
        let f2 = flags.clone();
        // Held at 800 bytes: past the 600 the first attempt sends before it
        // is dropped, so only the replacement reaches the hold, and only
        // the replacement's file is ever 800 bytes long.
        r.srv.edit(|s| {
            let (s, release, reached) = s.ignore_range().hold_after(800);
            *f2.lock().unwrap() = Some((release, reached));
            s
        });
        r.dl.prefetch(&ids(&["track/1"]));
        let (release, reached) = flags.lock().unwrap().clone().unwrap();
        wait("the replacement held", &|| {
            reached.load(Ordering::SeqCst)
                && r.srv.seen().len() >= 2
                && std::fs::metadata(&part).is_ok_and(|m| m.len() == 800)
        });
        assert_eq!(
            r.cache.reserved_bytes(),
            200,
            "owed counted from the truncated file's old length"
        );
        release.store(true, Ordering::SeqCst);
        assert!(r.dl.wait_idle(Duration::from_secs(20)));
        assert_eq!(read(&r.cache, "track/1"), body(1000, 1));
    }

    /// Free space that has room until `part` holds 600 bytes, then only
    /// `after` above the floor: the drop that leaves the part and the low
    /// reading are in place together, before any retry can run.
    fn room_once_part_is_600(
        part: Arc<Mutex<Option<std::path::PathBuf>>>,
        after: u64,
    ) -> FreeProbe {
        Box::new(move |_| {
            let len = part
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|p| std::fs::metadata(p).ok())
                .map_or(0, |m| m.len());
            Ok(if len >= 600 {
                MIN_FREE_BYTES + after
            } else {
                MIN_FREE_BYTES + 5000
            })
        })
    }

    /// An origin whose every answer is a whole new body (it ignores
    /// ranges) under validator vB, the first cut after 600 bytes: the
    /// retry is a replacement from the start, with no script change to
    /// race.
    fn replacing_origin() -> Script {
        Script::default()
            .file("/audio/1", body(1000, 1))
            .etag("vB")
            .ignore_range()
            .drop_first_after(600)
    }

    /// CR outside-diff prefetch.rs:152: a pin whose replacement cannot
    /// empty its old part is parked with the part and its validator, so
    /// the worker downloads the next pin. Once the part can be emptied,
    /// the parked pin is retried and completes.
    #[test]
    fn a_pin_whose_part_will_not_truncate_is_parked_and_the_next_pin_goes() {
        let script = replacing_origin().file("/audio/2", body(100, 2));
        let r = rig("part-not-truncated", script, 1 << 20, &["t"], fast());
        let part = r.cache.part_path("track/1");
        crate::cache::TRUNCATE_REFUSED
            .lock()
            .unwrap()
            .push(part.clone());
        r.cache.pin("album/a", &ids(&["track/1", "track/2"]));
        r.dl.enqueue(&ids(&["track/1", "track/2"]));
        let until = Instant::now() + Duration::from_secs(5);
        while !r.cache.is_complete("track/2") {
            assert!(
                Instant::now() < until,
                "the untruncatable part held the worker: {:?}",
                r.srv.seen()
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(!r.cache.is_complete("track/1"));
        assert_eq!(std::fs::metadata(&part).map(|m| m.len()).ok(), Some(600));
        assert!(!r.dl.waits().is_empty(), "track 1 was not parked");
        crate::cache::TRUNCATE_REFUSED
            .lock()
            .unwrap()
            .retain(|p| *p != part);
        assert!(
            r.dl.wait_idle(Duration::from_secs(10)),
            "track 1 never retried"
        );
        assert!(r.cache.is_complete("track/1"));
    }

    /// CR review 5995575621 (prefetch.rs:814-824): read-ahead over a part
    /// that will not truncate ends with the part kept, so the next track
    /// goes; it does not hold the worker retrying in place.
    #[test]
    fn read_ahead_whose_part_will_not_truncate_lets_the_next_track_go() {
        let script = replacing_origin().file("/audio/2", body(100, 2));
        let r = rig("read-ahead-not-truncated", script, 1 << 20, &["t"], fast());
        let part = r.cache.part_path("track/1");
        crate::cache::TRUNCATE_REFUSED
            .lock()
            .unwrap()
            .push(part.clone());
        r.dl.prefetch(&ids(&["track/1", "track/2"]));
        let until = Instant::now() + Duration::from_secs(5);
        while !r.cache.is_complete("track/2") {
            assert!(
                Instant::now() < until,
                "the untruncatable part held the worker: {:?}",
                r.srv.seen()
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        crate::cache::TRUNCATE_REFUSED
            .lock()
            .unwrap()
            .retain(|p| *p != part);
        assert!(!r.cache.is_complete("track/1"));
        assert_eq!(std::fs::metadata(&part).map(|m| m.len()).ok(), Some(600));
        assert!(r.dl.has_deferred("track/1"), "the part's validator went");
    }

    /// A pin parked for room, 30 s out, set up for the park tests below:
    /// its first body drops, a reconnect cuts the retry's wait, and the
    /// retry is refused for room and parked.
    fn parked_pin(tag: &str, starve: Arc<AtomicBool>) -> Rig {
        let script = Script::default()
            .file("/audio/1", body(1000, 1))
            .etag("vA")
            .drop_first_after(600);
        let slot: Arc<Mutex<Option<std::path::PathBuf>>> = Arc::default();
        let r = rig_with_free(
            tag,
            script,
            1 << 20,
            &["t"],
            Tuning {
                backoff_min: Duration::from_secs(30),
                backoff_max: Duration::from_secs(30),
                ..fast()
            },
            starved_while_part(slot.clone(), starve, 600, MIN_FREE_BYTES + 100),
        );
        *slot.lock().unwrap() = Some(r.cache.part_path("track/1"));
        r.cache.pin("album/a", &ids(&["track/1"]));
        r.dl.enqueue(&ids(&["track/1"]));
        let until = Instant::now() + Duration::from_secs(10);
        while r.srv.seen().is_empty() {
            assert!(Instant::now() < until, "the pin never started");
            std::thread::sleep(Duration::from_millis(2));
        }
        r.dl.set_online(false);
        r.dl.set_online(true);
        while !r.dl.events().iter().any(|e| e == "parked:track/1") {
            assert!(Instant::now() < until, "the pin was not parked");
            std::thread::sleep(Duration::from_millis(2));
        }
        r
    }

    /// CR review 5995575621 (prefetch.rs:464): playback asking for a pin
    /// that is parked does not run it before its park is due: no request
    /// goes out, and its retry stays scheduled.
    #[test]
    fn a_parked_pin_asked_for_by_playback_waits_out_its_park() {
        let starve = Arc::new(AtomicBool::new(true));
        let r = parked_pin("park-wanted", starve.clone());
        let sent = r.srv.seen().len();
        r.dl.prefetch(&ids(&["track/1"]));
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            r.srv.seen().len(),
            sent,
            "the parked pin ran before its park was due: {:?}",
            r.srv.seen()
        );
        starve.store(false, Ordering::SeqCst);
        r.dl.set_online(false);
        r.dl.set_online(true);
        assert!(r.dl.wait_idle(Duration::from_secs(5)), "the pin was lost");
        assert!(r.cache.is_complete("track/1"));
    }

    /// Outside diff prefetch.rs:493-503 (review 5997638687): a parked pin
    /// whose pin is gone, asked for by playback once space came back, runs
    /// as playback work: its stale park is dropped, not waited out. The
    /// control is the test above, whose pin stays.
    #[test]
    fn an_unpinned_parked_track_asked_for_by_playback_runs() {
        let starve = Arc::new(AtomicBool::new(true));
        let r = parked_pin("park-unpinned", starve.clone());
        starve.store(false, Ordering::SeqCst);
        r.cache.unpin("album/a");
        r.dl.prefetch(&ids(&["track/1"]));
        assert!(
            r.dl.wait_idle(Duration::from_secs(5)),
            "the request was lost"
        );
        assert!(
            r.cache.is_complete("track/1"),
            "playback of an unpinned parked track never ran: {:?}",
            r.srv.seen()
        );
        assert!(
            r.dl.events().iter().any(|e| e == "park_cleared:track/1"),
            "{:?}",
            r.dl.events()
        );
    }

    /// Outside diff prefetch.rs:1013-1015 (review 5997638687): a track
    /// refused for space before it wrote a byte keeps no deferred state,
    /// since no part exists whose eviction would drop it. The control is
    /// a kept part, which keeps its state (the tests above).
    #[test]
    fn a_track_refused_before_any_bytes_keeps_no_state() {
        let r = rig_with_free(
            "no-part-no-state",
            Script::default().file("/audio/1", body(1000, 1)).etag("vA"),
            1 << 20,
            &["t"],
            fast(),
            Box::new(|_| Ok(MIN_FREE_BYTES - 1)),
        );
        r.dl.prefetch(&ids(&["track/1"]));
        assert!(r.dl.wait_idle(Duration::from_secs(5)));
        assert!(!r.cache.part_path("track/1").exists());
        assert!(
            !r.dl.has_deferred("track/1"),
            "a track with no part kept its state"
        );
    }

    /// CR review 5995575621 (prefetch.rs:876): a reconnect after the
    /// refusal, before the park is recorded, makes the park due: the park
    /// carries the refused attempt's epoch, not the one after it.
    #[test]
    fn a_reconnect_before_a_park_is_recorded_makes_it_due() {
        let starve = Arc::new(AtomicBool::new(true));
        let fired = Arc::new(AtomicBool::new(false));
        let gate: Arc<Mutex<Option<Arc<Gate>>>> = Arc::default();
        let (s2, f2, g2) = (starve.clone(), fired.clone(), gate.clone());
        let hook: Box<dyn FnMut() + Send> = Box::new(move || {
            if !f2.swap(true, Ordering::SeqCst) {
                s2.store(false, Ordering::SeqCst);
                if let Some(g) = g2.lock().unwrap().as_ref() {
                    g.online_epoch.fetch_add(1, Ordering::SeqCst);
                }
            }
        });
        // The gate is known once the rig is: the hook reads it through
        // the slot, filled before the first park.
        let r = {
            let script = Script::default()
                .file("/audio/1", body(1000, 1))
                .etag("vA")
                .drop_first_after(600);
            let slot: Arc<Mutex<Option<std::path::PathBuf>>> = Arc::default();
            let r = rig_with_free(
                "park-epoch",
                script,
                1 << 20,
                &["t"],
                Tuning {
                    backoff_min: Duration::from_secs(30),
                    backoff_max: Duration::from_secs(30),
                    ..fast()
                },
                starved_while_part(slot.clone(), starve.clone(), 600, MIN_FREE_BYTES + 100),
            );
            *slot.lock().unwrap() = Some(r.cache.part_path("track/1"));
            *gate.lock().unwrap() = Some(r.dl.gate.clone());
            *r.dl.gate.before_park.lock().unwrap() = Some(hook);
            r.cache.pin("album/a", &ids(&["track/1"]));
            r.dl.enqueue(&ids(&["track/1"]));
            let until = Instant::now() + Duration::from_secs(10);
            while r.srv.seen().is_empty() {
                assert!(Instant::now() < until, "the pin never started");
                std::thread::sleep(Duration::from_millis(2));
            }
            r.dl.set_online(false);
            r.dl.set_online(true);
            r
        };
        assert!(
            r.dl.wait_idle(Duration::from_secs(5)),
            "the reconnect after the refusal left the pin to its 30 s park"
        );
        assert!(fired.load(Ordering::SeqCst), "the pin was never parked");
        assert!(r.cache.is_complete("track/1"));
        *gate.lock().unwrap() = None;
    }

    /// A whole new body refused for low disk keeps the old part and its
    /// validator: the bytes are on disk and counted already, so a pin's
    /// job backs off and tries again, resuming them once there is room.
    /// A pinned part is never evicted (CR 4638660907), and nothing else
    /// asks for room here.
    #[test]
    fn a_replacement_refused_for_low_disk_keeps_the_part_and_its_validator() {
        let slot: Arc<Mutex<Option<std::path::PathBuf>>> = Arc::default();
        let r = rig_with_free(
            "replacement-low-disk",
            replacing_origin(),
            1 << 20,
            &["t"],
            fast(),
            room_once_part_is_600(slot.clone(), 100),
        );
        *slot.lock().unwrap() = Some(r.cache.part_path("track/1"));
        let part = r.cache.part_path("track/1");
        r.cache.pin("album/a", &ids(&["track/1"]));
        r.dl.prefetch(&ids(&["track/1"]));
        let until = Instant::now() + Duration::from_secs(10);
        while r.srv.seen().len() < 4 {
            assert!(Instant::now() < until, "no retries: {:?}", r.srv.seen());
            std::thread::sleep(Duration::from_millis(2));
        }
        let seen = r.srv.seen();
        assert!(
            seen[1..].iter().all(|s| s.status == 200),
            "a retry was not a refused replacement: {seen:?}"
        );
        assert_eq!(
            std::fs::metadata(&part).map(|m| m.len()).ok(),
            Some(600),
            "the refused replacement deleted the part"
        );
        assert!(
            r.cache.take_evicted_parts().is_empty(),
            "a pinned part was evicted"
        );
        assert!(
            seen[1..]
                .iter()
                .all(|s| s.if_range.as_deref() == Some("\"vB\"")),
            "a retry lost the part's validator: {seen:?}"
        );
        assert!(!r.cache.is_complete("track/1"));
    }

    /// CR 4637783558: a 200 that is then refused for space is no progress:
    /// a pin's consecutive refusals back off further, up to the maximum,
    /// instead of starting again at the minimum on every 200.
    #[test]
    fn storage_refusals_back_off_despite_each_200() {
        let slot: Arc<Mutex<Option<std::path::PathBuf>>> = Arc::default();
        let r = rig_with_free(
            "replacement-backoff",
            replacing_origin(),
            1 << 20,
            &["t"],
            fast(),
            room_once_part_is_600(slot.clone(), 100),
        );
        *slot.lock().unwrap() = Some(r.cache.part_path("track/1"));
        r.cache.pin("album/a", &ids(&["track/1"]));
        r.dl.prefetch(&ids(&["track/1"]));
        let until = Instant::now() + Duration::from_secs(10);
        while r.srv.seen().len() < 6 {
            assert!(Instant::now() < until, "no retries: {:?}", r.srv.seen());
            std::thread::sleep(Duration::from_millis(2));
        }
        let waits = r.dl.waits();
        assert!(
            waits.iter().any(|w| *w == fast().backoff_max),
            "the refusals never backed off past the minimum: {waits:?}"
        );
    }

    /// CR 4637783554: read-ahead does not hold the worker on a replacement
    /// that cannot fit. The track stops with its part and validator kept,
    /// and the next track downloads.
    #[test]
    fn a_read_ahead_replacement_that_cannot_fit_lets_the_next_track_go() {
        let script = replacing_origin().file("/audio/2", body(100, 2));
        let slot: Arc<Mutex<Option<std::path::PathBuf>>> = Arc::default();
        let r = rig_with_free(
            "replacement-read-ahead",
            script,
            1 << 20,
            &["t"],
            fast(),
            room_once_part_is_600(slot.clone(), 300),
        );
        *slot.lock().unwrap() = Some(r.cache.part_path("track/1"));
        let part = r.cache.part_path("track/1");
        r.dl.prefetch(&ids(&["track/1", "track/2"]));
        assert!(
            r.dl.wait_idle(Duration::from_secs(10)),
            "the worker stayed on the replacement: {:?}",
            r.srv.seen()
        );
        assert!(r.cache.is_complete("track/2"));
        assert_eq!(std::fs::metadata(&part).map(|m| m.len()).ok(), Some(600));
    }

    /// CR 4637783552, CR 4638660900: a pin whose part is kept is parked
    /// through a preflight refused for low disk, with a backoff, rather
    /// than given up, and resumes the part once there is room.
    #[test]
    fn a_pin_waits_through_a_low_disk_preflight_and_resumes() {
        let script = Script::default()
            .file("/audio/1", body(1000, 1))
            .etag("vA")
            .drop_first_after(600);
        let starve = Arc::new(AtomicBool::new(true));
        let slot: Arc<Mutex<Option<std::path::PathBuf>>> = Arc::default();
        let (p2, s2) = (slot.clone(), starve.clone());
        let r = rig_with_free(
            "pin-preflight-low-disk",
            script,
            1 << 20,
            &["t"],
            fast(),
            Box::new(move |_| {
                let len = p2
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(|p| std::fs::metadata(p).ok())
                    .map_or(0, |m| m.len());
                Ok(if s2.load(Ordering::SeqCst) && len >= 600 {
                    MIN_FREE_BYTES - 1
                } else {
                    MIN_FREE_BYTES + 5000
                })
            }),
        );
        let part = r.cache.part_path("track/1");
        *slot.lock().unwrap() = Some(part.clone());
        r.cache.pin("album/a", &ids(&["track/1"]));
        r.dl.prefetch(&ids(&["track/1"]));
        let until = Instant::now() + Duration::from_secs(10);
        while r.dl.waits().len() < 3 {
            assert!(Instant::now() < until, "the pin did not wait for room");
            std::thread::sleep(Duration::from_millis(2));
        }
        // CR 4640691884: each retry's response is inspected (a range under
        // the part's validator), and its 206, refused for room, parks the
        // pin again without a byte written.
        let retries = r.srv.seen();
        assert!(retries.len() >= 2, "{retries:?}");
        assert!(
            retries[1..]
                .iter()
                .all(|s| s.if_range.as_deref() == Some("\"vA\"") && s.status == 206),
            "{retries:?}"
        );
        assert_eq!(std::fs::metadata(&part).map(|m| m.len()).ok(), Some(600));
        // An event, not the deferred state: the next retry may already have
        // taken that state when this line runs.
        assert!(
            r.dl.events().iter().any(|e| e == "parked:track/1"),
            "the pin was not parked: {:?}",
            r.dl.events()
        );
        starve.store(false, Ordering::SeqCst);
        assert!(
            r.dl.wait_idle(Duration::from_secs(10)),
            "the pin never resumed"
        );
        let seen = r.srv.seen();
        assert_eq!(
            seen.last().and_then(|s| s.if_range.as_deref()),
            Some("\"vA\""),
            "{seen:?}"
        );
        assert!(r.cache.is_complete("track/1"));
    }

    /// CR 4637705955: a retry whose free-space preflight is refused (the
    /// disk below the guard, nothing to evict) ends the job without
    /// deleting the part: its bytes are valid under their validator. A
    /// later prefetch resumes them with that validator. Nothing else needs
    /// the room meanwhile, so no eviction takes the part (CR 4638660907).
    #[test]
    fn a_retry_refused_for_low_disk_keeps_the_part_and_its_validator() {
        let script = Script::default()
            .file("/audio/1", body(1000, 1))
            .etag("vA")
            .drop_first_after(600);
        let slot: Arc<Mutex<Option<std::path::PathBuf>>> = Arc::default();
        let starve = Arc::new(AtomicBool::new(true));
        let (s2, st2) = (slot.clone(), starve.clone());
        // Room until the first attempt has left its part; below the guard
        // from then on, until the test lifts it.
        let r = rig_with_free(
            "retry-low-disk",
            script,
            1 << 20,
            &["t"],
            fast(),
            Box::new(move |_| {
                let part = s2
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(|p| std::fs::metadata(p).ok())
                    .map_or(0, |m| m.len());
                Ok(if st2.load(Ordering::SeqCst) && part >= 600 {
                    MIN_FREE_BYTES - 1
                } else {
                    MIN_FREE_BYTES + 5000
                })
            }),
        );
        let part = r.cache.part_path("track/1");
        *slot.lock().unwrap() = Some(part.clone());
        r.dl.prefetch(&ids(&["track/1"]));
        assert!(
            r.dl.wait_idle(Duration::from_secs(10)),
            "the job never ended"
        );
        // The retry's response is inspected: a 206 under the validator,
        // refused for room before a byte is written.
        let first = r.srv.seen();
        assert_eq!(first.len(), 2, "{first:?}");
        assert_eq!(first[1].status, 206, "{first:?}");
        assert_eq!(
            std::fs::metadata(&part).map(|m| m.len()).ok(),
            Some(600),
            "the low-disk refusal deleted the part"
        );
        assert!(r.dl.has_deferred("track/1"), "the part's state was dropped");
        // Room again: the part resumes under the validator it was fetched
        // with.
        starve.store(false, Ordering::SeqCst);
        r.dl.prefetch(&ids(&["track/1"]));
        assert!(
            r.dl.wait_idle(Duration::from_secs(10)),
            "the resume never ended"
        );
        let seen = r.srv.seen();
        assert_eq!(seen.len(), 3, "{seen:?}");
        assert_eq!(seen[2].range.as_deref(), Some("bytes=600-"), "{seen:?}");
        assert_eq!(seen[2].if_range.as_deref(), Some("\"vA\""), "{seen:?}");
        assert!(r.cache.is_complete("track/1"));
    }

    /// CR 4638660907: a read-ahead part kept for low disk, that playback
    /// moved on from, no longer blocks the next admission. The next
    /// track's preflight evicts it (no job holds it, nothing pins it), the
    /// next track downloads, and the state kept to resume the part is
    /// dropped: the track's next request starts from zero, with no range
    /// and no `If-Range`.
    #[test]
    fn a_kept_part_playback_moved_on_from_is_evicted_and_starts_from_zero() {
        let script = Script::default()
            .file("/audio/1", body(1000, 1))
            .etag("vA")
            .drop_first_after(600)
            .file("/audio/2", body(100, 2));
        let slot: Arc<Mutex<Option<std::path::PathBuf>>> = Arc::default();
        let s2 = slot.clone();
        // Below the guard while track 1's part holds its 600 bytes.
        let r = rig_with_free(
            "evict-kept-part",
            script,
            1 << 20,
            &["t"],
            fast(),
            Box::new(move |_| {
                let part = s2
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(|p| std::fs::metadata(p).ok())
                    .map_or(0, |m| m.len());
                Ok(if part >= 600 {
                    MIN_FREE_BYTES - 1
                } else {
                    MIN_FREE_BYTES + 5000
                })
            }),
        );
        let part = r.cache.part_path("track/1");
        *slot.lock().unwrap() = Some(part.clone());
        r.dl.prefetch(&ids(&["track/1"]));
        assert!(
            r.dl.wait_idle(Duration::from_secs(10)),
            "the job never ended"
        );
        assert_eq!(std::fs::metadata(&part).map(|m| m.len()).ok(), Some(600));
        assert!(r.dl.has_deferred("track/1"), "the kept part lost its state");
        // Playback moves on: the kept part is in the way of track 2.
        r.dl.prefetch(&ids(&["track/2"]));
        assert!(
            r.dl.wait_idle(Duration::from_secs(10)),
            "track 2 never ended"
        );
        assert!(r.cache.is_complete("track/2"), "{:?}", r.srv.seen());
        assert!(!part.exists(), "the kept part blocked the admission");
        assert!(
            !r.dl.has_deferred("track/1"),
            "the evicted part's state was kept"
        );
        r.dl.prefetch(&ids(&["track/1"]));
        assert!(
            r.dl.wait_idle(Duration::from_secs(10)),
            "track 1 never ended"
        );
        let seen: Vec<_> = r
            .srv
            .seen()
            .into_iter()
            .filter(|s| s.path == "/audio/1")
            .collect();
        // The kept part's retry had its 206 inspected and refused for room
        // (CR 4640691884); after the eviction a fresh whole body is asked.
        assert_eq!(seen.len(), 3, "{seen:?}");
        assert_eq!(seen[1].status, 206, "{seen:?}");
        assert_eq!(seen[2].range, None, "{seen:?}");
        assert_eq!(
            seen[2].if_range, None,
            "the evicted part's state was kept: {seen:?}"
        );
        assert!(r.cache.is_complete("track/1"));
    }

    /// CR 4638660900: a pin whose retained part cannot be resumed (no
    /// validator) is a whole-body replacement candidate. Its preflight
    /// credits the part's bytes, so the request goes out and the response
    /// is inspected: here a whole, smaller new body that fits once the
    /// part is truncated, though the disk is below the guard with the part.
    #[test]
    fn a_pin_whose_part_will_be_replaced_sends_its_request_through_low_disk() {
        // The new version is smaller: 400 bytes fit where the part was.
        let script = Script::default().file("/audio/1", body(400, 2)).no_etag();
        let slot: Arc<Mutex<Option<std::path::PathBuf>>> = Arc::default();
        let s2 = slot.clone();
        // 100 bytes under the guard while the part holds its 600.
        let r = rig_with_free(
            "preflight-credits-part",
            script,
            1 << 20,
            &["t"],
            fast(),
            Box::new(move |_| {
                let part = s2
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(|p| std::fs::metadata(p).ok())
                    .map_or(0, |m| m.len());
                Ok(if part >= 600 {
                    MIN_FREE_BYTES - 100
                } else {
                    MIN_FREE_BYTES + 5000
                })
            }),
        );
        let part = r.cache.part_path("track/1");
        *slot.lock().unwrap() = Some(part.clone());
        // A part an earlier version left, with no validator to resume it.
        std::fs::write(&part, body(600, 1)).unwrap();
        r.cache.pin("album/a", &ids(&["track/1"]));
        r.dl.enqueue(&ids(&["track/1"]));
        assert!(
            r.dl.wait_idle(Duration::from_secs(5)),
            "the preflight held the request: {:?}",
            r.srv.seen()
        );
        assert_eq!(read(&r.cache, "track/1"), body(400, 2));
    }

    /// CR 4638660900: a pin refused at the preflight does not hold the only
    /// worker. It is parked with its part and validator and a backoff, the
    /// next pin downloads, and the parked one resumes its part once there
    /// is room.
    #[test]
    fn a_pin_refused_for_low_disk_is_parked_and_the_next_pin_goes() {
        let script = Script::default()
            .file("/audio/1", body(1000, 1))
            .etag("vA")
            .drop_first_after(600)
            .file("/audio/2", body(100, 2));
        let starve = Arc::new(AtomicBool::new(true));
        let slot: Arc<Mutex<Option<std::path::PathBuf>>> = Arc::default();
        let cache: Arc<Mutex<std::sync::Weak<Cache>>> = Arc::default();
        let (p2, s2, c2) = (slot.clone(), starve.clone(), cache.clone());
        // Below the guard while track 1 holds its part, except for track
        // 2's own job: only a worker that moved on reaches it.
        let r = rig_with_free(
            "pin-parked",
            script,
            1 << 20,
            &["t"],
            fast(),
            Box::new(move |_| {
                let len = p2
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(|p| std::fs::metadata(p).ok())
                    .map_or(0, |m| m.len());
                let two = c2
                    .lock()
                    .unwrap()
                    .upgrade()
                    .is_some_and(|c| c.is_active("track/2"));
                Ok(if s2.load(Ordering::SeqCst) && len >= 600 && !two {
                    MIN_FREE_BYTES - 1
                } else {
                    MIN_FREE_BYTES + 5000
                })
            }),
        );
        *cache.lock().unwrap() = Arc::downgrade(&r.cache);
        let part = r.cache.part_path("track/1");
        *slot.lock().unwrap() = Some(part.clone());
        r.cache.pin("album/a", &ids(&["track/1", "track/2"]));
        r.dl.enqueue(&ids(&["track/1"]));
        let until = Instant::now() + Duration::from_secs(10);
        while r.dl.waits().is_empty() || !r.dl.has_deferred("track/1") {
            assert!(Instant::now() < until, "track 1 never stopped for room");
            std::thread::sleep(Duration::from_millis(2));
        }
        r.dl.enqueue(&ids(&["track/2"]));
        let until = Instant::now() + Duration::from_secs(5);
        while !r.cache.is_complete("track/2") {
            assert!(
                Instant::now() < until,
                "the refused pin held the worker: {:?}",
                r.srv.seen()
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        // Track 2 finished while track 1 stayed refused, its part kept.
        assert!(!r.cache.is_complete("track/1"), "track 1 was not refused");
        assert_eq!(std::fs::metadata(&part).map(|m| m.len()).ok(), Some(600));
        starve.store(false, Ordering::SeqCst);
        assert!(
            r.dl.wait_idle(Duration::from_secs(10)),
            "track 1 never resumed"
        );
        let one: Vec<_> = r
            .srv
            .seen()
            .into_iter()
            .filter(|s| s.path == "/audio/1")
            .collect();
        // The first request, then each refused retry (its 206 inspected
        // before the park, CR 4640691884) and the resume. How many refused
        // retries fit in the backoff is timing, so only their shape is
        // checked: every request after the first resumes with If-Range.
        assert!(one.len() >= 2, "track 1 never resumed: {one:?}");
        assert!(
            one[1..]
                .iter()
                .all(|s| s.if_range.as_deref() == Some("\"vA\"")),
            "{one:?}"
        );
        assert!(r.cache.is_complete("track/1"));
    }

    /// A probe that reads `free` while the part at `slot` holds at least
    /// `at` bytes and `starve` is set, and is roomy otherwise.
    fn starved_while_part(
        slot: Arc<Mutex<Option<std::path::PathBuf>>>,
        starve: Arc<AtomicBool>,
        at: u64,
        free: u64,
    ) -> FreeProbe {
        Box::new(move |_| {
            let len = slot
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|p| std::fs::metadata(p).ok())
                .map_or(0, |m| m.len());
            Ok(if starve.load(Ordering::SeqCst) && len >= at {
                free
            } else {
                MIN_FREE_BYTES + 5000
            })
        })
    }

    /// Outside-diff item on prefetch.rs:752: a pinned resume whose remaining
    /// bytes do not fit is parked (its 206 refused before a byte is
    /// written), not given up, and resumes once there is room. The disk is
    /// 100 bytes above the guard: the preflight passes, the 400 bytes left
    /// do not.
    #[test]
    fn a_pinned_resume_that_does_not_fit_is_parked() {
        let script = Script::default()
            .file("/audio/1", body(1000, 1))
            .etag("vA")
            .drop_first_after(600);
        let slot: Arc<Mutex<Option<std::path::PathBuf>>> = Arc::default();
        let starve = Arc::new(AtomicBool::new(true));
        let r = rig_with_free(
            "park-resume",
            script,
            1 << 20,
            &["t"],
            fast(),
            starved_while_part(slot.clone(), starve.clone(), 600, MIN_FREE_BYTES + 100),
        );
        let part = r.cache.part_path("track/1");
        *slot.lock().unwrap() = Some(part.clone());
        r.cache.pin("album/a", &ids(&["track/1"]));
        r.dl.enqueue(&ids(&["track/1"]));
        let until = Instant::now() + Duration::from_secs(10);
        while r.srv.seen().iter().filter(|s| s.status == 206).count() < 2 {
            assert!(
                Instant::now() < until,
                "the pin was not retried: {:?}",
                r.srv.seen()
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(std::fs::metadata(&part).map(|m| m.len()).ok(), Some(600));
        starve.store(false, Ordering::SeqCst);
        assert!(
            r.dl.wait_idle(Duration::from_secs(10)),
            "the pin never resumed"
        );
        assert!(r.cache.is_complete("track/1"));
    }

    /// Outside-diff item on prefetch.rs:480: a reconnect ends a parked pin's
    /// backoff, as `set_online` says, rather than leaving it to its timer.
    #[test]
    fn a_reconnect_ends_a_parked_pins_backoff() {
        let script = Script::default()
            .file("/audio/1", body(1000, 1))
            .etag("vA")
            .drop_first_after(600);
        let slot: Arc<Mutex<Option<std::path::PathBuf>>> = Arc::default();
        let starve = Arc::new(AtomicBool::new(true));
        let r = rig_with_free(
            "park-reconnect",
            script,
            1 << 20,
            &["t"],
            Tuning {
                backoff_min: Duration::from_secs(30),
                backoff_max: Duration::from_secs(30),
                ..fast()
            },
            starved_while_part(slot.clone(), starve.clone(), 600, MIN_FREE_BYTES + 100),
        );
        let part = r.cache.part_path("track/1");
        *slot.lock().unwrap() = Some(part.clone());
        r.cache.pin("album/a", &ids(&["track/1"]));
        r.dl.enqueue(&ids(&["track/1"]));
        let until = Instant::now() + Duration::from_secs(10);
        while r.srv.seen().is_empty() {
            assert!(Instant::now() < until, "the pin never started");
            std::thread::sleep(Duration::from_millis(2));
        }
        // The dropped first body's 30 s retry wait is cut by a reconnect
        // too, so the retry runs and is parked for room.
        r.dl.set_online(false);
        r.dl.set_online(true);
        while !r.dl.has_deferred("track/1") {
            assert!(Instant::now() < until, "the pin was not parked");
            std::thread::sleep(Duration::from_millis(2));
        }
        starve.store(false, Ordering::SeqCst);
        r.dl.set_online(false);
        r.dl.set_online(true);
        assert!(
            r.dl.wait_idle(Duration::from_secs(5)),
            "the reconnect left the pin to its 30 s backoff"
        );
        assert!(r.cache.is_complete("track/1"));
    }

    /// CR 4640691884: a part under a strong ETag still has its response
    /// inspected while the disk is below the guard. The origin answers the
    /// range request with a smaller whole body (its ETag changed), and the
    /// replacement is admitted with the part counted as room.
    #[test]
    fn a_resumable_part_gets_its_response_inspected_through_low_disk() {
        let script = Script::default()
            .file("/audio/1", body(1000, 1))
            .etag("vA")
            .drop_first_after(600);
        let slot: Arc<Mutex<Option<std::path::PathBuf>>> = Arc::default();
        let starve = Arc::new(AtomicBool::new(true));
        let r = rig_with_free(
            "inspect-resumable",
            script,
            1 << 20,
            &["t"],
            fast(),
            starved_while_part(slot.clone(), starve, 600, MIN_FREE_BYTES - 100),
        );
        let part = r.cache.part_path("track/1");
        *slot.lock().unwrap() = Some(part.clone());
        r.cache.pin("album/a", &ids(&["track/1"]));
        r.dl.enqueue(&ids(&["track/1"]));
        let until = Instant::now() + Duration::from_secs(10);
        while !std::fs::metadata(&part).is_ok_and(|m| m.len() == 600) {
            assert!(Instant::now() < until, "no part");
            std::thread::sleep(Duration::from_millis(2));
        }
        // The new version: smaller, under another ETag.
        r.srv.edit(|s| s.file("/audio/1", body(400, 2)).etag("vB"));
        assert!(
            r.dl.wait_idle(Duration::from_secs(10)),
            "the response was never inspected: {:?}",
            r.srv.seen()
        );
        assert_eq!(read(&r.cache, "track/1"), body(400, 2));
    }

    /// Outside-diff item on prefetch.rs:1141: a whole body of unknown
    /// length over a part, whose speculative window does not fit, is not
    /// refused: its first chunk is admitted with the part counted as room,
    /// and only then is the part replaced. A first chunk that does not fit
    /// even so is refused with the part kept.
    #[test]
    fn a_replacement_of_unknown_length_shrinks_its_window_to_the_first_chunk() {
        // 600 short: the preflight passes (the part's 600 bytes are room),
        // and no nonempty first piece fits, so the refusal is the
        // first-chunk admission's (CR review 5995575621, prefetch.rs:3269).
        for (under, fits) in [(100u64, true), (600, false)] {
            let script = Script::default()
                .file("/audio/1", body(400, 2))
                .chunked()
                .chunk_bytes(400)
                .no_etag();
            let slot: Arc<Mutex<Option<std::path::PathBuf>>> = Arc::default();
            let r = rig_with_free(
                &format!("replace-window-{under}"),
                script,
                1 << 20,
                &["t"],
                fast(),
                starved_while_part(
                    slot.clone(),
                    Arc::new(AtomicBool::new(true)),
                    600,
                    MIN_FREE_BYTES - under,
                ),
            );
            let part = r.cache.part_path("track/1");
            *slot.lock().unwrap() = Some(part.clone());
            std::fs::write(&part, body(600, 1)).unwrap();
            r.dl.prefetch(&ids(&["track/1"]));
            assert!(
                r.dl.wait_idle(Duration::from_secs(10)),
                "{under}: never ended"
            );
            if fits {
                assert_eq!(read(&r.cache, "track/1"), body(400, 2), "{under}");
            } else {
                assert!(!r.cache.is_complete("track/1"), "{under}");
                assert_eq!(
                    std::fs::read(&part).ok(),
                    Some(body(600, 1)),
                    "{under}: a refused first chunk changed the part"
                );
                assert!(
                    r.dl.events()
                        .iter()
                        .any(|e| e == "replacement_low_disk:track/1"),
                    "{under}: the refusal was not the first chunk's: {:?} {:?}",
                    r.dl.events(),
                    r.srv.seen()
                );
            }
        }
    }

    /// A whole new body that replaces a partial file is admitted with the
    /// old part's bytes counted as room: they come back when it is
    /// truncated, so a disk that holds the new body once the old one goes
    /// is not refused for both at once.
    #[test]
    fn a_replacement_body_counts_the_old_part_as_room() {
        let script = Script::default()
            .file("/audio/1", body(1000, 1))
            .drop_first_after(600);
        let slot: Arc<Mutex<Option<std::path::PathBuf>>> = Arc::default();
        let s2 = slot.clone();
        // A disk with room for the body and 100 bytes more, less whatever
        // the part holds now.
        let r = rig_with_free(
            "replacement-room",
            script,
            1 << 20,
            &["t"],
            fast(),
            Box::new(move |_| {
                let part = s2
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(|p| std::fs::metadata(p).ok())
                    .map_or(0, |m| m.len());
                Ok((MIN_FREE_BYTES + 1100).saturating_sub(part))
            }),
        );
        let part = r.cache.part_path("track/1");
        *slot.lock().unwrap() = Some(part.clone());
        r.srv.edit(Script::ignore_range);
        r.dl.prefetch(&ids(&["track/1"]));
        assert!(r.dl.wait_idle(Duration::from_secs(10)), "never finished");
        assert!(
            r.cache.is_complete("track/1"),
            "the replacement was refused for the bytes it was about to free"
        );
        assert_eq!(read(&r.cache, "track/1"), body(1000, 1));
    }

    /// A response with no length reserves the next check's window of free
    /// space while it writes, so an admission made meanwhile does not count
    /// that window as free.
    #[test]
    fn a_response_of_unknown_length_reserves_one_check_window() {
        let size = usize::try_from(FREE_CHECK_BYTES * 2).unwrap();
        let slot: Arc<Mutex<Option<std::sync::Weak<Cache>>>> = Arc::default();
        let seen: Arc<Mutex<Vec<u64>>> = Arc::default();
        let (slot2, seen2) = (slot.clone(), seen.clone());
        let r = rig_with_free(
            "unknown-length",
            Script::default().file("/audio/1", body(size, 1)).chunked(),
            u64::MAX,
            &["t"],
            fast(),
            Box::new(move |_| {
                let reserved = slot2
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(std::sync::Weak::upgrade)
                    .map_or(0, |c| {
                        c.reserved_bytes() + c.in_progress_bytes().unwrap_or(0)
                    });
                seen2.lock().unwrap().push(reserved);
                Ok(u64::MAX)
            }),
        );
        *slot.lock().unwrap() = Some(Arc::downgrade(&r.cache));
        r.dl.prefetch(&ids(&["track/1"]));
        assert!(r.dl.wait_idle(Duration::from_secs(20)));
        // Reads: the job's check, the check before writing, then each check
        // while writing; the last two see the window claimed by the one
        // before (what is still owed of it and what has landed of it).
        let seen = seen.lock().unwrap().clone();
        assert!(seen.len() >= 3, "{seen:?}");
        assert_eq!(seen[2], FREE_CHECK_BYTES, "window not reserved: {seen:?}");
        assert_eq!(r.cache.reserved_bytes(), 0, "kept after the job ended");
        // The probe holds the cache weakly: the rig's end frees it.
        let cache = Arc::downgrade(&r.cache);
        drop(r);
        assert!(cache.upgrade().is_none(), "the probe kept the cache alive");
    }

    /// A body of unknown length that ends exactly at a window boundary
    /// completes with only the room it used: the window is not renewed to
    /// read the end of the body.
    #[test]
    fn a_body_of_unknown_length_ending_at_a_window_completes_in_low_space() {
        let size = usize::try_from(FREE_CHECK_BYTES).unwrap();
        let slot: Arc<Mutex<Option<std::sync::Weak<Cache>>>> = Arc::default();
        let slot2 = slot.clone();
        let r = rig_with_free(
            "unknown-length-low-space",
            Script::default().file("/audio/1", body(size, 1)).chunked(),
            u64::MAX,
            &["t"],
            fast(),
            Box::new(move |_| {
                // Room is MIN_FREE plus 12 MiB, less what is on the disk.
                let cache = slot2
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(std::sync::Weak::upgrade);
                let on_disk = cache.as_ref().map_or(0, |c| {
                    std::fs::metadata(c.part_path("track/1")).map_or(0, |m| m.len())
                });
                Ok((MIN_FREE_BYTES + 12 * 1024 * 1024).saturating_sub(on_disk))
            }),
        );
        *slot.lock().unwrap() = Some(Arc::downgrade(&r.cache));
        r.dl.prefetch(&ids(&["track/1"]));
        assert!(r.dl.wait_idle(Duration::from_secs(20)));
        assert!(
            r.cache.is_complete("track/1"),
            "refused room for bytes that were not coming"
        );
    }

    /// A small track of unknown length fits where a whole check window
    /// does not: it reserves the chunk it is about to write instead, and
    /// completes as the same track with a length would.
    #[test]
    fn a_small_track_of_unknown_length_uses_the_room_it_needs() {
        let size = 1024 * 1024;
        let slot: Arc<Mutex<Option<std::sync::Weak<Cache>>>> = Arc::default();
        let slot2 = slot.clone();
        let r = rig_with_free(
            "unknown-length-small",
            Script::default()
                .file("/audio/1", body(size, 1))
                .chunked()
                .chunk_bytes(64 * 1024),
            u64::MAX,
            &["t"],
            fast(),
            Box::new(move |_| {
                // 4 MiB above the guard, less what is on the disk.
                let cache = slot2
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(std::sync::Weak::upgrade);
                let on_disk = cache.as_ref().map_or(0, |c| {
                    std::fs::metadata(c.part_path("track/1")).map_or(0, |m| m.len())
                });
                Ok((MIN_FREE_BYTES + 4 * 1024 * 1024).saturating_sub(on_disk))
            }),
        );
        *slot.lock().unwrap() = Some(Arc::downgrade(&r.cache));
        r.dl.prefetch(&ids(&["track/1"]));
        assert!(r.dl.wait_idle(Duration::from_secs(20)));
        assert!(
            r.cache.is_complete("track/1"),
            "a 1 MiB track was refused for want of an 8 MiB window"
        );
        assert_eq!(read(&r.cache, "track/1"), body(size, 1));
    }

    /// A speculative window never costs a cached track: with 4 MiB free
    /// above the guard and a 5 MiB track that eviction could free, a 1 MiB
    /// track of unknown length downloads and the cached track stays.
    #[test]
    fn a_speculative_window_does_not_evict_a_cached_track() {
        let size = 1024 * 1024;
        let cached = 5 * 1024 * 1024;
        let slot: Arc<Mutex<Option<std::sync::Weak<Cache>>>> = Arc::default();
        let slot2 = slot.clone();
        let r = rig_with_free(
            "window-no-evict",
            Script::default()
                .file("/audio/1", body(size, 1))
                .chunked()
                .chunk_bytes(64 * 1024),
            u64::MAX,
            &["t"],
            fast(),
            Box::new(move |_| {
                let cache = slot2
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(std::sync::Weak::upgrade);
                let (on_disk, freed) = cache.as_ref().map_or((0, 0), |c| {
                    let part = std::fs::metadata(c.part_path("track/1")).map_or(0, |m| m.len());
                    let gone = if c.final_path("track/9").exists() {
                        0
                    } else {
                        cached
                    };
                    (part, gone)
                });
                Ok((MIN_FREE_BYTES + 4 * 1024 * 1024 + freed).saturating_sub(on_disk))
            }),
        );
        std::fs::write(
            r.cache.part_path("track/9"),
            vec![9u8; usize::try_from(cached).unwrap()],
        )
        .unwrap();
        r.cache.commit("track/9", Some(cached)).unwrap();
        *slot.lock().unwrap() = Some(Arc::downgrade(&r.cache));
        r.dl.prefetch(&ids(&["track/1"]));
        assert!(r.dl.wait_idle(Duration::from_secs(20)));
        assert!(r.cache.is_complete("track/1"));
        assert!(
            r.cache.is_complete("track/9"),
            "a cached track was evicted for a window the new track did not need"
        );
    }

    /// A chunk that crosses the reserved window is reserved for before it is
    /// written: at every check, what has been written since the check that
    /// reserved is within what that check reserved.
    #[test]
    fn a_chunk_crossing_the_window_is_reserved_before_it_is_written() {
        let chunk = 5_000_000usize;
        let size = chunk * 4;
        let slot: Arc<Mutex<Option<std::sync::Weak<Cache>>>> = Arc::default();
        // (bytes on disk, the reservation and where it was made) at each
        // check.
        type Seen = Arc<Mutex<Vec<(u64, (u64, u64))>>>;
        let seen: Seen = Arc::default();
        let (slot2, seen2) = (slot.clone(), seen.clone());
        let r = rig_with_free(
            "crossing",
            Script::default()
                .file("/audio/1", body(size, 1))
                .chunked()
                .chunk_bytes(chunk),
            u64::MAX,
            &["t"],
            fast(),
            Box::new(move |_| {
                let cache = slot2
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(std::sync::Weak::upgrade);
                if let Some(c) = cache.as_ref() {
                    let on_disk = std::fs::metadata(c.part_path("track/1")).map_or(0, |m| m.len());
                    let held = c.reservation("track/1").unwrap_or((0, on_disk));
                    seen2.lock().unwrap().push((on_disk, held));
                }
                Ok(u64::MAX)
            }),
        );
        *slot.lock().unwrap() = Some(Arc::downgrade(&r.cache));
        r.dl.prefetch(&ids(&["track/1"]));
        assert!(r.dl.wait_idle(Duration::from_secs(20)));
        let seen = seen.lock().unwrap().clone();
        assert!(seen.len() >= 3, "{seen:?}");
        // A check sees the reservation still in force, made by the check
        // before it: what reached the disk since the length it recorded
        // must be within it.
        for &(now, (reserved, from)) in &seen {
            assert!(
                now.saturating_sub(from) <= reserved,
                "{} bytes written past a reservation of {reserved}: {seen:?}",
                now.saturating_sub(from)
            );
        }
    }

    /// While a download runs, the figure a host adds to its cap is what its
    /// `.part` holds, read at each free-space check: the file's length, and
    /// growing.
    #[test]
    fn a_running_download_reports_what_it_has_written() {
        let size = usize::try_from(FREE_CHECK_BYTES * 3).unwrap();
        let slot: Arc<Mutex<Option<std::sync::Weak<Cache>>>> = Arc::default();
        // (bytes on disk, in-progress figure) at each check.
        let seen: Arc<Mutex<Vec<(u64, u64)>>> = Arc::default();
        let (slot2, seen2) = (slot.clone(), seen.clone());
        let r = rig_with_free(
            "in-progress",
            Script::default().file("/audio/1", body(size, 1)),
            u64::MAX,
            &["t"],
            fast(),
            Box::new(move |_| {
                let cache = slot2
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(std::sync::Weak::upgrade);
                if let Some(c) = cache.as_ref() {
                    let on_disk = std::fs::metadata(c.part_path("track/1")).map_or(0, |m| m.len());
                    seen2
                        .lock()
                        .unwrap()
                        .push((on_disk, c.in_progress_bytes().unwrap_or(u64::MAX)));
                }
                Ok(u64::MAX)
            }),
        );
        *slot.lock().unwrap() = Some(Arc::downgrade(&r.cache));
        r.dl.prefetch(&ids(&["track/1"]));
        assert!(r.dl.wait_idle(Duration::from_secs(20)));
        let seen = seen.lock().unwrap().clone();
        assert!(
            seen.iter().any(|(on_disk, _)| *on_disk >= FREE_CHECK_BYTES),
            "no check after bytes were written: {seen:?}"
        );
        for (on_disk, figure) in &seen {
            assert_eq!(figure, on_disk, "{seen:?}");
        }
        assert_eq!(r.cache.in_progress_bytes(), Some(0), "kept after it landed");
    }

    #[test]
    fn free_space_is_checked_again_while_writing() {
        let size = usize::try_from(FREE_CHECK_BYTES * 2).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let c2 = calls.clone();
        // Plenty before the download; short once it has begun writing.
        let r = rig_with_free(
            "lowdisk-mid",
            Script::default().file("/audio/1", body(size, 1)),
            u64::MAX,
            &["t"],
            fast(),
            Box::new(move |_| {
                let n = c2.fetch_add(1, Ordering::SeqCst);
                Ok(if n < 2 { u64::MAX } else { 0 })
            }),
        );
        r.dl.prefetch(&ids(&["track/1"]));
        assert!(r.dl.wait_idle(Duration::from_secs(20)));
        assert!(!r.cache.is_complete("track/1"));
        // Refused for space, not for its bytes: what was written stays.
        assert!(r.cache.part_path("track/1").exists());
        assert!(
            calls.load(Ordering::SeqCst) >= 3,
            "read before and during the write"
        );
    }
}
