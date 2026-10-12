#![allow(clippy::unwrap_used, clippy::expect_used)]
//! 004-books (T075) — FR-009: reopening a part-read book returns to where you
//! stopped.
//!
//! A book's read position travels on the SAME `UserData.PlaybackPositionTicks`
//! a film's resume position does. The readers already report it — `bookPlayer`
//! sends the epub.js CFI offset, `pdfPlayer` the page number × 10000 — so
//! nothing new is invented here; what is asserted is that the existing
//! reporting path does not quietly reject an item with no media source.
//!
//! # RunTimeTicks stays 0, and that is the interesting part
//!
//! A book has no time axis (R8), so there is no duration to divide by.
//! `PlayedPercentage` must therefore be 0 rather than NaN or Infinity: a NaN
//! reaches the client as `null` through serde and a progress bar renders as
//! either full or broken, which looks like corrupted state rather than the
//! absence of a runtime. Division by a zero runtime is exactly where that
//! would come from, so it is asserted rather than assumed.

use actix_web::{http::StatusCode, test, web, App};
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

/// One tick is 100 ns; `bookPlayer` reports its position on the same scale
/// everything else does.
const POSITION: u64 = 1_234_000;

async fn seed() -> (web::Data<AppState>, String, UserId) {
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
            path: "/media/Books/Dune.epub".into(),
            title: "Dune".into(),
            kind: MediaKind::Book,
            book: Some(BookMeta {
                format: BookFormat::Epub,
                author: Some("Frank Herbert".into()),
                ..Default::default()
            }),
            probe: MediaProbe {
                size_bytes: Some(1_234_567),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();

    // A film alongside, so every assertion below is shown to be about BOOKS
    // rather than about progress reporting having broken for everything.
    stores
        .put(MediaItem {
            id: 43,
            path: "/media/Movies/Alien.mkv".into(),
            title: "Alien".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(7_020_000),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();

    let state = web::Data::new(AppState::new(stores, "srv".into()));
    (state, token.0.expose().to_string(), uid)
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

/// The wire id for internal id 42 — `{id:032x}`, which is what a client echoes
/// back on a progress report.
const BOOK_WIRE_ID: &str = "0000000000000000000000000000002a";
/// …and for the film (id 43). Exactly 32 hex digits; 31 parses as nothing and
/// the report is silently dropped, which is how a resume position stops
/// persisting without anything erroring (B15).
const FILM_WIRE_ID: &str = "0000000000000000000000000000002b";

#[actix_web::test]
async fn a_books_read_position_round_trips_through_userdata() {
    let (state, token, uid) = seed().await;
    let app = test::init_service(app(state)).await;

    // Report progress exactly as a reader does: the canonical wire id, a
    // PlaySessionId, and a position. No media source is involved anywhere.
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/Sessions/Playing/Progress")
            .insert_header(auth_header!(&token))
            .set_json(serde_json::json!({
                "ItemId": BOOK_WIRE_ID,
                "PlaySessionId": "sess-book",
                "PositionTicks": POSITION,
                "IsPaused": false,
            }))
            .to_request(),
    )
    .await;
    assert!(
        resp.status().is_success(),
        "a book must not be rejected for having no media source: {:?}",
        resp.status()
    );

    // It comes back on the item, which is where the reader reads it on reopen.
    let body: serde_json::Value = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/Users/{}/Items/42", uid.0.simple()))
                .insert_header(auth_header!(&token))
                .to_request(),
        )
        .await,
    )
    .await;

    assert_eq!(body["Type"], "Book");
    assert_eq!(
        body["UserData"]["PlaybackPositionTicks"], POSITION,
        "the read position must survive the round trip: {body}"
    );
    assert_eq!(
        body["RunTimeTicks"], 0,
        "a book still has no time axis — the position is an offset, not an \
         elapsed duration (R8)"
    );
    // The NaN guard. `position / runtime` with runtime 0 is NaN, serde writes
    // NaN as null, and a null percentage renders as a full or broken bar —
    // which reads as corrupted state rather than as "this thing has no length".
    let pct = &body["UserData"]["PlayedPercentage"];
    assert!(
        pct.is_number(),
        "PlayedPercentage must be a number, not null: dividing by a zero \
         runtime is exactly where a NaN comes from: {body}"
    );
    assert_eq!(
        pct.as_f64(),
        Some(0.0),
        "with no runtime there is no percentage to report, and 0 is the honest \
         answer: {body}"
    );
}

/// The counterweight: without it, a handler that ignored every progress report
/// would pass the test above.
#[actix_web::test]
async fn a_films_resume_position_still_reports_a_percentage() {
    let (state, token, uid) = seed().await;
    let app = test::init_service(app(state)).await;

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/Sessions/Playing/Progress")
            .insert_header(auth_header!(&token))
            .set_json(serde_json::json!({
                "ItemId": FILM_WIRE_ID,
                "PlaySessionId": "sess-film",
                // Half of 7_020_000 ms expressed in ticks.
                "PositionTicks": 35_100_000_000u64,
                "IsPaused": false,
            }))
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success());

    let body: serde_json::Value = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/Users/{}/Items/43", uid.0.simple()))
                .insert_header(auth_header!(&token))
                .to_request(),
        )
        .await,
    )
    .await;
    assert_ne!(body["RunTimeTicks"], 0, "a film keeps its duration");
    let pct = body["UserData"]["PlayedPercentage"].as_f64().unwrap();
    assert!(
        (49.0..=51.0).contains(&pct),
        "a film halfway through reports ~50%, so the percentage maths is live \
         rather than hardcoded to 0: got {pct}"
    );
}

/// A book must be able to finish, too — `/PlayedItems` is how a client marks it
/// read, and it has no runtime to infer completion from.
#[actix_web::test]
async fn a_book_can_be_marked_played_and_unplayed() {
    let (state, token, uid) = seed().await;
    let app = test::init_service(app(state)).await;

    let mark = |method: actix_web::http::Method| {
        let uri = format!("/Users/{}/PlayedItems/42", uid.0.simple());
        test::TestRequest::with_uri(&uri)
            .method(method)
            .insert_header(auth_header!(&token))
            .to_request()
    };

    let resp = test::call_service(&app, mark(actix_web::http::Method::POST)).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a book must be markable read"
    );

    let body: serde_json::Value = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/Users/{}/Items/42", uid.0.simple()))
                .insert_header(auth_header!(&token))
                .to_request(),
        )
        .await,
    )
    .await;
    assert_eq!(body["UserData"]["Played"], true, "{body}");

    let resp = test::call_service(&app, mark(actix_web::http::Method::DELETE)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/Users/{}/Items/42", uid.0.simple()))
                .insert_header(auth_header!(&token))
                .to_request(),
        )
        .await,
    )
    .await;
    assert_eq!(body["UserData"]["Played"], false, "{body}");
}
