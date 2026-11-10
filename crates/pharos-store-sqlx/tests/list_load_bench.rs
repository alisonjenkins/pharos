//! Full-library `list()` load: wall time and heap bytes per call.
//!
//! Ignored by default. Seeds ~15k episodes (2 audio tracks, 2 subtitle tracks
//! each) and reports, per `MediaStore::list`, the median wall time and the
//! heap high-water mark above the pre-call baseline, plus total bytes
//! allocated. Run with:
//!   PHAROS_TEST_POSTGRES_URL=... cargo test -p pharos-store-sqlx \
//!     --features postgres --release --test list_load_bench -- --ignored --nocapture

#![cfg(feature = "postgres")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pharos_core::{
    AudioTrack, MediaItem, MediaKind, MediaMetadata, MediaProbe, MediaStore, SeriesInfo,
    SubtitleTrack,
};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

struct Counting;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static TOTAL: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards to `System`, only adding atomic counters.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            TOTAL.fetch_add(l.size(), Relaxed);
            let live = LIVE.fetch_add(l.size(), Relaxed) + l.size();
            PEAK.fetch_max(live, Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size(), Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            if new > l.size() {
                TOTAL.fetch_add(new - l.size(), Relaxed);
                let live = LIVE.fetch_add(new - l.size(), Relaxed) + (new - l.size());
                PEAK.fetch_max(live, Relaxed);
            } else {
                LIVE.fetch_sub(l.size() - new, Relaxed);
            }
        }
        q
    }
}

#[global_allocator]
static A: Counting = Counting;

fn episode(id: u64) -> MediaItem {
    let audio = |i: u32, lang: &str, codec: &str, ch: u32| AudioTrack {
        stream_index: i,
        codec: Some(codec.into()),
        channels: Some(ch),
        sample_rate: Some(48_000),
        language: Some(lang.into()),
        title: Some(format!("{lang} {codec} {ch}ch")),
        is_default: i == 1,
        replaygain_track_centidb: None,
        replaygain_album_centidb: None,
    };
    let sub = |i: u32, lang: &str| SubtitleTrack {
        stream_index: i,
        language: Some(lang.into()),
        codec: Some("subrip".into()),
        title: Some(format!("{lang} subs")),
        ..Default::default()
    };
    MediaItem {
        locked: Default::default(),
        id,
        path: format!(
            "/media/tv/Show {}/Season 01/Show {} - S01E{:02}.mkv",
            id / 24,
            id / 24,
            id % 24
        )
        .into(),
        title: format!("Episode {id} title of reasonable length"),
        kind: MediaKind::Episode,
        book: None,
        probe: MediaProbe {
            size_bytes: Some(1_500_000_000),
            duration_ms: Some(1_440_000),
            container: Some("matroska,webm".into()),
            video_codec: Some("hevc".into()),
            audio_codec: Some("aac".into()),
            width: Some(1920),
            height: Some(1080),
            audio_tracks: vec![audio(1, "jpn", "aac", 2), audio(2, "eng", "ac3", 6)],
            subtitle_tracks: vec![sub(3, "eng"), sub(4, "jpn")],
            synopsis: Some("A synopsis of a few sentences. ".repeat(6)),
            ..Default::default()
        },
        series: Some(SeriesInfo {
            series_name: format!("Show {}", id / 24),
            season_number: Some(1),
            episode_number: Some((id % 24) as u32),
            series_folder: Some(format!("/media/tv/Show {}", id / 24)),
            series_year: Some(2020),
            series_key: None,
        }),
        created_at: Some(1_700_000_000 + id as i64),
        metadata: MediaMetadata::default(),
        has_primary_art: true,
        art_version: 1,
        match_provider: None,
        match_external_id: None,
        match_source: None,
        match_confidence: None,
        metadata_refreshed_at: None,
        edition: None,
    }
}

#[tokio::test]
#[ignore = "benchmark; needs PHAROS_TEST_POSTGRES_URL"]
async fn list_load() {
    let url = std::env::var("PHAROS_TEST_POSTGRES_URL").expect("PHAROS_TEST_POSTGRES_URL");
    let store = pharos_store_sqlx::postgres::PostgresStore::connect(&url)
        .await
        .unwrap();
    const N: u64 = 15_000;
    let items: Vec<MediaItem> = (1..=N).map(episode).collect();
    for chunk in items.chunks(500) {
        store.put_batch(chunk).await.unwrap();
    }
    drop(items);

    let mut times = Vec::new();
    let (mut peak, mut total) = (0, 0);
    for round in 0..9 {
        let base = LIVE.load(Relaxed);
        PEAK.store(base, Relaxed);
        let t0 = Instant::now();
        TOTAL.store(0, Relaxed);
        let list = store.list().await.unwrap();
        let dt = t0.elapsed();
        assert_eq!(list.len(), N as usize);
        let held = LIVE.load(Relaxed) - base;
        if round > 0 {
            times.push(dt);
        }
        peak = PEAK.load(Relaxed) - base;
        total = TOTAL.load(Relaxed);
        eprintln!("round {round}: {dt:?} peak+{peak} total {total} retained {held}");
        drop(list);
    }
    times.sort();
    eprintln!(
        "RESULT median {:?} min {:?} max {:?} peak_bytes {peak} total_alloc_bytes {total}",
        times[times.len() / 2],
        times[0],
        times[times.len() - 1]
    );
}
