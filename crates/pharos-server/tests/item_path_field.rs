#![allow(clippy::unwrap_used, clippy::expect_used)]
//! 004-books (T020) — `BaseItemDto.Path`, and its `Fields` gate.
//!
//! This one field decides whether the whole book feature works. All three
//! jellyfin-web readers gate on `canPlayItem`, which tests `item.Path` against
//! an extension. With `Path` absent every reader returns false and the client
//! declines to open the item with **no error, no toast and no network request**
//! — so there is nothing to see in a log and nothing to catch in a smoke test.
//! Hence an explicit assertion.
//!
//! The gate is asserted in both directions and in several spellings, because
//! V69 (serve every legal spelling a client dialect sends) is a recurring bug
//! class here: a silently-ignored camelCase parameter would disable books for
//! whichever client spells it that way.

use actix_web::{test, web, App};
use pharos_core::{
    BookFormat, BookMeta, MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore,
    UserId, UserPolicy, UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

const BOOK_PATH: &str = "/media/Books/Dune.epub";

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

    stores
        .put(MediaItem {
            id: 42,
            path: BOOK_PATH.into(),
            title: "Dune".into(),
            kind: MediaKind::Book,
            book: Some(BookMeta {
                format: BookFormat::Epub,
                author: Some("Frank Herbert".into()),
                ..Default::default()
            }),
            probe: MediaProbe::default(),
            series: None,
            created_at: Some(1_700_000_000),
            metadata: Default::default(),
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

    // A movie too: `Path` is Fields-gated for every kind EXCEPT books, so the
    // gate itself can only be tested on a non-book item.
    stores
        .put(MediaItem {
            id: 43,
            path: "/media/Movies/Alien.mkv".into(),
            title: "Alien".into(),
            kind: MediaKind::Movie,
            book: None,
            probe: MediaProbe::default(),
            series: None,
            created_at: Some(1_700_000_000),
            metadata: Default::default(),
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

/// A macro rather than a generic fn: actix's `init_service` returns an opaque
/// `impl Service` whose bounds are painful to name in a helper signature, and
/// the type gymnastics would obscure what these tests are actually asserting.
macro_rules! get_json {
    ($app:expr, $uri:expr, $token:expr) => {{
        let req = test::TestRequest::get()
            .uri($uri)
            .insert_header((
                "X-Emby-Authorization",
                format!(
                    "MediaBrowser Client=\"t\", Device=\"d\", DeviceId=\"i\", Version=\"1\", Token=\"{}\"",
                    $token
                ),
            ))
            .to_request();
        let resp = test::call_service(&$app, req).await;
        assert!(
            resp.status().is_success(),
            "{} returned {}",
            $uri,
            resp.status()
        );
        test::read_body_json::<serde_json::Value, _>(resp).await
    }};
}

/// A BOOK always carries `Path`, with or without `Fields`.
///
/// Proven necessary in a browser, not argued: jellyfin-web's details page
/// fetches `/Users/{uid}/Items/{id}` with NO `Fields` and hands that object to
/// `playbackManager`, whose readers all test `item.Path`. Gated, `Path` was
/// absent exactly there, and clicking Play did nothing — no iframe, no
/// `/Download` request, no error. If this is ever relaxed back to a gate, books
/// stop opening and nothing else fails.
#[actix_web::test]
async fn a_book_always_carries_its_path() {
    let (state, token) = seed().await;
    let app = test::init_service(app(state)).await;

    // No Fields at all — the request the details page actually makes.
    let body = get_json!(app, "/Items?IncludeItemTypes=Book", &token);
    let item = &body["Items"][0];
    assert_eq!(item["Name"], "Dune", "seed item not returned");
    assert_eq!(
        item["Path"], BOOK_PATH,
        "a book must carry Path even unasked, or every reader declines silently"
    );

    // The single-item fetch — the exact route that was broken.
    let detail = get_json!(app, "/Items/42", &token);
    assert_eq!(detail["Path"], BOOK_PATH);

    // Asking explicitly still yields the REAL stored path, not a synthesised one
    // shaped to satisfy the reader's extension test.
    let body = get_json!(
        app,
        "/Items?IncludeItemTypes=Book&Fields=CanDownload,Path",
        &token
    );
    assert_eq!(body["Items"][0]["Path"], BOOK_PATH);
}

/// Every OTHER kind keeps the gate, so no existing payload grew.
#[actix_web::test]
async fn path_is_absent_for_a_non_book_unless_the_client_asks_for_it() {
    let (state, token) = seed().await;
    let app = test::init_service(app(state)).await;

    let body = get_json!(app, "/Items?IncludeItemTypes=Movie", &token);
    let item = &body["Items"][0];
    assert_eq!(item["Name"], "Alien", "seed item not returned");
    assert!(
        item.get("Path").is_none(),
        "a movie must not leak its path unasked — least exposure is unchanged: {item}"
    );

    let body = get_json!(app, "/Items?IncludeItemTypes=Movie&Fields=Path", &token);
    assert_eq!(body["Items"][0]["Path"], "/media/Movies/Alien.mkv");
}

#[actix_web::test]
async fn the_path_field_is_recognised_in_every_spelling_a_client_may_send() {
    let (state, token) = seed().await;
    let app = test::init_service(app(state)).await;

    // V69. `CiQuery` normalises the parameter KEY and `fields_requests`
    // compares the field NAME case-insensitively, so every combination must
    // work. A client whose spelling was ignored would see books that open
    // nothing, with no error to diagnose.
    // A MOVIE, because books ignore the gate entirely — asserting spellings
    // against a book would pass no matter what `fields_requests` did.
    for uri in [
        "/Items?IncludeItemTypes=Movie&Fields=Path",
        "/Items?IncludeItemTypes=Movie&fields=path",
        "/Items?IncludeItemTypes=Movie&FIELDS=PATH",
        "/Items?IncludeItemTypes=Movie&fields=canDownload,path",
        // Whitespace around the comma-separated token, as some clients emit.
        "/Items?IncludeItemTypes=Movie&Fields=CanDownload,%20Path",
    ] {
        let body = get_json!(app, uri, &token);
        assert_eq!(
            body["Items"][0]["Path"], "/media/Movies/Alien.mkv",
            "Path missing for spelling: {uri}"
        );
    }
}

#[actix_web::test]
async fn the_detail_payload_agrees_with_the_list_payload_about_path() {
    let (state, token) = seed().await;
    let app = test::init_service(app(state)).await;

    // The single-item fetch reads its own `Fields`, so the two routes could
    // disagree — an item showing Path in a grid row but not on its detail page,
    // or the reverse. That divergence is exactly what broke the reader, so both
    // routes are asserted for both kinds.
    let detail = get_json!(app, "/Items/43", &token);
    assert!(
        detail.get("Path").is_none(),
        "a movie's detail payload must omit Path when not asked: {detail}"
    );
    let detail = get_json!(app, "/Items/43?Fields=Path", &token);
    assert_eq!(detail["Path"], "/media/Movies/Alien.mkv");

    let detail = get_json!(app, "/Items/42", &token);
    assert_eq!(
        detail["Path"], BOOK_PATH,
        "a book's detail payload carries Path unasked — this is the route that was broken"
    );
}
