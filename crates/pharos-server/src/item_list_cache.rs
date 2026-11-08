//! Stale-while-revalidate cache of the whole `media_items` list.
//!
//! A full-table read costs ~400 ms. The old cache held one mutex across that
//! read, so every home-screen and synth-id request arriving after a library
//! change (or the 60 s backstop) queued behind it. Now a stale value is
//! returned at once and exactly one background task refreshes it; only a cold
//! cache, or a caller that asks for [`ItemListCache::get_fresh`], waits for a
//! load. The slot lock is never held across a store read.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pharos_core::{DomainResult, MediaItem, MediaStore};

/// Backstop: even if some mutation path forgets to bump the generation, the
/// cache refreshes at least this often.
const MAX_AGE: Duration = Duration::from_secs(60);

/// What a lookup did. The `label()` strings are a dashboard contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Current value returned.
    Hit,
    /// Stale value returned; a background refresh was started or is running.
    Stale,
    /// The caller waited for a load (cold cache, or a fresh read was needed).
    Miss,
}

impl Outcome {
    pub const ALL: [Outcome; 3] = [Outcome::Hit, Outcome::Stale, Outcome::Miss];

    pub const fn label(self) -> &'static str {
        match self {
            Outcome::Hit => "hit",
            Outcome::Stale => "stale",
            Outcome::Miss => "miss",
        }
    }

    fn record(self) {
        metrics::counter!("pharos_item_list_cache_total", "outcome" => self.label()).increment(1);
    }
}

type Generation = (u64, u64);
type Entry = (Generation, Instant, Arc<Vec<MediaItem>>);

#[derive(Default)]
pub struct ItemListCache {
    slot: tokio::sync::Mutex<Option<Entry>>,
    /// Serialises store reads so a burst shares one scan.
    load: tokio::sync::Mutex<()>,
    refreshing: AtomicBool,
}

fn millis(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

impl ItemListCache {
    pub fn new() -> Self {
        Self::default()
    }

    async fn current(&self, gen: Generation) -> Option<Arc<Vec<MediaItem>>> {
        let guard = self.slot.lock().await;
        match guard.as_ref() {
            Some((g, at, items)) if *g == gen && at.elapsed() < MAX_AGE => Some(items.clone()),
            _ => None,
        }
    }

    /// Reads the store and fills the slot, unless a peer already did for
    /// `gen` while this call waited for the load lock.
    async fn load<S: MediaStore>(
        &self,
        store: &S,
        gen: Generation,
        reason: &'static str,
    ) -> DomainResult<Arc<Vec<MediaItem>>> {
        let _loading = self.load.lock().await;
        if let Some(items) = self.current(gen).await {
            return Ok(items);
        }
        let started = Instant::now();
        match store.list().await {
            Ok(list) => {
                let items = Arc::new(list);
                *self.slot.lock().await = Some((gen, Instant::now(), items.clone()));
                tracing::info!(
                    reason,
                    rows = items.len(),
                    duration_ms = millis(started),
                    "item list cache refreshed"
                );
                Ok(items)
            }
            Err(e) => {
                tracing::warn!(
                    reason,
                    error = %e,
                    duration_ms = millis(started),
                    "item list cache refresh failed"
                );
                Err(e)
            }
        }
    }

    /// The list for the generation `gen` reports. A stale value is returned
    /// immediately while one background task refreshes it.
    pub async fn get<S, G>(self: &Arc<Self>, store: &S, gen: G) -> DomainResult<Arc<Vec<MediaItem>>>
    where
        S: MediaStore + Clone + Send + 'static,
        G: Fn() -> Generation + Send + 'static,
    {
        let now = gen();
        let stale = {
            let guard = self.slot.lock().await;
            match guard.as_ref() {
                Some((g, at, items)) if *g == now && at.elapsed() < MAX_AGE => {
                    Outcome::Hit.record();
                    return Ok(items.clone());
                }
                Some((_, _, items)) => Some(items.clone()),
                None => None,
            }
        };
        let Some(stale) = stale else {
            Outcome::Miss.record();
            return self.load(store, now, "cold").await;
        };
        Outcome::Stale.record();
        if !self.refreshing.swap(true, Ordering::AcqRel) {
            let this = Arc::clone(self);
            let store = store.clone();
            tokio::spawn(async move {
                // The generation may have moved again while queued; load for
                // the newest so the entry is not stale on arrival.
                let _ = this.load(&store, gen(), "stale").await;
                this.refreshing.store(false, Ordering::Release);
            });
        }
        Ok(stale)
    }

    /// The list as of `gen`, waiting for a load when the cached value is not
    /// current. For a caller that just observed a mutation.
    pub async fn get_fresh<S: MediaStore>(
        &self,
        store: &S,
        gen: Generation,
    ) -> DomainResult<Arc<Vec<MediaItem>>> {
        if let Some(items) = self.current(gen).await {
            Outcome::Hit.record();
            return Ok(items);
        }
        Outcome::Miss.record();
        self.load(store, gen, "fresh").await
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    use pharos_store_sqlx::sqlite::SqliteStore;
    use std::sync::atomic::AtomicU64;

    fn count(snap: &pharos_test_metrics::CumulativeSnapshotter, outcome: &str) -> u64 {
        snap.snapshot()
            .into_vec()
            .into_iter()
            .filter(|(ck, ..)| {
                ck.key().name() == "pharos_item_list_cache_total"
                    && ck
                        .key()
                        .labels()
                        .any(|l| l.key() == "outcome" && l.value() == outcome)
            })
            .map(|(.., v)| match v {
                DebugValue::Counter(n) => n,
                _ => 0,
            })
            .sum()
    }

    fn item(id: u64) -> MediaItem {
        MediaItem {
            id,
            path: format!("/c/{id}.mkv").into(),
            title: format!("T{id}"),
            ..Default::default()
        }
    }

    #[test]
    fn outcome_labels_are_distinct_and_stable() {
        let labels: Vec<_> = Outcome::ALL.iter().map(|o| o.label()).collect();
        assert_eq!(labels, ["hit", "stale", "miss"]);
    }

    #[test]
    fn stale_value_is_served_at_once_and_refreshed_once_in_the_background() {
        let recorder = DebuggingRecorder::new();
        let snap = pharos_test_metrics::snapshotter(&recorder);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        metrics::with_local_recorder(&recorder, || {
            rt.block_on(async {
                let store = SqliteStore::connect("sqlite::memory:").await.unwrap();
                store.put(item(1)).await.unwrap();
                let cache = Arc::new(ItemListCache::new());
                let epoch = Arc::new(AtomicU64::new(0));
                let gen = {
                    let epoch = epoch.clone();
                    move || (epoch.load(Ordering::Relaxed), 0)
                };

                // Cold: waits for the load.
                assert_eq!(cache.get(&store, gen.clone()).await.unwrap().len(), 1);
                // Warm: hit.
                assert_eq!(cache.get(&store, gen.clone()).await.unwrap().len(), 1);

                store.put(item(2)).await.unwrap();
                epoch.fetch_add(1, Ordering::Relaxed);

                // Two callers right after the change: both get the old list
                // without waiting, and only one refresh is started.
                assert_eq!(cache.get(&store, gen.clone()).await.unwrap().len(), 1);
                assert_eq!(cache.get(&store, gen.clone()).await.unwrap().len(), 1);
                assert!(cache.refreshing.load(Ordering::Acquire));

                while cache.refreshing.load(Ordering::Acquire) {
                    tokio::task::yield_now().await;
                }
                assert_eq!(cache.get(&store, gen.clone()).await.unwrap().len(), 2);

                // A caller that must see its own write waits for it.
                store.put(item(3)).await.unwrap();
                epoch.fetch_add(1, Ordering::Relaxed);
                let fresh = cache
                    .get_fresh(&store, (epoch.load(Ordering::Relaxed), 0))
                    .await
                    .unwrap();
                assert_eq!(fresh.len(), 3);
            });
        });
        assert_eq!(count(&snap, "miss"), 2, "cold load + fresh read");
        assert_eq!(count(&snap, "stale"), 2);
        assert_eq!(count(&snap, "hit"), 2);
    }
}
