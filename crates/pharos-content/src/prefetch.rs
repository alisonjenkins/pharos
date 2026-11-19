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

use std::collections::{HashMap, VecDeque};
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
    generation: u64,
    budget: u64,
    busy: bool,
    /// The track `busy` is about: off the queues, so a request that lists it
    /// again must still see it as queued work.
    current: Option<String>,
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
    /// The cap shared with other downloads cannot be checked yet: wait and
    /// try again, keeping the partial file.
    AccountingUnknown,
    Unauthorized,
    Transient(String),
    Permanent(String),
}

impl Fail {
    fn no_room(no: NoRoom) -> Self {
        match no {
            NoRoom::LowDisk(room) => Self::LowDisk(room),
            NoRoom::AccountingUnknown => Self::AccountingUnknown,
        }
    }
}

#[derive(Default, Clone)]
struct JobState {
    etag: Option<String>,
    total: Option<u64>,
    /// A 2xx arrived since the last reset: the URL's token was good.
    got_ok: bool,
}

enum Outcome {
    Ok,
    Cancelled,
    LowDisk(u64),
    Failed(String),
    /// A retrying pin download stepped aside for playback read-ahead; the pin
    /// is queued again.
    Yielded,
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
            queue: Mutex::new(Queue {
                wanted: VecDeque::new(),
                pinned: VecDeque::new(),
                deferred: HashMap::new(),
                generation: 0,
                budget: 0,
                busy: false,
                current: None,
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
            {
                let mut q = self.lock();
                while let Some(id) = q.wanted.pop_front() {
                    if self.cache.is_complete(&id) {
                        continue;
                    }
                    q.busy = true;
                    q.current = Some(id.clone());
                    return Some((id, q.generation));
                }
                while let Some(id) = q.pinned.pop_front() {
                    if self.cache.is_complete(&id) || !self.is_pin_work(&id) {
                        continue;
                    }
                    q.busy = true;
                    q.current = Some(id.clone());
                    return Some((id, q.generation));
                }
                q.busy = false;
                q.current = None;
                self.idle.notify_all();
            }
            wait.await;
        }
    }

    /// `id` belongs to a pinned container that still needs it.
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
        let mut backoff = self.tuning.backoff_min;
        let mut epoch = self.online_epoch.load(Ordering::SeqCst);
        let mut refreshed = false;
        let mut attempt = 0u32;
        let mut st = self.lock().deferred.remove(id).unwrap_or_default();
        let part_len = || std::fs::metadata(self.cache.part_path(id)).map_or(0, |m| m.len());
        self.cache.clear_failed(id);
        // Whatever the job reserved is released however it ends.
        let _reserved = Reserved {
            cache: &self.cache,
            id,
        };
        let outcome = loop {
            match self.cache.ensure_free(id, 0) {
                Ok(_) => {}
                Err(NoRoom::LowDisk(free)) => break Outcome::LowDisk(free),
                Err(NoRoom::AccountingUnknown) => {
                    if self
                        .wait_for_accounting(id, &st, attempt, started, &mut backoff, &mut epoch)
                        .await
                    {
                        break Outcome::Yielded;
                    }
                    if !self.pause_until_online(id, &mut generation).await {
                        break Outcome::Cancelled;
                    }
                    continue;
                }
            }
            if !self.pause_until_online(id, &mut generation).await {
                break Outcome::Cancelled;
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
                backoff = self.tuning.backoff_min;
            }
            match res {
                Ok(()) => break Outcome::Ok,
                Err(Fail::Cancelled) => break Outcome::Cancelled,
                Err(Fail::LowDisk(free)) => break Outcome::LowDisk(free),
                Err(Fail::AccountingUnknown) => {
                    if self
                        .wait_for_accounting(id, &st, attempt, started, &mut backoff, &mut epoch)
                        .await
                    {
                        break Outcome::Yielded;
                    }
                }
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
            Outcome::Yielded => {
                log::info!(
                    "cache download id={id} bytes={} ms={ms} outcome=yielded reason=playback read-ahead is waiting; pin queued again",
                    part_len()
                );
            }
            Outcome::Cancelled => {
                log::info!(
                    "cache download id={id} bytes={} ms={ms} outcome=failed reason=cancelled (no longer wanted; partial kept)",
                    part_len()
                );
            }
            Outcome::LowDisk(free) => {
                log::warn!(
                    "cache download id={id} bytes={} ms={ms} outcome=skipped reason=low_disk free_bytes={free}",
                    part_len()
                );
                self.cache.discard_part(id);
                self.cache.mark_failed(id);
            }
            Outcome::Failed(why) => {
                log::warn!(
                    "cache download id={id} bytes={} ms={ms} outcome=failed reason={why} attempts={attempt}",
                    part_len()
                );
                self.cache.discard_part(id);
                self.cache.mark_failed(id);
            }
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
            let req = Request {
                range_from: (pos > 0).then_some(pos),
                if_range: st.etag.as_deref().filter(|_| pos > 0),
            };
            let mut resp = http::get(
                &parsed,
                &req,
                self.tuning.connect_timeout,
                self.tuning.read_timeout,
            )
            .await
            .map_err(|e| Fail::Transient(e.redacted()))?;
            let append = match resp.status {
                200 => {
                    if pos > 0 {
                        log::info!("cache download id={id} server sent the whole file for a range request; restarting at 0");
                    }
                    st.total = resp.content_length;
                    st.etag = resp.etag.clone();
                    false
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
                    st.total = cr.total;
                    true
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
            st.got_ok = true;
            let start = if append { pos } else { 0 };
            // With no length to go by, the next free-space check's window is
            // what the write may still add before it is looked at again: it
            // is reserved, and rolled forward at each check.
            let left = |written: u64| {
                st.total
                    .map_or(FREE_CHECK_BYTES, |t| t.saturating_sub(written))
            };
            self.cache
                .ensure_free(id, left(start))
                .map_err(Fail::no_room)?;
            let mut written = start;
            let mut since_check = 0u64;
            // What the last check reserved, from where it was made: a chunk
            // that would pass it is reserved for before it is written, not
            // after, so the bytes a host admits against never lag the disk.
            let mut reserved_ahead = left(start);
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .append(append)
                .truncate(!append)
                .open(&part)
                .map_err(|e| Fail::Permanent(format!("open part: {e}")))?;
            while let Some(chunk) = resp
                .next_chunk()
                .await
                .map_err(|e| Fail::Transient(e.redacted()))?
            {
                let len = chunk.len() as u64;
                if since_check.saturating_add(len) > reserved_ahead {
                    let need = len.max(left(written));
                    self.cache.ensure_free(id, need).map_err(Fail::no_room)?;
                    reserved_ahead = need;
                    since_check = 0;
                }
                file.write_all(&chunk)
                    .map_err(|e| Fail::Permanent(format!("write part: {e}")))?;
                written = written.saturating_add(len);
                since_check = since_check.saturating_add(len);
                // With no length, the window is renewed when another chunk
                // arrives (above, before it is written), never to read the
                // end of the body: a download that has all it will get must
                // not be refused room for bytes that are not coming.
                if since_check >= FREE_CHECK_BYTES && st.total.is_some() {
                    since_check = 0;
                    reserved_ahead = left(written);
                    self.cache
                        .ensure_free(id, reserved_ahead)
                        .map_err(Fail::no_room)?;
                }
                if !self.is_pin_work(id) {
                    let mut q = self.lock();
                    q.budget = q.budget.saturating_sub(chunk.len() as u64);
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

    fn finish(&self, id: &str, total: Option<u64>) -> Result<(), Fail> {
        self.cache
            .commit(id, total)
            .map(|_| ())
            .map_err(|e| Fail::Permanent(e.to_string()))
    }
}

/// Releases a track's reserved free space when its job ends.
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

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("pharos-auto-dl-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// A test's cache folder, deleted when the test ends.
    struct TmpDir(std::path::PathBuf);

    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

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
        let dir = TmpDir(tmp(name));
        let cache = Arc::new(Cache::open_with_probe(&dir.0, cap, free).unwrap());
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
        let dir = TmpDir(tmp("pin-after-cancel"));
        let cache = Arc::new(Cache::open(&dir.0, 1 << 20).unwrap());
        let gate = Arc::new(Gate {
            cache: cache.clone(),
            resolve: Arc::new(|_| Err(ResolveError::Transient("unused".into()))),
            tuning: fast(),
            online: AtomicBool::new(true),
            online_epoch: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            yields: AtomicU64::new(0),
            queue: Mutex::new(Queue {
                wanted: VecDeque::new(),
                pinned: VecDeque::new(),
                deferred: HashMap::new(),
                // A newer request has dropped the read-ahead being fetched.
                generation: 1,
                budget: 0,
                busy: true,
                current: Some("track/1".to_owned()),
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
                    .map_or(0, |c| c.reserved_bytes());
                seen2.lock().unwrap().push(reserved);
                Ok(u64::MAX)
            }),
        );
        *slot.lock().unwrap() = Some(Arc::downgrade(&r.cache));
        r.dl.prefetch(&ids(&["track/1"]));
        assert!(r.dl.wait_idle(Duration::from_secs(20)));
        // Reads: the job's check, the check before writing, then each check
        // while writing; the last two see the window reserved by the one
        // before.
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

    /// A chunk that crosses the reserved window is reserved for before it is
    /// written: at every check, what has been written since the check that
    /// reserved is within what that check reserved.
    #[test]
    fn a_chunk_crossing_the_window_is_reserved_before_it_is_written() {
        let chunk = 5_000_000usize;
        let size = chunk * 4;
        let slot: Arc<Mutex<Option<std::sync::Weak<Cache>>>> = Arc::default();
        // (bytes on disk, reserved) at each check.
        let seen: Arc<Mutex<Vec<(u64, u64)>>> = Arc::default();
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
                    seen2.lock().unwrap().push((on_disk, c.reserved_bytes()));
                }
                Ok(u64::MAX)
            }),
        );
        *slot.lock().unwrap() = Some(Arc::downgrade(&r.cache));
        r.dl.prefetch(&ids(&["track/1"]));
        assert!(r.dl.wait_idle(Duration::from_secs(20)));
        let seen = seen.lock().unwrap().clone();
        assert!(seen.len() >= 3, "{seen:?}");
        // A check sees the reservation the one before it made: what reached
        // the disk since then must be within it.
        for pair in seen.windows(2) {
            let ((was, _), (now, reserved)) = (pair[0], pair[1]);
            assert!(
                now - was <= reserved,
                "{} bytes written past a reservation of {reserved}: {seen:?}",
                now - was
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
        assert!(!r.cache.part_path("track/1").exists());
        assert!(
            calls.load(Ordering::SeqCst) >= 3,
            "read before and during the write"
        );
    }
}
