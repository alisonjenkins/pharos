#![allow(clippy::unwrap_used, clippy::expect_used)]
//! B313 — the server starts one scan task per root, and a typed library inside
//! a mixed root (the B174 shape) is two roots over the same files. Run at
//! once, each scan stamps the files it sees with its own scan id, and the one
//! that finishes first sweeps every row under its root that the other has
//! since re-stamped: files still on disk vanished from the library until the
//! next rescan. Scans now run one at a time in the process (V253).

use pharos_core::{DomainResult, MediaStore, ProbeInfo, Prober};
use pharos_scanner::fs::FsScanner;
use pharos_server::state::Stores;
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;

/// A movie for every file, after a pause, so two scans interleave.
#[derive(Clone, Default)]
struct SlowProber;

impl Prober for SlowProber {
    async fn probe(&self, _path: &Path) -> DomainResult<ProbeInfo> {
        tokio::time::sleep(Duration::from_millis(2)).await;
        Ok(ProbeInfo {
            kind: pharos_core::MediaKind::Movie,
            probe: Default::default(),
        })
    }
}

#[actix_web::test]
async fn a_library_inside_a_root_keeps_its_files_when_both_scan_at_once() {
    let td = TempDir::new().unwrap();
    let inner = td.path().join("Films");
    std::fs::create_dir_all(&inner).unwrap();
    for n in 0..40 {
        std::fs::write(inner.join(format!("Film {n:02}.mkv")), format!("film {n}")).unwrap();
    }
    for n in 0..5 {
        std::fs::write(
            td.path().join(format!("Loose {n}.mkv")),
            format!("loose {n}"),
        )
        .unwrap();
    }
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let outer_scan = FsScanner::new(SlowProber);
    let inner_scan = FsScanner::new(SlowProber);

    let (outer, inner) = tokio::join!(
        outer_scan.scan_into(td.path(), &stores),
        inner_scan.scan_into(&inner, &stores),
    );
    let (outer, inner) = (outer.unwrap(), inner.unwrap());

    let listed = MediaStore::list(&stores).await.unwrap();
    assert_eq!(
        listed.len(),
        45,
        "every file on disk stays listed; outer removed {:?}, inner removed {:?}",
        outer.removed,
        inner.removed,
    );
    assert!(outer.removed.is_empty() && inner.removed.is_empty());
}
