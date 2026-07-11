//! LIB-A9 — graceful-fallback integration test for tiered library
//! change-detection.
//!
//! Drives the *production* `spawn_for_roots` wiring with the watch tier forced
//! OFF (`watch_enabled = false`), so every root takes the **periodic
//! incremental-rescan** fallback tier — exactly the path a network/fuse root
//! (or a `watch`-feature-less build) takes. Asserts that a file created after
//! startup is picked up on the next poll and that the same `LibraryChanged`
//! delta the manual `/Library/Refresh` emits is broadcast on the `/socket`
//! bus.
//!
//! Uses a fake `Prober` (no ffmpeg / real media needed): `spawn_for_roots` is
//! generic over the prober, so the test substitutes one that returns a movie
//! `ProbeInfo` for any path, while the rest of the path — `scan_into`, the
//! in-memory `SqliteStore`, the `AppState` broadcast — is the real production
//! code.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::time::Duration;

use actix_web::web;
use pharos_core::{DomainResult, MediaKind, ProbeInfo};
use pharos_scanner::FsScanner;
use pharos_scanner::RootWatchability;
use pharos_server::{
    library_watch::{plan_mode, spawn_for_roots, RootMode, WatchConfig},
    state::{AppState, SocketBroadcast, Stores},
};

/// Minimal prober: classifies anything as a movie with an empty probe block.
/// Lets the scan exercise the full put/mark_seen/broadcast path without
/// spawning ffprobe.
#[derive(Clone, Default)]
struct FakeProber;

impl pharos_core::Prober for FakeProber {
    async fn probe(&self, _path: &Path) -> DomainResult<ProbeInfo> {
        Ok(ProbeInfo {
            kind: MediaKind::Movie,
            probe: Default::default(),
        })
    }
}

#[actix_web::test]
async fn forced_unsupported_root_uses_periodic_and_picks_up_new_file() {
    // The decision under test: a non-watchable (network/fuse) root, or one with
    // the watch flag off, must plan the Periodic tier — never Watch.
    let cfg = WatchConfig {
        watch_enabled: false,
        poll_interval: Duration::from_secs(1),
        rate_limit_ms: 0,
    };
    // Even pretending the feature is built + the fs is watchable, the disabled
    // flag forces periodic; and a genuinely network root would too.
    assert_eq!(
        plan_mode(RootWatchability::Watchable, &cfg, true),
        RootMode::Periodic,
        "watch-disabled must fall back to periodic",
    );
    assert_eq!(
        plan_mode(RootWatchability::Network, &cfg, true),
        RootMode::Periodic,
        "a network root must fall back to periodic even with watch on",
    );

    // Now exercise the real spawn + rescan + broadcast wiring.
    let td = tempfile::TempDir::new().unwrap();
    let root = td.path().to_path_buf();

    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let state = AppState::new(stores, "a9-fallback".into()).with_media_roots(vec![root.clone()]);
    let state = web::Data::new(state);

    // Subscribe to the broadcast bus BEFORE spawning so we don't miss the
    // delta. (notify_library_delta no-ops with zero subscribers.)
    let mut bus_rx = state.bus.subscribe();

    let _guards = spawn_for_roots(state.clone(), std::slice::from_ref(&root), cfg, || {
        FsScanner::new(FakeProber)
    });

    // Create a media file *after* the poller is up. The boot scan already ran
    // (on the then-empty dir), and the first periodic tick is one interval
    // away, so this file is indexed by the *periodic* rescan — the fallback
    // tier this test exists to prove. (The boot scan itself is covered by
    // `boot_scan_indexes_preexisting_file_before_first_poll`.)
    tokio::fs::write(root.join("movie.mkv"), b"fallback-bytes")
        .await
        .unwrap();

    let expected_id = pharos_scanner::stable_id(&root.join("movie.mkv"));

    // The periodic rescan (1s interval) should index the file and broadcast a
    // LibraryChanged delta carrying its id within a few intervals.
    let got_delta = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match bus_rx.recv().await {
                Ok(SocketBroadcast::LibraryChanged { added, .. }) => {
                    if added.contains(&expected_id.to_string()) {
                        return true;
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => return false,
            }
        }
    })
    .await
    .unwrap_or(false);

    assert!(
        got_delta,
        "periodic rescan should index the new file and broadcast its added id"
    );

    // And the row is actually in the store.
    use pharos_core::MediaStore;
    assert!(
        state.stores.get(expected_id).await.is_ok(),
        "the new file should be persisted by the periodic rescan"
    );
}

/// The boot scan: a file that already exists when `serve` starts must be
/// indexed immediately, not one poll interval later. Uses a deliberately long
/// poll interval so that a fast pickup can *only* be the startup scan — this is
/// the behaviour that lets a fresh deploy drop the chart `scan` initContainer.
#[actix_web::test]
async fn boot_scan_indexes_preexisting_file_before_first_poll() {
    let cfg = WatchConfig {
        watch_enabled: false,
        // Long enough that the periodic tier cannot be what indexes the file
        // within the assertion timeout — only the boot scan can.
        poll_interval: Duration::from_secs(3600),
        rate_limit_ms: 0,
    };

    let td = tempfile::TempDir::new().unwrap();
    let root = td.path().to_path_buf();

    // The file exists on disk *before* the poller is spawned.
    tokio::fs::write(root.join("movie.mkv"), b"preexisting-bytes")
        .await
        .unwrap();
    let expected_id = pharos_scanner::stable_id(&root.join("movie.mkv"));

    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let state = AppState::new(stores, "boot-scan".into()).with_media_roots(vec![root.clone()]);
    let state = web::Data::new(state);
    let mut bus_rx = state.bus.subscribe();

    let _guards = spawn_for_roots(state.clone(), std::slice::from_ref(&root), cfg, || {
        FsScanner::new(FakeProber)
    });

    // Well under the 3600s poll interval: only the boot scan can index it here.
    let got_delta = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match bus_rx.recv().await {
                Ok(SocketBroadcast::LibraryChanged { added, .. }) => {
                    if added.contains(&expected_id.to_string()) {
                        return true;
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => return false,
            }
        }
    })
    .await
    .unwrap_or(false);

    assert!(
        got_delta,
        "the boot scan should index a pre-existing file and broadcast its id \
         without waiting for the first poll tick"
    );

    use pharos_core::MediaStore;
    assert!(
        state.stores.get(expected_id).await.is_ok(),
        "the pre-existing file should be persisted by the boot scan"
    );
}
