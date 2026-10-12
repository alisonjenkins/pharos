#![allow(clippy::unwrap_used, clippy::expect_used)]
//! 004-books (T037/T041) — FR-008, FR-010, SC-004: a book is inert.
//!
//! The risk this guards is concrete. Give a book a `MediaSources` entry and a
//! client will happily construct `/Videos/{id}/stream` for it and ask pharos to
//! transcode an epub. So the assertion is that there is nothing to act on.
//!
//! **Empty, not absent.** `dto.rs` records why array fields are default-empty
//! across pharos: jellyfin-web iterates them without null guards, so omitting
//! one throws `Symbol.iterator` during view init (T30). And `RunTimeTicks` is a
//! plain `u64`, so `null` is not available without changing the field for every
//! item kind. What matters is that a client has nothing to request a stream FOR
//! — the JSON spelling of nothing was never the point.

use actix_web::{http::StatusCode, test, web, App};
use pharos_core::{
    BookFormat, BookMeta, MediaItem, MediaKind, MediaMetadata, MediaProbe, MediaStore,
    SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

async fn seed() -> (web::Data<AppState>, String) {
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

    // A book carrying a size, so "empty MediaSources" cannot pass merely because
    // the probe is blank — the DTO has real data it could have leaked.
    stores
        .put(MediaItem {
            id: 42,
            path: "/media/Books/Dune.epub".into(),
            title: "Dune".into(),
            kind: MediaKind::Book,
            book: Some(BookMeta {
                format: BookFormat::Epub,
                page_count: None,
                author: Some("Frank Herbert".into()),
                ..Default::default()
            }),
            probe: MediaProbe {
                size_bytes: Some(1_234_567),
                ..Default::default()
            },
            series: None,
            created_at: Some(1_700_000_000),
            metadata: MediaMetadata::default(),
            has_primary_art: false,
            art_version: 0,
            match_provider: None,
            match_external_id: None,
            match_source: None,
            match_confidence: None,
            metadata_refreshed_at: None,
        })
        .await
        .unwrap();

    // A movie alongside, so every assertion below is shown to be about BOOKS and
    // not about the endpoint having gone inert for everything.
    stores
        .put(MediaItem {
            id: 43,
            path: "/media/Movies/Alien.mkv".into(),
            title: "Alien".into(),
            kind: MediaKind::Movie,
            book: None,
            probe: MediaProbe {
                duration_ms: Some(7_020_000),
                container: Some("mkv".into()),
                video_codec: Some("h264".into()),
                audio_codec: Some("aac".into()),
                width: Some(1920),
                height: Some(1080),
                ..Default::default()
            },
            series: None,
            created_at: Some(1_700_000_000),
            metadata: MediaMetadata::default(),
            has_primary_art: false,
            art_version: 0,
            match_provider: None,
            match_external_id: None,
            match_source: None,
            match_confidence: None,
            metadata_refreshed_at: None,
        })
        .await
        .unwrap();

    let state = web::Data::new(AppState::new(stores, "srv".into()));
    (state, token.0.expose().to_string())
}

fn app(
    state: web::Data<AppState>,
) -> App<
    impl actix_web::dev::ServiceFactory<
        actix_web::dev::ServiceRequest,
        Config = (),
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
        InitError = (),
    >,
> {
    App::new()
        .app_data(state)
        .wrap(LowercasePath)
        .configure(jellyfin::configure)
}

macro_rules! auth_header {
    ($token:expr) => {
        (
            "X-Emby-Authorization",
            format!(
                "MediaBrowser Client=\"t\", Device=\"d\", DeviceId=\"i\", Version=\"1\", Token=\"{}\"",
                $token
            ),
        )
    };
}

#[actix_web::test]
async fn a_book_item_offers_nothing_to_play() {
    let (state, token) = seed().await;
    let app = test::init_service(app(state)).await;

    let req = test::TestRequest::get()
        .uri("/Items?IncludeItemTypes=Book")
        .insert_header(auth_header!(&token))
        .to_request();
    let body: serde_json::Value = test::read_body_json(test::call_service(&app, req).await).await;
    let item = &body["Items"][0];

    assert_eq!(item["Type"], "Book", "Type drives the grid's card shape");
    assert_eq!(
        item["MediaType"], "Book",
        "MediaType is the FIRST gate all three readers check; \"Video\" here \
         silently opens nothing"
    );

    // Empty, and present. Both halves matter: absent would crash a view init,
    // non-empty would invite a transcode.
    assert!(
        item["MediaSources"].is_array(),
        "MediaSources must be present as an array, not omitted (T30)"
    );
    assert_eq!(
        item["MediaSources"].as_array().map(Vec::len),
        Some(0),
        "a book must offer no media source: {item}"
    );
    assert_eq!(
        item["RunTimeTicks"], 0,
        "a book has no time axis; 0 rather than a fabricated duration (R8)"
    );

    // No frame-derived artwork is advertised. A cover-less book claiming a
    // Primary tag 404s on every grid render — the B149 shape.
    let tags = &item["ImageTags"];
    assert!(
        tags.get("Backdrop").is_none() && tags.get("Thumb").is_none(),
        "a book has no frames, so no backdrop or thumb may be advertised: {tags}"
    );
    assert!(
        tags.get("Primary").is_none(),
        "this book has no cover, so no Primary tag may be advertised: {tags}"
    );
}

#[actix_web::test]
async fn a_movie_still_offers_a_media_source() {
    // The counterweight: without this, gutting MediaSources for everything would
    // pass the test above.
    let (state, token) = seed().await;
    let app = test::init_service(app(state)).await;

    let req = test::TestRequest::get()
        .uri("/Items?IncludeItemTypes=Movie")
        .insert_header(auth_header!(&token))
        .to_request();
    let body: serde_json::Value = test::read_body_json(test::call_service(&app, req).await).await;
    let item = &body["Items"][0];

    assert_eq!(item["MediaType"], "Video");
    assert_eq!(
        item["MediaSources"].as_array().map(Vec::len),
        Some(1),
        "a movie must still be playable"
    );
    assert_ne!(item["RunTimeTicks"], 0, "a movie keeps its duration");
}

#[actix_web::test]
async fn playbackinfo_offers_no_source_for_a_book() {
    let (state, token) = seed().await;
    let app = test::init_service(app(state)).await;

    // FR-010. The route accepts any item id, and this rule previously lived only
    // in a contract document — so nothing enforced it.
    for method in [actix_web::http::Method::GET, actix_web::http::Method::POST] {
        let req = test::TestRequest::with_uri("/Items/42/PlaybackInfo")
            .method(method.clone())
            .insert_header(auth_header!(&token))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK, "{method} PlaybackInfo");
        let body: serde_json::Value = test::read_body_json(resp).await;

        assert_eq!(
            body["MediaSources"].as_array().map(Vec::len),
            Some(0),
            "{method} PlaybackInfo must offer a book no source: {body}"
        );
        assert!(
            body["PlaySessionId"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "a PlaySessionId is still issued — a book reports read progress (FR-009)"
        );
        // Nothing anywhere in the payload should hand back a transcode URL.
        assert!(
            !serde_json::to_string(&body)
                .unwrap()
                .contains("TranscodingUrl"),
            "no TranscodingUrl may be offered for a book: {body}"
        );
    }
}

#[actix_web::test]
async fn playbackinfo_still_negotiates_for_a_movie() {
    let (state, token) = seed().await;
    let app = test::init_service(app(state)).await;

    let req = test::TestRequest::post()
        .uri("/Items/43/PlaybackInfo")
        .insert_header(auth_header!(&token))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(
        body["MediaSources"].as_array().map(Vec::len),
        Some(1),
        "the book early-return must not have swallowed the negotiation path"
    );
}
