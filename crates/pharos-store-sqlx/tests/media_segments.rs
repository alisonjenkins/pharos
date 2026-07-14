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
