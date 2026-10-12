//! Pagination contract for /Items.
//!
//! Required behaviour (matches Jellyfin's wire shape):
//!   - `TotalRecordCount` is the count of the full filtered set,
//!     NEVER the page slice.
//!   - `Items` is the page slice — at most `Limit` entries, starting
//!     at `StartIndex`.
//!   - `StartIndex` past the end yields empty `Items` but `Total`
//!     stays correct.
//!   - Sweeping (Start, Limit) across the whole set reconstructs
//!     the full list with NO duplicates and NO holes.
//!
//! A bug that returned `Items.len()` as `TotalRecordCount` would
//! silently break jellyfin-web's "Showing X of Y" footer + infinite
//! scroll; a bug that double-counted entries would break it worse.

#![allow(clippy::unwrap_used, clippy::expect_used)]

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
use std::collections::HashSet;

const TOTAL_ITEMS: usize = 23; // deliberately not a power-of-two

async fn seed_n_items(n: usize) -> (web::Data<AppState>, String) {
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
    for i in 1..=n {
        stores
            .put(MediaItem {
                id: i as u64,
                path: format!("/m/item{i:02}.mkv").into(),
                title: format!("Item {i:02}"),
                kind: MediaKind::Movie,
                ..Default::default()
            })
            .await
            .unwrap();
    }
    let token = stores.issue(uid, "t").await.unwrap();
    let state = web::Data::new(AppState::new(stores, "srv".into()));
    (state, token.0.expose().to_string())
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
async fn total_record_count_reflects_full_set_not_page_slice() {
    let (state, token) = seed_n_items(TOTAL_ITEMS).await;
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Items?StartIndex=5&Limit=3")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        v["TotalRecordCount"].as_u64(),
        Some(TOTAL_ITEMS as u64),
        "Total must reflect full set, not page; got {v}"
    );
    assert_eq!(v["Items"].as_array().unwrap().len(), 3);
    assert_eq!(v["StartIndex"].as_u64(), Some(5));
}

#[actix_web::test]
async fn start_past_end_yields_empty_items_with_total_intact() {
    let (state, token) = seed_n_items(TOTAL_ITEMS).await;
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Items?StartIndex=999&Limit=10")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v["Items"].as_array().unwrap().is_empty());
    assert_eq!(
        v["TotalRecordCount"].as_u64(),
        Some(TOTAL_ITEMS as u64),
        "past-end paging must still report full Total"
    );
}

#[actix_web::test]
async fn limit_zero_yields_empty_page_but_full_total() {
    let (state, token) = seed_n_items(TOTAL_ITEMS).await;
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Items?StartIndex=0&Limit=0")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v["Items"].as_array().unwrap().is_empty());
    assert_eq!(v["TotalRecordCount"].as_u64(), Some(TOTAL_ITEMS as u64));
}

#[actix_web::test]
async fn pagination_sweep_reconstructs_full_set_with_no_dupes_or_holes() {
    let (state, token) = seed_n_items(TOTAL_ITEMS).await;
    let app = test::init_service(build_app(state)).await;
    let page_size = 5u32;
    let mut seen: HashSet<u64> = HashSet::new();
    let mut start = 0u32;
    loop {
        let body = test::call_and_read_body(
            &app,
            test::TestRequest::get()
                .uri(&format!("/Items?StartIndex={start}&Limit={page_size}"))
                .insert_header(("X-Emby-Token", token.as_str()))
                .to_request(),
        )
        .await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let items = v["Items"].as_array().unwrap();
        if items.is_empty() {
            break;
        }
        for it in items {
            // BaseItemDto.Id is the 32-hex stringified u64 — parse
            // back to verify integrity.
            let id_str = it["Id"].as_str().unwrap();
            let id: u64 = u64::from_str_radix(id_str, 16).unwrap();
            assert!(
                seen.insert(id),
                "duplicate item id {id} across pages (start={start})"
            );
        }
        start += page_size;
        // Hard cap so a bug that returns the full set every page
        // doesn't loop forever.
        assert!(start < 1_000, "pagination did not terminate");
    }
    assert_eq!(
        seen.len(),
        TOTAL_ITEMS,
        "pagination reconstruction missed items; got {} of {TOTAL_ITEMS}",
        seen.len()
    );
}
