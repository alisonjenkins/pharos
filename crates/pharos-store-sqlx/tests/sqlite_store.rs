#![cfg(feature = "sqlite")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pharos_core::{DomainError, MediaItem, MediaKind, MediaStore};
use pharos_store_sqlx::sqlite::SqliteStore;
use std::sync::Arc;

async fn fresh() -> SqliteStore {
    SqliteStore::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite")
}

fn item(id: u64, path: &str, title: &str, kind: MediaKind) -> MediaItem {
    MediaItem {
        id,
        path: path.into(),
        title: title.into(),
        kind,
    }
}

#[tokio::test]
async fn put_then_get_roundtrip() {
    let s = fresh().await;
    let it = item(1, "/m/a.mkv", "A", MediaKind::Movie);
    s.put(it.clone()).await.unwrap();
    let got = s.get(1).await.unwrap();
    assert_eq!(got, it);
}

#[tokio::test]
async fn get_missing_is_not_found() {
    let s = fresh().await;
    match s.get(42).await {
        Err(DomainError::NotFound(42)) => {}
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[tokio::test]
async fn list_returns_all_in_id_order() {
    let s = fresh().await;
    s.put(item(2, "/m/b.mkv", "B", MediaKind::Movie))
        .await
        .unwrap();
    s.put(item(1, "/m/a.mkv", "A", MediaKind::Movie))
        .await
        .unwrap();
    s.put(item(3, "/m/c.flac", "C", MediaKind::Audio))
        .await
        .unwrap();
    let all = s.list().await.unwrap();
    let ids: Vec<u64> = all.iter().map(|i| i.id).collect();
    assert_eq!(ids, vec![1, 2, 3]);
}

#[tokio::test]
async fn upsert_overwrites_existing_id() {
    let s = fresh().await;
    s.put(item(7, "/m/x.mkv", "old", MediaKind::Movie))
        .await
        .unwrap();
    s.put(item(7, "/m/x.mkv", "new", MediaKind::Movie))
        .await
        .unwrap();
    let got = s.get(7).await.unwrap();
    assert_eq!(got.title, "new");
}

#[tokio::test]
async fn concurrent_puts_do_not_lose_data() {
    // V10: store writes atomic per logical op. Spawn N puts in parallel;
    // every id observed afterwards.
    let s = Arc::new(fresh().await);
    let n: u64 = 32;
    let mut handles = Vec::with_capacity(n as usize);
    for i in 1..=n {
        let s = s.clone();
        handles.push(tokio::spawn(async move {
            s.put(item(i, &format!("/m/{i}.mkv"), &format!("t{i}"), MediaKind::Movie))
                .await
        }));
    }
    for h in handles {
        h.await.unwrap().unwrap();
    }
    let all = s.list().await.unwrap();
    assert_eq!(all.len(), n as usize);
}

#[tokio::test]
async fn store_usable_via_generic_bound() {
    async fn drive<S: MediaStore>(s: &S, it: MediaItem) -> MediaItem {
        s.put(it.clone()).await.unwrap();
        s.get(it.id).await.unwrap()
    }
    let s = fresh().await;
    let got = drive(&s, item(1, "/m/a.mkv", "A", MediaKind::Movie)).await;
    assert_eq!(got.title, "A");
}
