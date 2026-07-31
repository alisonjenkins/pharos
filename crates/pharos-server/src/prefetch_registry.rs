//! Per-play-session tracking of outstanding HLS segment-prefetch tasks, so they
//! can be cancelled when a session moves to a different media (episode swap) or
//! stops.
//!
//! Segment prefetch is deliberately fire-and-forget (`spawn_one_prefetch` spawns
//! a detached task): it must OUTLIVE the request that triggered it so the next
//! few segments warm while the client keeps playing. But the transcode
//! scheduler only skips a queued job when its caller future was dropped
//! (`scheduler.rs` `ctx.reply.is_closed()`), and a detached prefetch task keeps
//! that caller alive — so nothing cancels it. When the user swaps episodes, the
//! OLD episode's prefetch therefore transcodes to completion, burning encoder
//! slots (and, on a single-GPU box, delaying the NEW episode's first segments).
//!
//! This registry gives prefetch a cancellation handle scoped to the
//! `PlaySessionId`. Cancellation fires on a media switch (`register` with a new
//! media for the same session) and on session stop (`cancel`, from `DELETE
//! /Videos/ActiveEncodings`).
//!
//! What an abort reaches, and how, changed in 006 phase 2a and had to be put
//! back. The fill was once caller-driven under a per-key lock, so dropping
//! `segment_bytes_keyed().await` dropped the scheduler `submit()` with it. Once
//! the encode moved to a DETACHED driver, the abort dropped only the WAITER —
//! the `oneshot` lived on inside the driver, `reply.is_closed()` was false for
//! every abandoned prefetch, and this file's entire purpose quietly stopped
//! working. The segment cache now stops an encode when the last REQUESTER's
//! receiver drops (`hls_cache::await_last_requester_gone`), which is what an
//! abort here produces, so the chain is whole again: abort -> waiter dropped ->
//! last receiver gone -> `produce_segment` future dropped -> `submit()` dropped
//! -> `QueueOutcome::Abandoned`. An encode any OTHER session is still waiting on
//! is untouched, which is the shared result working as intended.

use std::collections::HashMap;
use std::sync::Mutex;
use tokio::task::JoinHandle;

/// Above this many tracked sessions, drop entries whose prefetch has all
/// finished (a session that never sent a stop). Prefetch is a handful of tasks
/// per session and completes in seconds, so this only reclaims leaked map slots.
const MAX_TRACKED_SESSIONS: usize = 256;

struct Entry {
    media_id: u64,
    handles: Vec<JoinHandle<()>>,
}

/// Cancellable registry of in-flight prefetch tasks, keyed by `PlaySessionId`.
#[derive(Default)]
pub struct PrefetchRegistry {
    inner: Mutex<HashMap<String, Entry>>,
}

impl PrefetchRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record freshly-spawned prefetch `handles` for `psid` + `media_id`.
    ///
    /// If the session was prefetching a DIFFERENT media, abort those stale tasks
    /// first (episode swap). For the same media, finished handles are pruned and
    /// the new ones appended. `psid == None` keeps the old untracked
    /// fire-and-forget behaviour (the tasks run but cannot be cancelled).
    pub fn register(&self, psid: Option<&str>, media_id: u64, handles: Vec<JoinHandle<()>>) {
        let Some(psid) = psid else {
            return; // untracked — caller keeps fire-and-forget semantics
        };
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() >= MAX_TRACKED_SESSIONS && !map.contains_key(psid) {
            // Reclaim slots leaked by sessions that never sent a stop: an entry
            // whose prefetch has all finished is safe to forget (aborting a
            // finished task is a no-op anyway).
            map.retain(|_, e| e.handles.iter().any(|h| !h.is_finished()));
        }
        match map.get_mut(psid) {
            Some(entry) if entry.media_id == media_id => {
                entry.handles.retain(|h| !h.is_finished());
                entry.handles.extend(handles);
            }
            Some(entry) => {
                for h in entry.handles.drain(..) {
                    h.abort();
                }
                entry.media_id = media_id;
                entry.handles = handles;
            }
            None => {
                map.insert(psid.to_string(), Entry { media_id, handles });
            }
        }
    }

    /// Append `handles` to `psid`'s set ONLY if it is still on `media_id`;
    /// otherwise abort them. Used by the deep (subtitle-burn) prefetch, which is
    /// spawned from a nested task that may finish AFTER the session already
    /// switched media — appending unconditionally would resurrect abandoned work
    /// (or, via `register`, wrongly abort the new episode's handles).
    pub fn append_if_current(
        &self,
        psid: Option<&str>,
        media_id: u64,
        handles: Vec<JoinHandle<()>>,
    ) {
        let Some(psid) = psid else {
            return;
        };
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match map.get_mut(psid) {
            Some(entry) if entry.media_id == media_id => {
                entry.handles.retain(|h| !h.is_finished());
                entry.handles.extend(handles);
            }
            _ => {
                for h in handles {
                    h.abort(); // session moved on → this late work is unwanted
                }
            }
        }
    }

    /// Cancel and forget all outstanding prefetch for `psid` (session stopped).
    pub fn cancel(&self, psid: &str) {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = map.remove(psid) {
            for h in entry.handles {
                h.abort();
            }
        }
    }

    /// Count of not-yet-finished prefetch tasks tracked for `psid`.
    #[cfg(test)]
    fn live_count(&self, psid: &str) -> usize {
        let map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.get(psid)
            .map(|e| e.handles.iter().filter(|h| !h.is_finished()).count())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// Spawn a task that bumps `counter` only AFTER `delay` — so an abort before
    /// then leaves the counter untouched (proving cancellation).
    fn slow_task(counter: Arc<AtomicU32>, delay: Duration) -> JoinHandle<()> {
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            counter.fetch_add(1, Ordering::SeqCst);
        })
    }

    #[tokio::test]
    async fn switching_media_aborts_the_previous_episodes_prefetch() {
        let reg = PrefetchRegistry::new();
        let ran = Arc::new(AtomicU32::new(0));

        // Episode A queues 3 prefetch tasks that each "transcode" for 200ms.
        let a = (0..3)
            .map(|_| slow_task(ran.clone(), Duration::from_millis(200)))
            .collect();
        reg.register(Some("sess-1"), 100, a);
        assert_eq!(reg.live_count("sess-1"), 3);

        // Swap to episode B on the SAME session → A's prefetch is aborted.
        let b = vec![slow_task(ran.clone(), Duration::from_millis(200))];
        reg.register(Some("sess-1"), 200, b);

        // Let the original 200ms elapse. Only B's one task should have run;
        // A's three were aborted before their sleep completed.
        tokio::time::sleep(Duration::from_millis(350)).await;
        assert_eq!(
            ran.load(Ordering::SeqCst),
            1,
            "episode A's 3 prefetch tasks must be cancelled on the swap; only B's 1 runs"
        );
    }

    #[tokio::test]
    async fn cancel_aborts_all_outstanding_for_the_session() {
        let reg = PrefetchRegistry::new();
        let ran = Arc::new(AtomicU32::new(0));
        let h = (0..4)
            .map(|_| slow_task(ran.clone(), Duration::from_millis(200)))
            .collect();
        reg.register(Some("sess-x"), 7, h);

        reg.cancel("sess-x");
        assert_eq!(reg.live_count("sess-x"), 0, "cancel forgets the session");

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            ran.load(Ordering::SeqCst),
            0,
            "all 4 tasks aborted by cancel"
        );
    }

    #[tokio::test]
    async fn same_media_appends_and_prunes_finished() {
        let reg = PrefetchRegistry::new();
        let ran = Arc::new(AtomicU32::new(0));

        // A fast (already-finished) task + a slow one under the same media.
        let fast = slow_task(ran.clone(), Duration::from_millis(1));
        reg.register(Some("s"), 5, vec![fast]);
        tokio::time::sleep(Duration::from_millis(30)).await; // let the fast one finish

        let slow = slow_task(ran.clone(), Duration::from_millis(200));
        reg.register(Some("s"), 5, vec![slow]);
        // The finished fast handle is pruned; only the slow one is live.
        assert_eq!(reg.live_count("s"), 1);
    }

    #[tokio::test]
    async fn append_if_current_drops_late_work_after_a_switch() {
        let reg = PrefetchRegistry::new();
        let ran = Arc::new(AtomicU32::new(0));
        reg.register(Some("s"), 100, vec![]); // session on media 100

        // Session switches to media 200.
        reg.register(Some("s"), 200, vec![]);

        // A late deep-burn task for the OLD media 100 arrives → must be aborted,
        // not appended (and must not disturb the current media-200 entry).
        let late = vec![slow_task(ran.clone(), Duration::from_millis(150))];
        reg.append_if_current(Some("s"), 100, late);

        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(
            ran.load(Ordering::SeqCst),
            0,
            "late old-media work is aborted"
        );
    }

    #[tokio::test]
    async fn none_psid_is_untracked() {
        let reg = PrefetchRegistry::new();
        let ran = Arc::new(AtomicU32::new(0));
        // No psid → not tracked, runs to completion (old behaviour).
        reg.register(
            None,
            1,
            vec![slow_task(ran.clone(), Duration::from_millis(20))],
        );
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }
}
