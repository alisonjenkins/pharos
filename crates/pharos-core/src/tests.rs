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
    }
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
