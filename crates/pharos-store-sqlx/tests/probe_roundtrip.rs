//! MediaItem round-trip property test. `put → get` must preserve
//! every persisted field exactly (modulo server-stamped
//! `created_at`).
//!
//! Why proptest the round-trip: hand-crafted unit tests cover the
//! happy path. A regression in `subtitle_tracks_json` JSON encoding
//! (eg. dropping `is_default`), an off-by-one on `frame_rate_mille`
//! integer scaling, or a None-vs-Some(0) confusion on an optional
//! numeric column survives single-fixture tests but fails the moment
//! a real probe lands a value the fixture didn't exercise. Generating
//! arbitrary MediaProbe shapes catches the bugs the fixture missed.

#![cfg(feature = "sqlite")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pharos_core::{MediaItem, MediaKind, MediaProbe, MediaStore, SeriesInfo, SubtitleTrack};
use pharos_store_sqlx::sqlite::SqliteStore;
use proptest::prelude::*;
use proptest::test_runner::TestRunner;

fn arb_kind() -> impl Strategy<Value = MediaKind> {
    prop_oneof![
        Just(MediaKind::Movie),
        Just(MediaKind::Episode),
        Just(MediaKind::Audio),
    ]
}

fn arb_subtitle_track() -> impl Strategy<Value = SubtitleTrack> {
    (
        any::<u32>(),
        proptest::option::of("[a-z]{2,3}"),
        proptest::option::of("[a-z]{3,8}"),
        proptest::option::of("[A-Za-z0-9 ]{0,32}"),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(|(idx, lang, codec, title, dflt, forced)| SubtitleTrack {
            stream_index: idx,
            language: lang,
            codec,
            title,
            is_default: dflt,
            is_forced: forced,
            is_hearing_impaired: false,
        })
}

fn arb_probe() -> impl Strategy<Value = MediaProbe> {
    // u64 fields cap at i64::MAX because the SQLite store binds them
    // as i64. Documented constraint: bytes / ms / bps over 9.2 EB /
    // 290M years / 9.2 Ebps round-trip as None. Real probe values
    // are nowhere near.
    let big = 0u64..(i64::MAX as u64);
    (
        proptest::option::of(big.clone()),
        proptest::option::of(big.clone()),
        proptest::option::of("[a-z]{3,8}"),
        proptest::option::of(big),
        proptest::option::of("[a-z0-9]{3,8}"),
        proptest::option::of("[a-z0-9]{3,8}"),
        proptest::option::of(any::<u32>()),
        proptest::option::of(any::<u32>()),
        proptest::option::of(any::<u32>()),
        proptest::option::of(any::<u32>()),
        proptest::option::of(any::<u32>()),
        proptest::collection::vec(arb_subtitle_track(), 0..4),
    )
        .prop_flat_map(|t| {
            (
                Just(t),
                proptest::option::of("[A-Za-z0-9 ]{0,24}"),
                proptest::option::of("[A-Za-z0-9 ]{0,24}"),
                proptest::option::of("[A-Za-z0-9 ]{0,24}"),
                proptest::option::of("[A-Za-z0-9 ]{0,24}"),
            )
        })
        .prop_map(
            |(
                (size, dur, container, br, vc, ac, w, h, fr, ch, sr, subs),
                artist,
                album,
                album_artist,
                genre,
            )| MediaProbe {
                size_bytes: size,
                duration_ms: dur,
                container,
                bitrate_bps: br,
                video_codec: vc,
                video_profile: None,
                video_level: None,
                pixel_format: None,
                color_primaries: None,
                color_transfer: None,
                color_space: None,
                audio_codec: ac,
                width: w,
                height: h,
                frame_rate_mille: fr,
                audio_channels: ch,
                sample_rate: sr,
                subtitle_tracks: subs,
                audio_tracks: Vec::new(),
                artist,
                album,
                album_artist,
                genre,
                chapters: Vec::new(),
                alternate_sources: Vec::new(),
            },
        )
}

fn arb_series() -> impl Strategy<Value = Option<SeriesInfo>> {
    proptest::option::of(
        (
            "[A-Za-z0-9 ]{1,32}",
            proptest::option::of(any::<u32>()),
            proptest::option::of(any::<u32>()),
        )
            .prop_map(|(name, season, ep)| SeriesInfo {
                series_name: name,
                season_number: season,
                episode_number: ep,
                ..Default::default()
            }),
    )
}

fn arb_item() -> impl Strategy<Value = MediaItem> {
    (
        1u64..1_000_000_u64,
        "[A-Za-z0-9_/-]{1,64}",
        "[A-Za-z0-9 .'-]{1,48}",
        arb_kind(),
        arb_probe(),
        arb_series(),
    )
        .prop_map(|(id, path, title, kind, probe, series)| MediaItem {
            id,
            path: format!("/m/{path}.bin").into(),
            title,
            kind,
            probe,
            series,
            // `created_at = None` so the store backfills — compared
            // out below.
            created_at: None,
            // Descriptive metadata round-trip is covered by dedicated
            // fixtures in sqlite_store.rs; the probe proptest keeps it
            // at default.
            metadata: Default::default(),
        })
}

/// Strip server-stamped fields the store generates on insert so the
/// pre-put item compares equal to the post-get item.
fn strip_volatile(mut item: MediaItem) -> MediaItem {
    item.created_at = None;
    item
}

#[test]
fn arbitrary_media_item_roundtrips_through_sqlite() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    // P47 — `PROPTEST_CASES` env override. Local default 16 since
    // each case spins up a fresh in-memory sqlite + full migration.
    let cases: u32 = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(16);
    let mut runner = TestRunner::new(ProptestConfig {
        cases,
        max_shrink_iters: cases.saturating_mul(2).min(64),
        ..ProptestConfig::default()
    });

    runner
        .run(&arb_item(), |item| {
            let res: Result<(), TestCaseError> = runtime.block_on(async {
                let s = SqliteStore::connect("sqlite::memory:").await.unwrap();
                s.put(item.clone()).await.unwrap();
                let back = s.get(item.id).await.unwrap();
                let expected = strip_volatile(item.clone());
                let actual = strip_volatile(back);
                prop_assert_eq!(actual, expected);
                Ok(())
            });
            res
        })
        .unwrap();
}
