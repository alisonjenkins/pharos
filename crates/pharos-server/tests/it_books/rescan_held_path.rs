//! B257 — a rescan meets a row that holds a file's path under an id other than
//! `stable_id(path)`: a seeded row, or a moved file rebound to its old id whose
//! bytes have since changed. Looking the row up by the path-derived id missed
//! it, and the insert under that id failed on `UNIQUE(media_items.path)`,
//! failing the whole rescan. The rescan now writes over the row that holds the
//! path, keeping its id.

use pharos_core::{MediaItem, MediaKind, MediaStore};
use pharos_scanner::fs::{stable_id, FsScanner, PathUpdate};
use pharos_server::state::Stores;
use tempfile::TempDir;

use super::book_end_to_end::{write_epub, NeverProber};

const HELD: u64 = 42;

async fn library_with_held_row() -> (TempDir, std::path::PathBuf, Stores) {
    let td = TempDir::new().unwrap();
    let epub = td.path().join("Dune.epub");
    write_epub(&epub);
    assert_ne!(HELD, stable_id(&epub));
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    stores
        .put(MediaItem {
            id: HELD,
            path: epub.clone(),
            title: "Seeded".into(),
            kind: MediaKind::Book,
            ..Default::default()
        })
        .await
        .unwrap();
    (td, epub, stores)
}

async fn the_one_row_is_held_and_scanned(stores: &Stores) {
    let all = MediaStore::list(stores).await.unwrap();
    assert_eq!(all.len(), 1, "one row per path: {all:?}");
    assert_eq!(all[0].id, HELD, "the row keeps its id");
    assert_eq!(
        all[0].book.as_ref().and_then(|b| b.author.as_deref()),
        Some("Frank Herbert"),
        "the scan wrote its result over the held row"
    );
}

#[actix_web::test]
async fn a_rescan_writes_over_a_row_holding_the_path_under_another_id() {
    let (td, _, stores) = library_with_held_row().await;
    let scanned = FsScanner::new(NeverProber)
        .scan_into(td.path(), &stores)
        .await
        .expect("the rescan must not fail on the held path");
    assert!(scanned.added.is_empty(), "{scanned:?}");
    assert_eq!(scanned.updated, [HELD], "{scanned:?}");
    the_one_row_is_held_and_scanned(&stores).await;
}

#[actix_web::test]
async fn a_watched_change_writes_over_a_row_holding_the_path_under_another_id() {
    let (td, epub, stores) = library_with_held_row().await;
    let scan_id = stores.begin_scan(td.path()).await.unwrap();
    let update = FsScanner::new(NeverProber)
        .update_path(&epub, &stores, scan_id)
        .await
        .expect("the watcher must not fail on the held path");
    assert_eq!(update, PathUpdate::Updated(HELD));
    the_one_row_is_held_and_scanned(&stores).await;
}
