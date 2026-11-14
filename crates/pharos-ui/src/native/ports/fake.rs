//! In-memory hosts for the ports, for tests. They behave like a real host in
//! the ways the UI relies on: commands change the snapshot, and the snapshot
//! reaches every receiver.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use tokio::sync::watch;

use super::{
    ContainerRef, Download, NativeAudio, OfflineError, OfflineStore, PlayState, PlayerSnapshot,
    TrackDownload, TrackState,
};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

pub struct FakeAudio {
    tx: watch::Sender<PlayerSnapshot>,
}

impl FakeAudio {
    pub fn new() -> Self {
        Self {
            tx: watch::Sender::new(PlayerSnapshot::default()),
        }
    }

    /// The engine learning the current track's length.
    pub fn set_duration(&self, duration_ms: u64) {
        self.tx.send_modify(|s| s.duration_ms = Some(duration_ms));
    }

    pub fn snapshot(&self) -> PlayerSnapshot {
        self.tx.borrow().clone()
    }

    fn go_to(s: &mut PlayerSnapshot, index: usize) {
        s.index = Some(index);
        s.current_id = s.queue.get(index).cloned();
        s.position_ms = 0;
        s.duration_ms = None;
        s.state = PlayState::Playing;
        s.error = None;
    }
}

impl Default for FakeAudio {
    fn default() -> Self {
        Self::new()
    }
}

impl NativeAudio for FakeAudio {
    fn play(&self, ids: Vec<String>, start_index: usize) {
        self.tx.send_modify(|s| {
            if start_index >= ids.len() {
                s.error = Some(format!(
                    "start index {start_index} is outside a queue of {}",
                    ids.len()
                ));
                return;
            }
            s.queue = ids;
            Self::go_to(s, start_index);
        });
    }

    fn pause(&self) {
        self.tx.send_modify(|s| {
            if s.state == PlayState::Playing {
                s.state = PlayState::Paused;
            }
        });
    }

    fn resume(&self) {
        self.tx.send_modify(|s| {
            if s.state == PlayState::Paused {
                s.state = PlayState::Playing;
            }
        });
    }

    fn seek(&self, position_ms: u64) {
        self.tx.send_modify(|s| {
            if s.current_id.is_none() {
                return;
            }
            s.position_ms = s.duration_ms.map_or(position_ms, |d| position_ms.min(d));
        });
    }

    fn next(&self) {
        self.tx.send_modify(|s| {
            let Some(i) = s.index else { return };
            let n = i.saturating_add(1);
            if n < s.queue.len() {
                Self::go_to(s, n);
            } else {
                s.state = PlayState::Ended;
            }
        });
    }

    fn previous(&self) {
        self.tx.send_modify(|s| {
            let Some(i) = s.index else { return };
            if i > 0 {
                Self::go_to(s, i.saturating_sub(1));
            } else {
                s.position_ms = 0;
            }
        });
    }

    fn stop(&self) {
        self.tx.send_modify(|s| *s = PlayerSnapshot::default());
    }

    fn state(&self) -> watch::Receiver<PlayerSnapshot> {
        self.tx.subscribe()
    }
}

/// 4 GiB, the default cap in the plan.
const DEFAULT_CAP_BYTES: u64 = 4 * 1024 * 1024 * 1024;

struct StoreState {
    cap: u64,
    free: u64,
    /// What the server would list for each container: track id and size.
    catalogue: HashMap<ContainerRef, Vec<(String, u64)>>,
    pinned: Vec<Download>,
}

pub struct FakeOfflineStore {
    inner: Mutex<StoreState>,
}

impl FakeOfflineStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(StoreState {
                cap: DEFAULT_CAP_BYTES,
                free: u64::MAX,
                catalogue: HashMap::new(),
                pinned: Vec::new(),
            }),
        }
    }

    /// Makes `container` pinnable, with these tracks.
    pub fn add_container(&self, container: ContainerRef, tracks: Vec<(String, u64)>) {
        lock(&self.inner).catalogue.insert(container, tracks);
    }

    pub fn set_free_bytes(&self, bytes: u64) {
        lock(&self.inner).free = bytes;
    }

    /// Marks a pinned track as failed, as a dropped download would.
    pub fn fail_track(&self, track_id: &str) {
        for d in &mut lock(&self.inner).pinned {
            for t in &mut d.tracks {
                if t.id == track_id {
                    t.state = TrackState::Failed;
                }
            }
        }
    }

    fn used(st: &StoreState) -> u64 {
        st.pinned
            .iter()
            .fold(0u64, |sum, d| sum.saturating_add(d.cached_bytes()))
    }
}

impl Default for FakeOfflineStore {
    fn default() -> Self {
        Self::new()
    }
}

impl OfflineStore for FakeOfflineStore {
    fn cache_cap_bytes(&self) -> u64 {
        lock(&self.inner).cap
    }

    fn set_cache_cap_bytes(&self, bytes: u64) -> Result<(), OfflineError> {
        lock(&self.inner).cap = bytes;
        Ok(())
    }

    fn used_bytes(&self) -> u64 {
        Self::used(&lock(&self.inner))
    }

    fn free_bytes(&self) -> u64 {
        lock(&self.inner).free
    }

    fn pin(&self, container: &ContainerRef) -> Result<(), OfflineError> {
        let mut st = lock(&self.inner);
        if st.pinned.iter().any(|d| &d.container == container) {
            return Ok(());
        }
        let Some(tracks) = st.catalogue.get(container).cloned() else {
            return Err(OfflineError::UnknownContainer {
                kind: container.kind,
                id: container.id.clone(),
            });
        };
        let needed = tracks
            .iter()
            .fold(0u64, |sum, (_, b)| sum.saturating_add(*b));
        let available = st.cap.saturating_sub(Self::used(&st));
        if needed > available {
            return Err(OfflineError::OverCap { needed, available });
        }
        st.pinned.push(Download {
            container: container.clone(),
            tracks: tracks
                .into_iter()
                .map(|(id, bytes)| TrackDownload {
                    id,
                    state: TrackState::Cached,
                    bytes,
                })
                .collect(),
        });
        Ok(())
    }

    fn unpin(&self, container: &ContainerRef) -> Result<(), OfflineError> {
        lock(&self.inner)
            .pinned
            .retain(|d| &d.container != container);
        Ok(())
    }

    fn downloads(&self) -> Vec<Download> {
        lock(&self.inner).pinned.clone()
    }

    fn is_cached(&self, track_id: &str) -> bool {
        lock(&self.inner).pinned.iter().any(|d| {
            d.tracks
                .iter()
                .any(|t| t.id == track_id && t.state == TrackState::Cached)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::ports::ContainerKind;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    fn album(id: &str) -> ContainerRef {
        ContainerRef {
            kind: ContainerKind::Album,
            id: id.into(),
        }
    }

    #[test]
    fn play_publishes_the_queue_and_current_track() {
        let a = FakeAudio::new();
        let rx = a.state();
        a.play(ids(&["t1", "t2", "t3"]), 1);
        let s = rx.borrow().clone();
        assert_eq!(s.state, PlayState::Playing);
        assert_eq!(s.current_id.as_deref(), Some("t2"));
        assert_eq!(s.index, Some(1));
        assert_eq!(s.queue, ids(&["t1", "t2", "t3"]));
        assert_eq!(s.position_ms, 0);
    }

    #[test]
    fn a_start_index_outside_the_queue_reports_its_value() {
        let a = FakeAudio::new();
        a.play(ids(&["t1"]), 5);
        let s = a.snapshot();
        assert_eq!(s.state, PlayState::Idle);
        assert_eq!(
            s.error.as_deref(),
            Some("start index 5 is outside a queue of 1")
        );
    }

    #[test]
    fn pause_and_resume_toggle_only_from_the_right_state() {
        let a = FakeAudio::new();
        a.resume();
        assert_eq!(a.snapshot().state, PlayState::Idle);
        a.play(ids(&["t1"]), 0);
        a.pause();
        assert_eq!(a.snapshot().state, PlayState::Paused);
        a.resume();
        assert_eq!(a.snapshot().state, PlayState::Playing);
    }

    #[test]
    fn seek_clamps_to_a_known_duration() {
        let a = FakeAudio::new();
        a.seek(500);
        assert_eq!(a.snapshot().position_ms, 0, "nothing is loaded");
        a.play(ids(&["t1"]), 0);
        a.set_duration(1_000);
        a.seek(400);
        assert_eq!(a.snapshot().position_ms, 400);
        a.seek(9_999);
        assert_eq!(a.snapshot().position_ms, 1_000);
    }

    #[test]
    fn next_walks_the_queue_then_ends() {
        let a = FakeAudio::new();
        a.play(ids(&["t1", "t2"]), 0);
        a.next();
        assert_eq!(a.snapshot().current_id.as_deref(), Some("t2"));
        a.next();
        let s = a.snapshot();
        assert_eq!(s.state, PlayState::Ended);
        assert_eq!(s.current_id.as_deref(), Some("t2"));
    }

    #[test]
    fn previous_goes_back_and_at_the_start_rewinds() {
        let a = FakeAudio::new();
        a.play(ids(&["t1", "t2"]), 1);
        a.previous();
        assert_eq!(a.snapshot().current_id.as_deref(), Some("t1"));
        a.set_duration(1_000);
        a.seek(700);
        a.previous();
        let s = a.snapshot();
        assert_eq!(s.current_id.as_deref(), Some("t1"));
        assert_eq!(s.position_ms, 0);
    }

    #[test]
    fn stop_clears_the_snapshot() {
        let a = FakeAudio::new();
        a.play(ids(&["t1"]), 0);
        a.stop();
        assert_eq!(a.snapshot(), PlayerSnapshot::default());
    }

    #[tokio::test]
    async fn a_receiver_is_woken_by_a_command() {
        let a = FakeAudio::new();
        let mut rx = a.state();
        a.play(ids(&["t1"]), 0);
        assert!(rx.changed().await.is_ok());
        assert_eq!(rx.borrow_and_update().state, PlayState::Playing);
    }

    fn store() -> FakeOfflineStore {
        let s = FakeOfflineStore::new();
        s.add_container(album("a"), vec![("t1".into(), 100), ("t2".into(), 200)]);
        s.add_container(album("b"), vec![("t3".into(), 700)]);
        s
    }

    #[test]
    fn the_default_cap_is_four_gib() {
        assert_eq!(FakeOfflineStore::new().cache_cap_bytes(), 4 << 30);
    }

    #[test]
    fn pin_caches_tracks_and_counts_used_bytes() {
        let s = store();
        assert!(!s.is_cached("t1"));
        assert_eq!(s.pin(&album("a")), Ok(()));
        assert!(s.is_cached("t1") && s.is_cached("t2") && !s.is_cached("t3"));
        assert_eq!(s.used_bytes(), 300);
        assert_eq!(s.downloads().len(), 1);
        assert_eq!(s.pin(&album("a")), Ok(()), "pinning twice is a no-op");
        assert_eq!(s.downloads().len(), 1);
    }

    #[test]
    fn unpin_frees_the_bytes() {
        let s = store();
        assert_eq!(s.pin(&album("a")), Ok(()));
        assert_eq!(s.unpin(&album("a")), Ok(()));
        assert_eq!(s.used_bytes(), 0);
        assert!(s.downloads().is_empty());
        assert!(!s.is_cached("t1"));
    }

    #[test]
    fn pin_over_the_cap_is_refused_with_both_numbers() {
        let s = store();
        assert_eq!(s.set_cache_cap_bytes(500), Ok(()));
        assert_eq!(s.pin(&album("a")), Ok(()));
        assert_eq!(
            s.pin(&album("b")),
            Err(OfflineError::OverCap {
                needed: 700,
                available: 200
            })
        );
    }

    #[test]
    fn pin_of_an_unknown_container_names_it() {
        let s = store();
        let err = s.pin(&album("zzz")).err();
        assert_eq!(
            err.map(|e| e.to_string()),
            Some("unknown album \"zzz\"".to_string())
        );
    }

    #[test]
    fn a_failed_track_is_not_cached_and_not_counted() {
        let s = store();
        assert_eq!(s.pin(&album("a")), Ok(()));
        s.fail_track("t2");
        assert!(!s.is_cached("t2"));
        assert_eq!(s.used_bytes(), 100);
    }

    #[test]
    fn free_bytes_is_what_the_host_reports() {
        let s = store();
        s.set_free_bytes(123);
        assert_eq!(s.free_bytes(), 123);
    }
}
