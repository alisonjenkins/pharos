#![allow(clippy::unwrap_used, clippy::expect_used)]
//! An episode's Primary image is its own still, not the series poster.
//!
//! The sidecar provider records the show folder's `folder.jpg` as every
//! episode's Primary artwork (synth series/season tiles resolve through that
//! row), while the episode's own `<stem>-thumb.jpg` is recorded as Thumb. The
//! episode list asks for Primary, so every episode showed the series poster.

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaStore, SecretString, SeriesInfo, TokenStore, UserId, UserPolicy,
    UserRecord, UserStore,
};
use pharos_jellyfin_api::dto::series_id_for_key;
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};
use std::path::Path;

fn episode(id: u64, series_dir: &Path, ep: u32) -> MediaItem {
    MediaItem {
        id,
        path: series_dir
            .join("Season 1")
            .join(format!("Dwarf - S01E{ep:02}.mkv")),
        title: format!("e{ep:02}"),
        kind: MediaKind::Episode,
        series: Some(SeriesInfo {
            series_name: "Dwarf".into(),
            season_number: Some(1),
            episode_number: Some(ep),
            series_folder: Some(series_dir.to_string_lossy().into_owned()),
            series_year: None,
        }),
        ..Default::default()
    }
}

async fn record(stores: &Stores, id: u64, role: &str, source: &str, path: &Path) {
    stores
        .set_artwork(id, role, source, &path.to_string_lossy())
        .await
        .unwrap();
}

/// Episode 1 has its own `-thumb` sidecar, episode 2 only show-level art
/// (`landscape.jpg` in the season folder), episode 3 a provider still.
async fn seed(series_dir: &Path, cache_dir: &Path) -> (web::Data<AppState>, String) {
    let season = series_dir.join("Season 1");
    std::fs::create_dir_all(&season).unwrap();
    let folder = series_dir.join("folder.jpg");
    std::fs::write(&folder, b"SERIES-POSTER").unwrap();
    let own_thumb = season.join("Dwarf - S01E01-thumb.jpg");
    std::fs::write(&own_thumb, b"E01-STILL").unwrap();
    let landscape = season.join("landscape.jpg");
    std::fs::write(&landscape, b"SHOW-LANDSCAPE").unwrap();
    let provider = cache_dir.join("e03-tvdb.jpg");
    std::fs::write(&provider, b"E03-TVDB-STILL").unwrap();

    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("p")).unwrap();
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "u".into(),
            password_hash: hash,
            policy: UserPolicy::default(),
        })
        .await
        .unwrap();
    let token = stores.issue(uid, "t").await.unwrap();
    for ep in 1..=3 {
        let id = u64::from(ep);
        stores.put(episode(id, series_dir, ep)).await.unwrap();
        record(&stores, id, "Primary", "local", &folder).await;
    }
    record(&stores, 1, "Thumb", "local", &own_thumb).await;
    record(&stores, 2, "Thumb", "local", &landscape).await;
    record(&stores, 3, "Primary", "tvdb", &provider).await;
    let state = AppState::new(stores, "t".into())
        .with_image_cache(pharos_cache::ImageCache::new(cache_dir));
    (web::Data::new(state), token.0.expose().to_string())
}

macro_rules! fetch_primary {
    ($app:expr, $token:expr, $id:expr) => {{
        let req = test::TestRequest::get()
            .uri(&format!("/Items/{}/Images/Primary", $id))
            .insert_header(("X-Emby-Token", $token))
            .to_request();
        test::call_and_read_body($app, req).await.to_vec()
    }};
}

#[actix_web::test]
async fn episode_primary_is_its_own_still_and_the_series_keeps_its_poster() {
    let media = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let (state, token) = seed(media.path(), cache.path()).await;
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    let token = token.as_str();

    assert_eq!(
        fetch_primary!(&app, token, 1),
        b"E01-STILL".to_vec(),
        "an episode's own -thumb sidecar beats the series poster"
    );
    assert_eq!(
        fetch_primary!(&app, token, 2),
        b"SERIES-POSTER".to_vec(),
        "show-level art is not an episode still; the series poster stands"
    );
    assert_eq!(
        fetch_primary!(&app, token, 3),
        b"E03-TVDB-STILL".to_vec(),
        "a provider still beats the series poster"
    );
    let series = series_id_for_key(Some(&media.path().to_string_lossy()), "Dwarf");
    assert_eq!(
        fetch_primary!(&app, token, &series),
        b"SERIES-POSTER".to_vec(),
        "the series tile still resolves to the series poster"
    );
}

/// B835 — an episode's Primary reads its artwork rows once, whether the
/// answer is its own still or falls through to the recorded poster.
#[actix_web::test]
async fn an_episode_primary_reads_its_artwork_rows_once() {
    use crate::store_calls::StoreCallLog;
    use tracing_subscriber::prelude::*;

    let media = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let (state, token) = seed(media.path(), cache.path()).await;
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    let calls = StoreCallLog::default();
    let _spans =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(calls.clone()));
    for (id, body) in [
        (1, &b"E01-STILL"[..]),
        (2, &b"SERIES-POSTER"[..]),
        (3, &b"E03-TVDB-STILL"[..]),
    ] {
        calls.take();
        assert_eq!(fetch_primary!(&app, token.as_str(), id), body);
        let artwork = calls
            .take()
            .into_iter()
            .filter(|c| c == "artwork_for")
            .count();
        assert_eq!(artwork, 1, "episode {id}");
    }
}
