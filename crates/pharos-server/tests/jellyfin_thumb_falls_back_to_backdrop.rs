#![allow(clippy::unwrap_used, clippy::expect_used)]
//! B122 — a movie's Thumb must be its backdrop, not an arbitrary video frame.
//!
//! Neither online provider publishes a thumb image for a film: TMDB's
//! `/movie/{id}/images` carries `posters` / `backdrops` / `logos` only, and
//! TVDB the same. So an enriched movie ends up with a Backdrop artwork row and
//! no Thumb row, and `/Items/{id}/Images/Thumb` fell through to extracting a
//! frame at `image_seek_seconds` — which on a film with a dark opening is the
//! title card, the "ugly and not useful" landscape card users reported.
//!
//! A backdrop IS the curated landscape still a Thumb wants, so serve it. The
//! frame extract stays the last resort for an item with no artwork at all.

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaStore, SecretString, TokenStore, UserId, UserPolicy, UserRecord,
    UserStore,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

/// Seed one movie whose artwork rows are exactly what enrichment leaves behind:
/// a Primary and a Backdrop from `tmdb`, and no Thumb.
async fn seed(
    art_dir: &std::path::Path,
    cache_dir: &std::path::Path,
) -> (web::Data<AppState>, String) {
    let backdrop = art_dir.join("backdrop.jpg");
    std::fs::write(&backdrop, b"CURATED-BACKDROP").unwrap();
    let poster = art_dir.join("poster.jpg");
    std::fs::write(&poster, b"CURATED-POSTER").unwrap();

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
    stores
        .put(MediaItem {
            id: 7,
            // Deliberately unreadable: the frame-extract fallback must never be
            // what satisfies this request.
            path: "/no/such/movie.mkv".into(),
            title: "Project Hail Mary".into(),
            kind: MediaKind::Movie,
            ..Default::default()
        })
        .await
        .unwrap();
    stores
        .set_artwork(7, "Primary", "tmdb", &poster.to_string_lossy())
        .await
        .unwrap();
    stores
        .set_artwork(7, "Backdrop", "tmdb", &backdrop.to_string_lossy())
        .await
        .unwrap();
    let state = AppState::new(stores, "t".into())
        .with_image_cache(pharos_cache::ImageCache::new(cache_dir));
    (web::Data::new(state), token.0.expose().to_string())
}

macro_rules! fetch {
    ($app:expr, $token:expr, $role:expr) => {{
        let req = test::TestRequest::get()
            .uri(&format!("/Items/7/Images/{}", $role))
            .insert_header(("X-Emby-Token", $token.as_str()))
            .to_request();
        test::call_and_read_body(&$app, req).await.to_vec()
    }};
}

#[actix_web::test]
async fn a_movies_thumb_is_its_backdrop_when_no_thumb_art_exists() {
    let art = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let (state, token) = seed(art.path(), cache.path()).await;
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;

    assert_eq!(
        fetch!(app, token, "Thumb"),
        b"CURATED-BACKDROP",
        "an enriched movie has no Thumb row, so its Thumb must come from the \
         backdrop — falling through to a frame extract is the dark title card bug"
    );
    // The other roles are untouched by the fallback.
    assert_eq!(fetch!(app, token, "Backdrop"), b"CURATED-BACKDROP");
    assert_eq!(fetch!(app, token, "Primary"), b"CURATED-POSTER");
}

#[actix_web::test]
async fn a_recorded_thumb_still_wins_over_the_backdrop() {
    let art = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let (state, token) = seed(art.path(), cache.path()).await;
    let thumb = art.path().join("landscape.jpg");
    std::fs::write(&thumb, b"OWN-THUMB").unwrap();
    {
        use pharos_core::MediaStore;
        state
            .stores
            .set_artwork(7, "Thumb", "local", &thumb.to_string_lossy())
            .await
            .unwrap();
    }
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;

    assert_eq!(
        fetch!(app, token, "Thumb"),
        b"OWN-THUMB",
        "a user's own landscape sidecar must outrank the backdrop fallback"
    );
}
