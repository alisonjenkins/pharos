//! `KeyedLocks` — per-key single-flight for cache fills.
//!
//! Every cache here shares one shape: a miss triggers an expensive fill (an
//! ffmpeg demux, a whole-file decode). Without coordination, N concurrent
//! first-requesters of the SAME key each run that fill — a stampede that, over
//! an NFS-backed multi-GB library, fans one cache miss into N whole-file reads
//! and (worse) races on a shared scratch path. This is exactly a B72 site:
//! `ImageCache` had no such coordination.
//!
//! `KeyedLocks` hands out one `Arc<Mutex<()>>` per key. The canonical use is
//! double-checked:
//!
//! ```ignore
//! if let Some(hit) = cache.get(&key) { return hit; }          // fast path
//! let guard = locks.lock(key.clone()).await;
//! let _g = guard.lock().await;                                // serialize per key
//! if let Some(hit) = cache.get(&key) { return hit; }          // peer filled while we waited
//! let val = expensive_fill().await;                           // exactly one runs
//! cache.store(key, val);
//! ```
//!
//! So the fill runs once per key; every other requester waits and then hits the
//! now-warm cache. Matches the inline pattern already proven in the subtitle /
//! HLS / trickplay caches, extracted here so new caches adopt it by reference.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Arc;
use tokio::sync::Mutex;

/// A registry of per-key locks. Cheap to clone-share behind an `Arc`; the inner
/// map holds one tiny `Arc<Mutex<()>>` per distinct key seen (never pruned, same
/// as the caches this generalises — the entries are pointer-sized and bounded by
/// the library's distinct-key count).
#[derive(Debug)]
pub struct KeyedLocks<K> {
    locks: Mutex<HashMap<K, Arc<Mutex<()>>>>,
}

impl<K> Default for KeyedLocks<K> {
    fn default() -> Self {
        Self {
            locks: Mutex::new(HashMap::new()),
        }
    }
}

impl<K: Eq + Hash + Clone> KeyedLocks<K> {
    pub fn new() -> Self {
        Self::default()
    }

    /// The lock guarding `key`'s fill. Take it, then `.lock().await` it, then
    /// re-check the cache (a peer may have filled while you waited).
    pub async fn lock(&self, key: K) -> Arc<Mutex<()>> {
        self.locks
            .lock()
            .await
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn same_key_serializes_and_fills_once() {
        let locks: Arc<KeyedLocks<u64>> = Arc::new(KeyedLocks::new());
        let fills = Arc::new(AtomicUsize::new(0));
        let done = Arc::new(AtomicUsize::new(0));

        // Ten racers for the SAME key; only the first past the double-check
        // should run the "fill". Simulated cache is the `done` flag.
        let mut tasks = Vec::new();
        for _ in 0..10 {
            let locks = locks.clone();
            let fills = fills.clone();
            let done = done.clone();
            tasks.push(tokio::spawn(async move {
                if done.load(Ordering::SeqCst) == 1 {
                    return;
                }
                let g = locks.lock(7).await;
                let _held = g.lock().await;
                if done.load(Ordering::SeqCst) == 1 {
                    return; // peer filled while we waited
                }
                fills.fetch_add(1, Ordering::SeqCst);
                done.store(1, Ordering::SeqCst);
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(
            fills.load(Ordering::SeqCst),
            1,
            "the fill must run exactly once for a single key under a stampede"
        );
    }

    #[tokio::test]
    async fn distinct_keys_get_distinct_locks() {
        let locks: KeyedLocks<u64> = KeyedLocks::new();
        let a = locks.lock(1).await;
        let b = locks.lock(2).await;
        // Different keys → different Arcs → holding one never blocks the other.
        let _ga = a.lock().await;
        assert!(b.try_lock().is_ok(), "distinct keys must not share a lock");
    }
}
