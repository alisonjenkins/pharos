#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Season posters must be SEASON-specific, not the series poster repeated.
//!
//! A synth Season wire id resolves to a representative episode, and that
//! episode's artwork rows record the SERIES-root `poster.jpg` (the sidecar
//! provider probes the show folder for episodes) — so every season of a show
//! served the identical series poster. The fix probes the Kodi/Jellyfin
//! series-root per-season convention (`season{NN}-poster.jpg`,
//! `season-specials-poster.jpg`) for the season the id names, falling back to
//! the old behaviour when no such file exists.

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaStore, SecretString, SeriesInfo, TokenStore, UserId, UserPolicy,
    UserRecord, UserStore,
};
use pharos_jellyfin_api::dto::season_id_for_key;
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

fn episode(id: u64, folder: &std::path::Path, season: u32, ep: u32) -> MediaItem {
    MediaItem {
        id,
        path: folder.join(format!("Buffy S{season:02}E{ep:02}.mkv")),
        title: format!("s{season:02}e{ep:02}"),
        kind: MediaKind::Episode,
        series: Some(SeriesInfo {
            series_name: "Buffy".into(),
            season_number: Some(season),
            episode_number: Some(ep),
            series_folder: Some(folder.to_string_lossy().into_owned()),
            series_year: None,
        }),
        ..Default::default()
    }
}

/// Seed a flat show folder (episodes beside the art — the layout that
/// triggered the bug) with a series poster and one per-season poster.
async fn seed(
    series_dir: &std::path::Path,
    cache_dir: &std::path::Path,
) -> (web::Data<AppState>, String) {
    std::fs::write(series_dir.join("poster.jpg"), b"SERIES-POSTER").unwrap();
    std::fs::write(series_dir.join("season02-poster.jpg"), b"SEASON-2-POSTER").unwrap();
    std::fs::write(
        series_dir.join("season-specials-poster.jpg"),
        b"SPECIALS-POSTER",
    )
    .unwrap();

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
    for (id, season, ep) in [(1, 1, 1), (2, 2, 1), (3, 0, 1)] {
        stores
            .put(episode(id, series_dir, season, ep))
            .await
            .unwrap();
        // The scanner records the series-root poster as each EPISODE's local
        // Primary artwork (that's exactly how the identical-posters bug
        // manifested on the real library).
        stores
            .set_artwork(
                id,
                "Primary",
                "local",
                &series_dir.join("poster.jpg").to_string_lossy(),
            )
            .await
            .unwrap();
    }
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
async fn season_posters_are_season_specific_with_series_fallback() {
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

    let folder = media.path().to_string_lossy();
    let s1 = season_id_for_key(Some(&folder), "Buffy", 1);
    let s2 = season_id_for_key(Some(&folder), "Buffy", 2);
    let s0 = season_id_for_key(Some(&folder), "Buffy", 0);

    // Season 2 has its own `season02-poster.jpg` → must serve THAT, not the
    // series poster (the bug: all seasons returned identical series bytes).
    assert_eq!(
        fetch_primary!(&app, token, &s2),
        b"SEASON-2-POSTER".to_vec(),
        "season with per-season sidecar serves it"
    );
    // Season 0 (Specials) uses the `season-specials-poster` alias.
    assert_eq!(
        fetch_primary!(&app, token, &s0),
        b"SPECIALS-POSTER".to_vec(),
        "specials serve the season-specials sidecar"
    );
    // Season 1 has no per-season art → the series poster fallback stands
    // (Jellyfin's own behaviour for an art-less season).
    assert_eq!(
        fetch_primary!(&app, token, &s1),
        b"SERIES-POSTER".to_vec(),
        "art-less season falls back to the series poster"
    );
}
