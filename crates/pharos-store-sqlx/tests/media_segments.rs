//! T86 — MediaSegmentStore round-trip on the real sqlite backend.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pharos_core::{
    DetectedSegment, FingerprintKind, MediaSegmentKind, MediaSegmentStore, SEGMENT_SCHEMA_VERSION,
};
use pharos_store_sqlx::sqlite::SqliteStore;

#[tokio::test]
async fn segments_round_trip_and_replace() {
    let s = SqliteStore::connect("sqlite::memory:").await.unwrap();
    let item = 4242u64;
    // Empty initially.
    assert!(s.media_segments_for(item).await.unwrap().is_empty());

    let segs = vec![
        DetectedSegment {
            kind: MediaSegmentKind::Intro,
            start_ms: 0,
            end_ms: 30_000,
            detector: "chromaprint".into(),
            confidence: 0.9,
        },
        DetectedSegment {
            kind: MediaSegmentKind::Outro,
            start_ms: 1_200_000,
            end_ms: 1_260_000,
            detector: "chromaprint".into(),
            confidence: 0.8,
        },
    ];
    s.set_media_segments(item, &segs, SEGMENT_SCHEMA_VERSION)
        .await
        .unwrap();
    let got = s.media_segments_for(item).await.unwrap();
    assert_eq!(got.len(), 2);
    // Ordered by start_ms → Intro first.
    assert_eq!(got[0].kind, MediaSegmentKind::Intro);
    assert_eq!(got[0].end_ms, 30_000);
    assert!((got[1].confidence - 0.8).abs() < 1e-4);

    // set_media_segments REPLACES: writing one clears the other.
    s.set_media_segments(
        item,
        &[DetectedSegment {
            kind: MediaSegmentKind::Recap,
            start_ms: 0,
            end_ms: 20_000,
            detector: "chromaprint".into(),
            confidence: 1.0,
        }],
        SEGMENT_SCHEMA_VERSION,
    )
    .await
    .unwrap();
    let got = s.media_segments_for(item).await.unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].kind, MediaSegmentKind::Recap);
}

#[tokio::test]
async fn fingerprints_round_trip_and_version_gated() {
    let s = SqliteStore::connect("sqlite::memory:").await.unwrap();
    let item = 77u64;
    let points: Vec<u32> = vec![0xDEAD_BEEF, 0x0102_0304, 0, u32::MAX];
    s.set_episode_fingerprint(
        item,
        FingerprintKind::Intro,
        &points,
        SEGMENT_SCHEMA_VERSION,
    )
    .await
    .unwrap();
    // Exact round-trip.
    let got = s
        .episode_fingerprint_for(item, FingerprintKind::Intro, SEGMENT_SCHEMA_VERSION)
        .await
        .unwrap();
    assert_eq!(got, Some(points.clone()));
    // Wrong window → None.
    assert!(s
        .episode_fingerprint_for(item, FingerprintKind::Credits, SEGMENT_SCHEMA_VERSION)
        .await
        .unwrap()
        .is_none());
    // Wrong schema version → None (forces re-analysis on algo change).
    assert!(s
        .episode_fingerprint_for(item, FingerprintKind::Intro, SEGMENT_SCHEMA_VERSION + 1)
        .await
        .unwrap()
        .is_none());
}

/// B123 — an episode with no detectable intro must still be recorded as
/// ANALYSED. Results alone cannot express that: an empty detection writes no
/// segment row, which is indistinguishable from never having looked, so the
/// sweep re-analysed such a season on every pass forever.
#[tokio::test]
async fn an_empty_analysis_is_still_recorded_as_done() {
    let s = SqliteStore::connect("sqlite::memory:").await.unwrap();
    let item = 77u64;
    assert_eq!(s.segment_scan_version(item).await.unwrap(), None);

    // The detector found nothing for this episode.
    s.set_media_segments(item, &[], SEGMENT_SCHEMA_VERSION)
        .await
        .unwrap();
    s.set_segment_scan(item, SEGMENT_SCHEMA_VERSION)
        .await
        .unwrap();

    assert!(
        s.media_segments_for(item).await.unwrap().is_empty(),
        "nothing was detected, so nothing is served"
    );
    assert_eq!(
        s.segment_scan_version(item).await.unwrap(),
        Some(SEGMENT_SCHEMA_VERSION),
        "but the analysis itself must be recorded, or it runs again forever"
    );

    // A later algorithm version supersedes the stamp, which is what makes a
    // SEGMENT_SCHEMA_VERSION bump able to force re-detection.
    s.set_segment_scan(item, SEGMENT_SCHEMA_VERSION + 1)
        .await
        .unwrap();
    assert_eq!(
        s.segment_scan_version(item).await.unwrap(),
        Some(SEGMENT_SCHEMA_VERSION + 1)
    );
}

/// T103 — the snapshot exists so a detector change can be judged, so the one
/// thing it must never do is overwrite itself on a later pass. The sweep calls
/// it once per pass; only the FIRST call, before anything is replaced, holds
/// the state worth keeping.
#[tokio::test]
async fn a_snapshot_is_taken_once_and_never_overwritten() {
    let s = SqliteStore::connect("sqlite::memory:").await.unwrap();
    let item = 91u64;
    let before = [DetectedSegment {
        kind: MediaSegmentKind::Intro,
        start_ms: 0,
        end_ms: 111_000,
        detector: "chromaprint".into(),
        confidence: 0.88,
    }];
    s.set_media_segments(item, &before, SEGMENT_SCHEMA_VERSION)
        .await
        .unwrap();

    let rows = s.snapshot_media_segments("pre_detect_v4").await.unwrap();
    assert_eq!(rows, 1, "the pre-change state must be captured");

    // The detector now runs and REPLACES the segment — the exact event that
    // destroyed the only baseline when SEGMENT_DETECT_VERSION 2->3 ran.
    s.set_media_segments(item, &[], SEGMENT_SCHEMA_VERSION)
        .await
        .unwrap();
    assert!(s.media_segments_for(item).await.unwrap().is_empty());

    let again = s.snapshot_media_segments("pre_detect_v4").await.unwrap();
    assert_eq!(
        again, 0,
        "a second call must NOT re-snapshot; that would replace the baseline \
         with the post-change state and make the comparison impossible"
    );

    // A different detect version gets its own baseline.
    let next = s.snapshot_media_segments("pre_detect_v5").await.unwrap();
    assert_eq!(next, 0, "nothing to copy now, but the label is independent");
}
