//! `MediaStore::get_many` answers in the caller's id order and skips ids with
//! no row (B811 / V761).

use pharos_core::{MediaId, MediaItem, MediaKind, MediaStore};
use pharos_store_sqlx::sqlite::SqliteStore;

fn item(id: MediaId) -> MediaItem {
    MediaItem {
        id,
        path: format!("/gm/{id}.mkv").into(),
        title: format!("Item {id}"),
        kind: MediaKind::Movie,
        ..Default::default()
    }
}

#[tokio::test]
async fn returns_rows_in_request_order_and_skips_missing_ids() {
    let store = SqliteStore::connect("sqlite::memory:").await.unwrap();
    for id in [10, 20, 30] {
        store.put(item(id)).await.unwrap();
    }
    let got = store.get_many(&[30, 999, 10, 20]).await.unwrap();
    let ids: Vec<MediaId> = got.iter().map(|i| i.id).collect();
    assert_eq!(ids, vec![30, 10, 20]);
    assert_eq!(got[0], store.get(30).await.unwrap());
}

#[tokio::test]
async fn empty_request_is_empty() {
    let store = SqliteStore::connect("sqlite::memory:").await.unwrap();
    assert!(store.get_many(&[]).await.unwrap().is_empty());
}

#[tokio::test]
async fn spans_more_than_one_chunk() {
    let store = SqliteStore::connect("sqlite::memory:").await.unwrap();
    let ids: Vec<MediaId> = (1..=1200).collect();
    let items: Vec<MediaItem> = ids.iter().map(|&i| item(i)).collect();
    store.put_batch(&items).await.unwrap();
    let mut want = ids.clone();
    want.reverse();
    let got: Vec<MediaId> = store
        .get_many(&want)
        .await
        .unwrap()
        .iter()
        .map(|i| i.id)
        .collect();
    assert_eq!(got, want);
}
