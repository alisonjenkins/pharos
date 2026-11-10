//! Per-pass cache of TVDB's merged episode-list bodies (B989).
//!
//! A long show's episode list is several pages of a large body, and the
//! enrichment and translation passes read it once per episode. The cache
//! holds the merged body per series for the length of one pass: it is empty
//! until [`PassListCache::begin`], and [`PassListCache::end`] drops every
//! entry, so nothing is kept between passes (or while no pass runs).
//! Memory is bounded by [`MAX_ENTRIES`] and [`MAX_BYTES`]; the least
//! recently used entries go first.

use std::sync::{Arc, Mutex};

/// Most series held at once. A pass walks one show's episodes together, so a
/// handful would do; the margin keeps interleaved shows from thrashing.
pub(crate) const MAX_ENTRIES: usize = 32;

/// Most body bytes held at once. A ~1,100 episode list is about 2 MiB merged
/// and the node's RAM is scarce, so 16 MiB holds several long shows at
/// bounded cost. A body larger than this is never cached.
pub(crate) const MAX_BYTES: usize = 16 * 1024 * 1024;

/// What a pass did with the cache, for the log line at its end.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PassListStats {
    pub hits: u64,
    pub misses: u64,
    pub entries: usize,
    pub bytes: usize,
}

#[derive(Default)]
struct Pass {
    /// Least recently used first.
    entries: Vec<(String, Arc<str>)>,
    bytes: usize,
    hits: u64,
    misses: u64,
}

impl Pass {
    fn stats(&self) -> PassListStats {
        PassListStats {
            hits: self.hits,
            misses: self.misses,
            entries: self.entries.len(),
            bytes: self.bytes,
        }
    }
}

/// The cache. `Clone`s share one state; the lock is never held across an
/// await (every method is synchronous).
#[derive(Clone, Default)]
pub(crate) struct PassListCache {
    pass: Arc<Mutex<Option<Pass>>>,
}

impl PassListCache {
    fn with<R>(&self, f: impl FnOnce(&mut Option<Pass>) -> R) -> R {
        // A poisoned lock only means another thread panicked mid-update; the
        // cache holds no invariant a reader cannot rebuild, so keep going.
        let mut guard = self
            .pass
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut guard)
    }

    /// Open a pass: start from an empty cache.
    pub(crate) fn begin(&self) {
        self.with(|p| *p = Some(Pass::default()));
    }

    /// Close the pass, dropping every entry. `None` when no pass was open.
    pub(crate) fn end(&self) -> Option<PassListStats> {
        self.with(Option::take).map(|p| p.stats())
    }

    /// The cached body for `series_id`, counting a hit or a miss. Outside a
    /// pass this is always `None` and counts nothing.
    pub(crate) fn get(&self, series_id: &str) -> Option<Arc<str>> {
        let found = self.with(|p| {
            let pass = p.as_mut()?;
            let found = match pass.entries.iter().position(|(k, _)| k == series_id) {
                Some(i) => {
                    let entry = pass.entries.remove(i);
                    let body = Arc::clone(&entry.1);
                    pass.entries.push(entry);
                    pass.hits = pass.hits.saturating_add(1);
                    Some(body)
                }
                None => {
                    pass.misses = pass.misses.saturating_add(1);
                    None
                }
            };
            Some(found)
        })?;
        let outcome = if found.is_some() { "hit" } else { "miss" };
        metrics::counter!("pharos_tvdb_episode_list_cache_total", "outcome" => outcome)
            .increment(1);
        found
    }

    /// Keep `body` for `series_id` for the rest of the pass, evicting the
    /// least recently used entries to stay within the caps. Outside a pass,
    /// or for a body over [`MAX_BYTES`], nothing is kept.
    pub(crate) fn insert(&self, series_id: &str, body: Arc<str>) {
        self.with(|p| {
            let Some(pass) = p.as_mut() else { return };
            if body.len() > MAX_BYTES {
                return;
            }
            if let Some(i) = pass.entries.iter().position(|(k, _)| k == series_id) {
                let (_, old) = pass.entries.remove(i);
                pass.bytes = pass.bytes.saturating_sub(old.len());
            }
            while !pass.entries.is_empty()
                && (pass.entries.len() >= MAX_ENTRIES
                    || pass.bytes.saturating_add(body.len()) > MAX_BYTES)
            {
                let (_, old) = pass.entries.remove(0);
                pass.bytes = pass.bytes.saturating_sub(old.len());
            }
            pass.bytes = pass.bytes.saturating_add(body.len());
            pass.entries.push((series_id.to_string(), body));
        });
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};

    fn body(n: usize) -> Arc<str> {
        Arc::from("x".repeat(n))
    }

    fn count(snap: &pharos_test_metrics::CumulativeSnapshotter, outcome: &str) -> u64 {
        snap.snapshot()
            .into_vec()
            .into_iter()
            .filter(|(ck, ..)| {
                ck.key().name() == "pharos_tvdb_episode_list_cache_total"
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

    #[test]
    fn outside_a_pass_nothing_is_kept_or_counted() {
        let c = PassListCache::default();
        c.insert("910001", body(10));
        assert!(c.get("910001").is_none());
        assert!(c.end().is_none());
    }

    #[test]
    fn a_hit_after_an_insert_and_counters_move() {
        let recorder = DebuggingRecorder::new();
        let snap = pharos_test_metrics::snapshotter(&recorder);
        metrics::with_local_recorder(&recorder, || {
            let c = PassListCache::default();
            c.begin();
            assert!(c.get("910001").is_none());
            c.insert("910001", body(10));
            assert_eq!(c.get("910001").as_deref().map(str::len), Some(10));
            assert_eq!(c.get("910001").as_deref().map(str::len), Some(10));
            let stats = c.end().expect("pass");
            assert_eq!(
                stats,
                PassListStats {
                    hits: 2,
                    misses: 1,
                    entries: 1,
                    bytes: 10
                }
            );
        });
        assert_eq!(count(&snap, "hit"), 2);
        assert_eq!(count(&snap, "miss"), 1);
    }

    #[test]
    fn nothing_survives_into_the_next_pass() {
        let c = PassListCache::default();
        c.begin();
        c.insert("910001", body(10));
        c.end();
        assert!(c.get("910001").is_none(), "closed pass keeps nothing");
        c.begin();
        assert!(c.get("910001").is_none(), "new pass starts empty");
        assert_eq!(c.end().expect("pass").entries, 0);
    }

    #[test]
    fn the_entry_cap_evicts_the_least_recently_used() {
        let c = PassListCache::default();
        c.begin();
        for i in 0..MAX_ENTRIES {
            c.insert(&format!("{}", 910_000 + i), body(1));
        }
        // Touch the oldest so the second oldest is the one evicted.
        assert!(c.get("910000").is_some());
        c.insert("919999", body(1));
        assert!(c.get("910000").is_some());
        assert!(c.get("910001").is_none());
        assert_eq!(c.end().expect("pass").entries, MAX_ENTRIES);
    }

    #[test]
    fn the_byte_cap_is_honoured() {
        let c = PassListCache::default();
        c.begin();
        let third = MAX_BYTES / 3 + 1;
        c.insert("910001", body(third));
        c.insert("910002", body(third));
        c.insert("910003", body(third));
        let stats = c.end().expect("pass");
        assert!(stats.bytes <= MAX_BYTES, "{stats:?}");
        assert_eq!(stats.entries, 2);
    }

    #[test]
    fn a_body_over_the_cap_is_not_kept() {
        let c = PassListCache::default();
        c.begin();
        c.insert("910001", body(MAX_BYTES + 1));
        assert!(c.get("910001").is_none());
        assert_eq!(c.end().expect("pass").bytes, 0);
    }
}
