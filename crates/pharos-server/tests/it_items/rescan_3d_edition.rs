#![allow(clippy::unwrap_used, clippy::expect_used)]
//! B289 — a 3D rip grouped under its 2D movie is stored on the primary's
//! `alternate_sources`, and `has_3d` is computed from it when the row is put.
//! An incremental rescan used to skip a primary whose own file was unchanged
//! without looking at its editions, so deleting the 3D file left the item
//! listing a source that no longer existed and still matching `Is3D`.

use pharos_core::{
    DomainResult, MediaFilters, MediaQuery, MediaStore, ProbeInfo, Prober, Video3DFormat,
};
use pharos_scanner::fs::{stable_id, FsScanner};
use pharos_server::state::Stores;
use std::path::Path;
use tempfile::TempDir;

#[derive(Clone, Default)]
struct MovieProber;

impl Prober for MovieProber {
    async fn probe(&self, _path: &Path) -> DomainResult<ProbeInfo> {
        Ok(ProbeInfo {
            kind: pharos_core::MediaKind::Movie,
            probe: Default::default(),
        })
    }
}

async fn is_3d_ids(stores: &Stores) -> Vec<u64> {
    let q = MediaQuery {
        filters: MediaFilters {
            is_3d: Some(true),
            ..Default::default()
        },
        ..Default::default()
    };
    stores
        .query(&q)
        .await
        .unwrap()
        .0
        .into_iter()
        .map(|i| i.id)
        .collect()
}

#[actix_web::test]
async fn a_deleted_3d_edition_stops_matching_is_3d_after_a_plain_rescan() {
    let td = TempDir::new().unwrap();
    let dir = td.path().join("Film (2010)");
    std::fs::create_dir_all(&dir).unwrap();
    let primary = dir.join("Film (2010).mkv");
    let rip = dir.join("Film (2010) - 3D.HSBS.mkv");
    std::fs::write(&primary, b"2d").unwrap();
    std::fs::write(&rip, b"3dddd").unwrap();
    let id = stable_id(&primary);

    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let scanner = FsScanner::new(MovieProber);
    scanner.scan_into(td.path(), &stores).await.unwrap();
    let item = stores.get(id).await.unwrap();
    assert_eq!(
        item.probe.alternate_sources[0].video_3d_format,
        Some(Video3DFormat::HalfSideBySide)
    );
    assert_eq!(is_3d_ids(&stores).await, [id]);

    std::fs::remove_file(&rip).unwrap();
    scanner.scan_into(td.path(), &stores).await.unwrap();

    assert!(stores
        .get(id)
        .await
        .unwrap()
        .probe
        .alternate_sources
        .is_empty());
    assert!(
        is_3d_ids(&stores).await.is_empty(),
        "no 3D file is left, so Is3D must not match"
    );
}
