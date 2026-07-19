#![allow(clippy::unwrap_used, clippy::expect_used)]
//! B97 — `/Items/Latest` ("Recently Added") must order most-recently-ADDED
//! first (DateCreated desc). Before the fix it returned items in cache/id order,
//! so a freshly-scanned film (newest DateCreated) never surfaced in the home row.

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

async fn seed() -> (web::Data<AppState>, String, UserId) {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("pw")).unwrap();
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "ali".into(),
            password_hash: hash,
            policy: UserPolicy {
                admin: true,
                ..Default::default()
            },
        })
        .await
        .unwrap();
    let token = stores.issue(uid, "test").await.unwrap();

    // Insert in NON-chronological id order so a correct result can only come
    // from sorting by created_at, never from insertion / id order. (The store
    // backfills `created_at` to now() when it's None, so every row carries an
    // explicit distinct timestamp here — the sort is what must order them.)
    let rows = [
        (10u64, "middle", 2_000i64),
        (30, "newest", 9_000),
        (20, "oldest", 500),
        (40, "older", 1_000),
    ];
    for (id, title, created_at) in rows {
        stores
            .put(MediaItem {
                id,
                path: format!("/media/Movies/{title}.mkv").into(),
                title: title.into(),
                kind: MediaKind::Movie,
                created_at: Some(created_at),
                ..Default::default()
            })
            .await
            .unwrap();
    }
    let state = web::Data::new(AppState::new(stores, "srv".into()));
    (state, token.0.expose().to_string(), uid)
}

fn build_app(
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

#[actix_web::test]
async fn latest_orders_by_date_created_desc() {
    let (state, token, uid) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri(&format!(
            "/Users/{}/Items/Latest?IncludeItemTypes=Movie&Limit=10",
            uid.0.simple()
        ))
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let names: Vec<&str> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["newest", "middle", "older", "oldest"],
        "Latest must be DateCreated desc, got {names:?}"
    );
}

#[actix_web::test]
async fn latest_newest_is_first_and_respects_limit() {
    let (state, token, uid) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri(&format!(
            "/Users/{}/Items/Latest?IncludeItemTypes=Movie&Limit=1",
            uid.0.simple()
        ))
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 1, "Limit=1 must return exactly one item");
    assert_eq!(
        arr[0]["Name"], "newest",
        "the single Latest item must be the most-recently-added"
    );
}
