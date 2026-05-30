#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

#[derive(Default)]
struct MemStore {
    inner: Mutex<HashMap<MediaId, MediaItem>>,
    states: Mutex<HashMap<MediaId, ScanState>>,
    next_scan_id: std::sync::atomic::AtomicI64,
}

impl MediaStore for MemStore {
    async fn get(&self, id: MediaId) -> DomainResult<MediaItem> {
        self.inner
            .lock()
            .map_err(|e| DomainError::Backend(e.to_string()))?
            .get(&id)
            .cloned()
            .ok_or(DomainError::NotFound(id))
    }
    async fn put(&self, item: MediaItem) -> DomainResult<()> {
        self.inner
            .lock()
            .map_err(|e| DomainError::Backend(e.to_string()))?
            .insert(item.id, item);
        Ok(())
    }
    async fn list(&self) -> DomainResult<Vec<MediaItem>> {
        Ok(self
            .inner
            .lock()
            .map_err(|e| DomainError::Backend(e.to_string()))?
            .values()
            .cloned()
            .collect())
    }

    async fn scan_state(&self, id: MediaId) -> DomainResult<Option<ScanState>> {
        Ok(self
            .states
            .lock()
            .map_err(|e| DomainError::Backend(e.to_string()))?
            .get(&id)
            .copied())
    }

    async fn begin_scan(&self, _root: &Path) -> DomainResult<i64> {
        Ok(self
            .next_scan_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1)
    }

    async fn mark_seen(
        &self,
        id: MediaId,
        scan_id: i64,
        mtime: i64,
        size: u64,
    ) -> DomainResult<()> {
        self.states
            .lock()
            .map_err(|e| DomainError::Backend(e.to_string()))?
            .insert(
                id,
                ScanState {
                    last_scanned: 0,
                    file_mtime: mtime,
                    file_size: size,
                    last_seen_scan_id: scan_id,
                },
            );
        Ok(())
    }

    async fn sweep_unseen(&self, scan_id: i64, root_prefix: &str) -> DomainResult<Vec<MediaId>> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        let states = self
            .states
            .lock()
            .map_err(|e| DomainError::Backend(e.to_string()))?;
        let doomed: Vec<MediaId> = inner
            .iter()
            .filter(|(id, item)| {
                item.path.to_string_lossy().starts_with(root_prefix)
                    && states.get(*id).map(|s| s.last_seen_scan_id) != Some(scan_id)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in &doomed {
            inner.remove(id);
        }
        Ok(doomed)
    }

    async fn finish_scan(
        &self,
        _scan_id: i64,
        _items_seen: i64,
        _items_swept: i64,
    ) -> DomainResult<()> {
        Ok(())
    }
}

struct NoopScanner;

impl Scanner for NoopScanner {
    async fn scan(&self, _root: &Path) -> DomainResult<Vec<MediaItem>> {
        Ok(vec![])
    }
}

fn sample() -> MediaItem {
    MediaItem {
        id: 1,
        path: "/m/a.mkv".into(),
        title: "A".into(),
        kind: MediaKind::Movie,
        ..Default::default()
    }
}

#[test]
fn media_probe_run_time_ticks_converts_ms_to_jellyfin_ticks() {
    let p = MediaProbe {
        duration_ms: Some(2_500),
        ..Default::default()
    };
    assert_eq!(p.run_time_ticks(), Some(25_000_000));
}

#[test]
fn media_probe_frame_rate_round_trips_through_mille() {
    let p = MediaProbe {
        frame_rate_mille: Some(23_976),
        ..Default::default()
    };
    let fps = p.frame_rate_f32().expect("set");
    assert!((fps - 23.976).abs() < 0.001, "got {fps}");
}

#[tokio::test]
async fn store_put_then_get_roundtrip() {
    let s = MemStore::default();
    s.put(sample()).await.unwrap();
    let got = s.get(1).await.unwrap();
    assert_eq!(got, sample());
}

#[tokio::test]
async fn store_get_missing_is_not_found() {
    let s = MemStore::default();
    match s.get(99).await {
        Err(DomainError::NotFound(99)) => {}
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[tokio::test]
async fn scanner_callable_via_generic_bound() {
    async fn drive<S: Scanner>(s: &S) -> Vec<MediaItem> {
        s.scan(Path::new("/")).await.unwrap()
    }
    assert!(drive(&NoopScanner).await.is_empty());
}
