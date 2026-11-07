//! Spec 030 — the Jellyfin RemoteSearch/Subtitles endpoints over a fake
//! OpenSubtitles provider (no network): V563 off without a key, V564 downloads
//! only on the cache volume, V565 the track appears in PlaybackInfo, V566
//! ordering and query, V567 quota answers 429.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use actix_web::test;
use pharos_core::{MediaItem, MediaKind, MediaStore as _};
use pharos_server::opensubtitles::{
    BoxFut, Downloaded, OpenSubtitles, ProviderError, QuotaGate, SearchQuery, SubtitleHit,
    SubtitleProvider,
};
use serde_json::Value;

use crate::common::{build_app, seed_rich, seed_rich_with, Fixture};

const SRT: &[u8] = b"1\n00:00:01,000 --> 00:00:02,000\nhello\n";

#[derive(Default)]
struct Fake {
    hits: Vec<SubtitleHit>,
    queries: Mutex<Vec<SearchQuery>>,
    downloads: AtomicU32,
    quota_exhausted: bool,
}

impl SubtitleProvider for Fake {
    fn search<'a>(
        &'a self,
        query: &'a SearchQuery,
    ) -> BoxFut<'a, Result<Vec<SubtitleHit>, ProviderError>> {
        self.queries.lock().unwrap().push(query.clone());
        let hits = self.hits.clone();
        Box::pin(async move { Ok(hits) })
    }

    fn download(&self, _file_id: u64) -> BoxFut<'_, Result<Downloaded, ProviderError>> {
        self.downloads.fetch_add(1, Ordering::SeqCst);
        let quota = self.quota_exhausted;
        Box::pin(async move {
            if quota {
                return Err(ProviderError::QuotaExceeded {
                    reset_unix: 4_000_000_000,
                });
            }
            Ok(Downloaded {
                file_name: "x.srt".into(),
                bytes: SRT.to_vec(),
                remaining: Some(3),
            })
        })
    }
}

fn hit(file_id: u64, release: &str, hash_match: bool, downloads: u32) -> SubtitleHit {
    SubtitleHit {
        file_id,
        subtitle_id: format!("s{file_id}"),
        language: "en".into(),
        release: release.into(),
        download_count: downloads,
        ratings: 7.5,
        from_trusted: false,
        hearing_impaired: false,
        ai_translated: false,
        machine_translated: false,
        foreign_parts_only: false,
        fps: Some(23.976),
        uploader: Some("alice".into()),
        upload_date: Some("2020-05-01T10:00:00Z".into()),
        hash_match,
    }
}

/// One store root per test process: `install_root` is first-wins.
fn store_root() -> std::path::PathBuf {
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    DIR.get_or_init(|| tempfile::tempdir().unwrap())
        .path()
        .join("opensubtitles")
}

async fn fixture_with(fake: Arc<Fake>) -> Fixture {
    let provider: Arc<dyn SubtitleProvider> = fake;
    seed_rich_with(move |state| {
        state.with_opensubtitles(OpenSubtitles {
            provider,
            root: store_root(),
            gate: Arc::new(QuotaGate::new()),
        })
    })
    .await
}

/// A 200 KiB movie file on disk (big enough to hash) in its own directory.
async fn put_movie(f: &Fixture, id: u64) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("film.mkv");
    std::fs::write(&video, vec![7u8; 200 * 1024]).unwrap();
    let mut item = MediaItem {
        id,
        path: video.clone(),
        title: "Film".into(),
        kind: MediaKind::Movie,
        ..MediaItem::default()
    };
    item.metadata.provider_ids.imdb = Some("tt0111161".into());
    f.state.stores.put(item).await.unwrap();
    (dir, video)
}

async fn get_json(f: &Fixture, uri: &str) -> (u16, Value) {
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::get()
        .uri(uri)
        .insert_header(("X-Emby-Token", f.admin_token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    let status = resp.status().as_u16();
    let body = test::read_body(resp).await;
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

async fn post_status(f: &Fixture, uri: &str, token: &str) -> u16 {
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::post()
        .uri(uri)
        .insert_header(("X-Emby-Token", token))
        .to_request();
    test::call_service(&app, req).await.status().as_u16()
}

#[actix_web::test]
async fn off_without_a_key_search_is_empty_and_download_is_refused() {
    let f = seed_rich().await;
    let (status, body) = get_json(
        &f,
        &format!("/Items/{}/RemoteSearch/Subtitles/eng", f.rich_item_id),
    )
    .await;
    assert_eq!((status, body), (200, serde_json::json!([])));
    let status = post_status(
        &f,
        &format!("/Items/{}/RemoteSearch/Subtitles/1-eng", f.rich_item_id),
        &f.admin_token,
    )
    .await;
    assert_eq!(status, 400);
}

#[actix_web::test]
async fn search_ranks_hash_matches_first_and_sends_hash_and_imdb() {
    let fake = Arc::new(Fake {
        hits: vec![
            hit(1, "popular", false, 9000),
            hit(2, "exact", true, 3),
            hit(3, "meh", false, 10),
        ],
        ..Fake::default()
    });
    let f = fixture_with(fake.clone()).await;
    let (_dir, _video) = put_movie(&f, 601).await;
    let (status, body) = get_json(&f, "/Items/601/RemoteSearch/Subtitles/eng").await;
    assert_eq!(status, 200, "{body}");
    let names: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["Name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["exact", "popular", "meh"]);
    let first = &body[0];
    assert_eq!(first["Id"], "2-en");
    assert_eq!(first["ProviderName"], "OpenSubtitles");
    assert_eq!(first["ThreeLetterISOLanguageName"], "eng");
    assert_eq!(first["IsHashMatch"], true);
    assert_eq!(first["Format"], "srt");
    assert_eq!(first["DownloadCount"], 3);
    assert_eq!(first["Author"], "alice");
    let q = fake.queries.lock().unwrap();
    assert_eq!(q.len(), 1);
    assert_eq!(q[0].language, "en");
    assert_eq!(q[0].imdb_id, Some(111161));
    let hash = q[0].moviehash.as_deref().unwrap();
    assert_eq!(hash.len(), 16, "a 200 KiB file is hashed");
}

#[actix_web::test]
async fn download_stores_on_the_cache_volume_and_playback_info_lists_it() {
    let fake = Arc::new(Fake::default());
    let f = fixture_with(fake.clone()).await;
    let (dir, video) = put_movie(&f, 602).await;
    let before: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();

    let status = post_status(
        &f,
        "/Items/602/RemoteSearch/Subtitles/42-en",
        &f.admin_token,
    )
    .await;
    assert_eq!(status, 204);
    assert_eq!(fake.downloads.load(Ordering::SeqCst), 1);

    let after: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(before, after, "nothing is written beside the media");
    let stored =
        pharos_server::opensubtitles::store::item_dir(&store_root(), &video).join("os42.en.srt");
    assert_eq!(std::fs::read(&stored).unwrap(), SRT);

    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::post()
        .uri("/Items/602/PlaybackInfo")
        .insert_header(("X-Emby-Token", f.admin_token.as_str()))
        .set_json(serde_json::json!({}))
        .to_request();
    let body: Value = test::call_and_read_body_json(&app, req).await;
    let subs: Vec<&Value> = body["MediaSources"][0]["MediaStreams"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["Type"] == "Subtitle")
        .collect();
    assert_eq!(subs.len(), 1, "{body}");
    assert_eq!(subs[0]["IsExternal"], true);
    assert!(
        subs[0]["Language"]
            .as_str()
            .unwrap()
            .to_ascii_lowercase()
            .starts_with("en"),
        "{}",
        subs[0]
    );
}

#[actix_web::test]
async fn quota_exhaustion_answers_429_and_stores_nothing() {
    let fake = Arc::new(Fake {
        quota_exhausted: true,
        ..Fake::default()
    });
    let f = fixture_with(fake).await;
    let (_dir, video) = put_movie(&f, 603).await;
    let status = post_status(
        &f,
        "/Items/603/RemoteSearch/Subtitles/42-en",
        &f.admin_token,
    )
    .await;
    assert_eq!(status, 429);
    assert!(!pharos_server::opensubtitles::store::item_dir(&store_root(), &video).exists());
}

#[actix_web::test]
async fn non_admin_cannot_download() {
    let fake = Arc::new(Fake::default());
    let f = fixture_with(fake.clone()).await;
    let (_dir, _video) = put_movie(&f, 604).await;
    let status = post_status(&f, "/Items/604/RemoteSearch/Subtitles/42-en", &f.user_token).await;
    assert_eq!(status, 403);
    assert_eq!(fake.downloads.load(Ordering::SeqCst), 0);
}
