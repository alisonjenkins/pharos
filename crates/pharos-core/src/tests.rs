#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

#[derive(Default)]
struct MemStore {
    inner: Mutex<HashMap<MediaId, MediaItem>>,
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
