//! /Items?IsReadable= contract (B1470, V1449).
//!
//! Disarm check: with `ItemQuery::from(&ListQuery)` back to
//! `book_readable: None`, `IsReadable=true` returns the locked book too and
//! the first test goes red.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{
    BookFormat, BookMeta, MediaItem, MediaKind, MediaStore, SecretString, TokenStore, UserId,
    UserPolicy, UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

async fn seed_books() -> (web::Data<AppState>, String) {
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
    for (id, title, ext, format) in [
        (1u64, "Converted Kindle", "azw3", BookFormat::Epub),
        (2, "Locked Kindle", "azw3", BookFormat::Unreadable),
        (3, "Plain Epub", "epub", BookFormat::Epub),
    ] {
        stores
            .put(MediaItem {
                id,
                path: format!("/b/{id}.{ext}").into(),
                title: title.into(),
                kind: MediaKind::Book,
                book: Some(BookMeta {
                    format,
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await
            .unwrap();
    }
    let token = stores.issue(uid, "t").await.unwrap();
    let state = web::Data::new(AppState::new(stores, "srv".into()));
    (state, token.0.expose().to_string())
}

async fn listing(state: web::Data<AppState>, token: &str, qs: &str) -> (Vec<String>, u64) {
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri(&format!(
                "/Items?IncludeItemTypes=Book&Recursive=true&SortBy=SortName&Limit=100{qs}"
            ))
            .insert_header(("X-Emby-Token", token))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let names = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap_or("").to_owned())
        .collect();
    (names, v["TotalRecordCount"].as_u64().unwrap())
}

#[actix_web::test]
async fn is_readable_true_leaves_out_books_no_client_can_open() {
    let (state, token) = seed_books().await;
    let (names, total) = listing(state, &token, "&IsReadable=true").await;
    assert_eq!(names, ["Converted Kindle", "Plain Epub"]);
    assert_eq!(total, 2, "the total counts only what is kept");
}

#[actix_web::test]
async fn is_readable_false_keeps_only_the_locked_ones() {
    let (state, token) = seed_books().await;
    let (names, total) = listing(state, &token, "&IsReadable=false").await;
    assert_eq!(names, ["Locked Kindle"]);
    assert_eq!(total, 1);
}

#[actix_web::test]
async fn without_is_readable_every_book_is_listed() {
    let (state, token) = seed_books().await;
    let (names, _) = listing(state, &token, "").await;
    assert_eq!(names, ["Converted Kindle", "Locked Kindle", "Plain Epub"]);
}
