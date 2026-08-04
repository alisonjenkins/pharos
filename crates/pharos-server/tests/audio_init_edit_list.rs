//! The demuxed audio rendition's shared init must not carry a presentation
//! offset of its own.
//!
//! `resolve_audio_file` serves `init.mp4` from whichever session has one,
//! documented as safe because "the init is codec configuration and is identical
//! across sessions". It is not: ffmpeg gives a session built with
//! `-output_ts_offset` (every SEEK session) an EMPTY EDIT in its edit list
//! equal to that offset. Every fragment is separately re-anchored onto the
//! absolute timeline (B121), so a client that honours the edit applies the
//! offset twice and puts the audio at double its true position — where it can
//! never overlap the video, so playback never starts (B186).
//!
//! These drive real ffmpeg sessions through the real cache: the hazard is a
//! property of what the muxer writes, and a hand-built `moov` would only assert
//! this file's own idea of it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

use actix_web::{test, web, App};
use pharos_cache::HlsSegmentCache;
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore, UserId, UserPolicy,
    UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin::{fmp4, hls},
    auth::BuiltinAuth,
    state::{AppState, Stores},
};

/// Segment the seek session starts at — deep enough that its offset is many
/// times a segment, so a doubled offset cannot be mistaken for rounding.
const SEEK_SEG: u32 = 5;

fn ffmpeg(args: &[&str], what: &str) {
    let out = Command::new("ffmpeg")
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawn ffmpeg for {what}: {e}"));
    assert!(
        out.status.success(),
        "{what} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A plain 60 s tone — long enough for a session seeked to `SEEK_SEG` to have
/// real content after it.
fn make_source(dir: &Path) -> PathBuf {
    let src = dir.join("src.mkv");
    ffmpeg(
        &[
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=60",
            "-c:a",
            "aac",
            src.to_str().expect("utf-8 path"),
        ],
        "source",
    );
    src
}

/// The init bytes of a session covering `want_seg`, and the session's start.
async fn init_of(cache: &HlsSegmentCache, src: &Path, media_id: u64, want_seg: u32) -> Vec<u8> {
    let dir = cache
        .ensure_audio_hls_covering(src, media_id, None, Some(128_000), want_seg)
        .await
        .expect("audio session");
    // Force the session to actually produce, so the init exists.
    cache
        .audio_hls_file(&dir, &format!("a{want_seg}.m4s"))
        .await
        .expect("fragment produced");
    cache
        .audio_hls_file(&dir, "init.mp4")
        .await
        .expect("init produced")
        .bytes
}

/// The whole-file session's init defers nothing — the shape every fragment is
/// served against.
#[tokio::test]
async fn a_whole_file_sessions_init_defers_nothing() {
    let td = tempfile::tempdir().expect("tempdir");
    let src = make_source(td.path());
    let cache = HlsSegmentCache::new(td.path().join("cache"), 1 << 30);

    let init = init_of(&cache, &src, 1, 0).await;

    assert_eq!(
        fmp4::init_empty_edit_secs(&init),
        None,
        "the from-0 session's init should carry no empty edit"
    );
}

/// A SEEK session's init defers its track by the session's own start — the
/// difference the serve path was built to believe does not exist.
///
/// This is the whole bug in one assertion: the same URL, served `immutable` for
/// a year, hands out one of two different bodies depending on which session
/// happened to have written an init first.
#[tokio::test]
async fn a_seek_sessions_init_defers_its_track_by_the_session_start() {
    let td = tempfile::tempdir().expect("tempdir");
    let src = make_source(td.path());
    let cache = HlsSegmentCache::new(td.path().join("cache"), 1 << 30);

    // Separate media ids: one rendition root per session, so each init is the
    // one its own session wrote rather than whichever came first.
    let from0 = init_of(&cache, &src, 2, 0).await;
    let seek = init_of(&cache, &src, 3, SEEK_SEG).await;

    let want = f64::from(SEEK_SEG) * HlsSegmentCache::AUDIO_SEGMENT_SECONDS;
    let got = fmp4::init_empty_edit_secs(&seek).expect(
        "a seek session's init carries an empty edit — if ffmpeg stopped writing one, \
                 the serve-side neutralisation is dead code and should be deleted, not relaxed",
    );
    assert!(
        (got - want).abs() < 0.05,
        "the session seeked to segment {SEEK_SEG} should defer its track by {want:.3}s \
         (its own start), but the init defers {got:.3}s"
    );
    assert_ne!(
        from0, seek,
        "the two sessions' inits differ — 'the init is identical across sessions' is false, \
         and both are served under one immutable URL"
    );
}

/// B186 / V141 — dropping the empty edit makes a seek session's init the
/// whole-file session's init, byte for byte.
///
/// Byte equality is the assertion worth making rather than "no empty edit
/// remains": the init is served `immutable, max-age=1y` under a URL that names
/// the item and nothing about which session produced the bytes, so two sessions
/// that disagree by even one byte put a stale, incompatible init in a browser
/// for a year. Neutralising has to converge on ONE body, not merely a
/// well-behaved one.
#[tokio::test]
async fn dropping_the_empty_edit_converges_on_the_whole_file_sessions_init() {
    let td = tempfile::tempdir().expect("tempdir");
    let src = make_source(td.path());
    let cache = HlsSegmentCache::new(td.path().join("cache"), 1 << 30);

    let from0 = init_of(&cache, &src, 4, 0).await;
    let mut seek = init_of(&cache, &src, 5, SEEK_SEG).await;

    let want = f64::from(SEEK_SEG) * HlsSegmentCache::AUDIO_SEGMENT_SECONDS;
    let dropped = fmp4::drop_empty_edits(&mut seek)
        .expect("edit list rewritten")
        .expect("a seek session's init has an empty edit to drop");
    assert!(
        (dropped - want).abs() < 0.05,
        "should have dropped the session's own {want:.3}s deferral, dropped {dropped:.3}s"
    );
    assert_eq!(
        fmp4::init_empty_edit_secs(&seek),
        None,
        "the neutralised init must defer nothing"
    );
    assert_eq!(
        seek, from0,
        "a neutralised seek-session init must be the from-0 session's init byte for byte — \
         one immutable URL cannot have two bodies"
    );
}

/// Neutralising an already-neutral init changes nothing and reports nothing.
/// The serve path runs this on every init, including the overwhelmingly common
/// from-0 one.
#[tokio::test]
async fn dropping_from_a_neutral_init_is_a_no_op() {
    let td = tempfile::tempdir().expect("tempdir");
    let src = make_source(td.path());
    let cache = HlsSegmentCache::new(td.path().join("cache"), 1 << 30);

    let mut init = init_of(&cache, &src, 6, 0).await;
    let before = init.clone();

    assert_eq!(
        fmp4::drop_empty_edits(&mut init).expect("edit list read"),
        None,
        "a from-0 init has no empty edit to drop"
    );
    assert_eq!(init, before, "a no-op must not touch a byte");
}

/// Seed a store + app state around one audio-only fixture.
async fn seed(fixture: PathBuf, cache_dir: &Path) -> (web::Data<AppState>, String) {
    let stores = Stores::connect("sqlite::memory:").await.expect("stores");
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth
        .hash_password(&SecretString::new("p"))
        .expect("hash password");
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "u".into(),
            password_hash: hash,
            policy: UserPolicy::default(),
        })
        .await
        .expect("create user");
    let token = stores.issue(uid, "t").await.expect("issue token");
    stores
        .put(MediaItem {
            id: 42,
            path: fixture,
            title: "tone".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(60_000),
                audio_codec: Some("aac".into()),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .expect("put item");
    let cache = HlsSegmentCache::new(cache_dir, 128 * 1024 * 1024);
    let state = web::Data::new(AppState::new(stores, "t".into()).with_hls_cache(cache));
    (state, token.0.expose().to_string())
}

/// B186 — the ROUTE, not just the helper, must serve a neutral init.
///
/// The two are worth separating: `drop_empty_edits` passing proves the rewrite
/// works, and nothing else. Deleting its one call site in `vp9_audio_file`
/// leaves every other test in this file green while putting the bug straight
/// back on the wire.
#[actix_web::test]
async fn the_audio_init_route_serves_a_neutral_init_after_a_deep_seek() {
    let td = tempfile::tempdir().expect("tempdir");
    let src = make_source(td.path());
    let (state, token) = seed(src, &td.path().join("cache")).await;
    let app = test::init_service(App::new().app_data(state).configure(hls::register)).await;

    // Fetch a deep fragment first: that is what spawns the seek session whose
    // init carries the empty edit, and the session the init then resolves from.
    let frag = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!(
                "/videos/42/vp9/audio/a{SEEK_SEG}.m4s?api_key={token}"
            ))
            .to_request(),
    )
    .await;
    assert!(
        frag.status().is_success(),
        "seek fragment should be served: {}",
        frag.status()
    );

    let init = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri(&format!("/videos/42/vp9/audio/init.mp4?api_key={token}"))
            .to_request(),
    )
    .await;

    assert_eq!(
        fmp4::init_empty_edit_secs(&init),
        None,
        "the init served after a deep seek still defers its track — every fragment \
         under it is re-anchored to the same offset, so the audio lands at twice \
         its true position and never overlaps the video"
    );
}
