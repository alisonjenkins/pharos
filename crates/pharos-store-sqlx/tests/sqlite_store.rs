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
        ..Default::default()
    }
}

#[tokio::test]
async fn put_then_get_roundtrip() {
    let s = fresh().await;
    let it = item(1, "/m/a.mkv", "A", MediaKind::Movie);
    s.put(it.clone()).await.unwrap();
    let got = s.get(1).await.unwrap();
    // `created_at` is server-stamped on first insert — strip both
    // sides of that field before equality compare.
    let mut got_no_ts = got.clone();
    got_no_ts.created_at = None;
    let mut it_no_ts = it.clone();
    it_no_ts.created_at = None;
    assert_eq!(got_no_ts, it_no_ts);
    assert!(got.created_at.is_some(), "store should stamp created_at");
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
            s.put(item(
                i,
                &format!("/m/{i}.mkv"),
                &format!("t{i}"),
                MediaKind::Movie,
            ))
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
async fn scan_state_round_trips_through_mark_seen() {
    let s = fresh().await;
    s.put(item(1, "/a/x.mkv", "X", MediaKind::Movie))
        .await
        .unwrap();
    // No signature recorded yet (predates first scan).
    let st = s.scan_state(1).await.unwrap().expect("row present");
    assert_eq!(st.file_mtime, 0);
    assert_eq!(st.file_size, 0);
    assert_eq!(st.last_seen_scan_id, 0);

    let scan = s.begin_scan(std::path::Path::new("/a")).await.unwrap();
    s.mark_seen(1, scan, 1_700_000_000, 4242).await.unwrap();
    let st = s.scan_state(1).await.unwrap().expect("row present");
    assert_eq!(st.file_mtime, 1_700_000_000);
    assert_eq!(st.file_size, 4242);
    assert_eq!(st.last_seen_scan_id, scan);
    assert!(st.last_scanned > 0, "mark_seen stamps last_scanned");

    // Absent row -> None.
    assert!(s.scan_state(999).await.unwrap().is_none());
}

#[tokio::test]
async fn sweep_unseen_deletes_only_unseen_under_root() {
    let s = fresh().await;
    s.put(item(1, "/a/keep.mkv", "Keep", MediaKind::Movie))
        .await
        .unwrap();
    s.put(item(2, "/a/gone.mkv", "Gone", MediaKind::Movie))
        .await
        .unwrap();

    let scan = s.begin_scan(std::path::Path::new("/a")).await.unwrap();
    // Only item 1 is seen this run; item 2 was deleted on disk.
    s.mark_seen(1, scan, 100, 10).await.unwrap();

    let swept = s.sweep_unseen(scan, "/a").await.unwrap();
    assert_eq!(swept, vec![2]);
    assert!(s.get(1).await.is_ok(), "seen item survives");
    match s.get(2).await {
        Err(DomainError::NotFound(2)) => {}
        other => panic!("unseen item should be gone, got {other:?}"),
    }
    s.finish_scan(scan, 1, swept.len() as i64).await.unwrap();
}

#[tokio::test]
async fn sweep_is_root_scoped_and_never_touches_sibling_root() {
    // V10 / brief: sweeping root A must not delete a root-B item even
    // though B's row was never marked by A's scan.
    let s = fresh().await;
    s.put(item(1, "/rootA/a.mkv", "A", MediaKind::Movie))
        .await
        .unwrap();
    s.put(item(2, "/rootB/b.mkv", "B", MediaKind::Movie))
        .await
        .unwrap();

    // Scan rootA, mark nothing under it (simulate everything deleted).
    let scan = s.begin_scan(std::path::Path::new("/rootA")).await.unwrap();
    let swept = s.sweep_unseen(scan, "/rootA").await.unwrap();
    assert_eq!(swept, vec![1], "only rootA item swept");
    assert!(
        s.get(2).await.is_ok(),
        "sibling root B item must be untouched"
    );
}

#[tokio::test]
async fn sweep_respects_path_boundary_not_string_prefix() {
    // Regression: sweeping /media/movies must NOT touch /media/movies-4k
    // (a sibling whose name merely shares a string prefix). Pre-fix, the
    // `path LIKE prefix || '%'` matched it.
    let s = fresh().await;
    s.put(item(1, "/media/movies/old.mkv", "Old", MediaKind::Movie))
        .await
        .unwrap();
    s.put(item(
        2,
        "/media/movies-4k/keep.mkv",
        "Keep",
        MediaKind::Movie,
    ))
    .await
    .unwrap();

    // Scan /media/movies, mark nothing (all gone on disk).
    let scan = s
        .begin_scan(std::path::Path::new("/media/movies"))
        .await
        .unwrap();
    let swept = s.sweep_unseen(scan, "/media/movies").await.unwrap();
    assert_eq!(swept, vec![1], "only the real /media/movies item swept");
    assert!(
        s.get(2).await.is_ok(),
        "/media/movies-4k must survive a /media/movies sweep"
    );
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
