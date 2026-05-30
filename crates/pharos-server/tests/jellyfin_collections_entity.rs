//! LIB-C5 — collections / box sets as real entities, both NFO-driven and
//! manual CRUD.
//!
//! Covers:
//!  - NFO-driven membership (store `link_item_collections`, as the scanner
//!    wire-in does) surfaces in /Collections + ParentId pivot.
//!  - /Collections lists box-set rows (Name + 32-hex wire id Id, BoxSet
//!    type, member count), name-ordered.
//!  - /Items?ParentId=<collection id> resolves the members in curated
//!    sort_order via the collection_items join.
//!  - /Items/{collection wire id} returns the BoxSet BaseItemDto.
//!  - /Items?IncludeItemTypes=BoxSet lists the box sets (Collections view).
//!  - Manual CRUD: POST /Collections (create, optionally seeded),
//!    POST /Collections/{id}/Items (add), DELETE /Collections/{id}/Items
//!    (remove), then browse the result.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{
    collection_wire_id, CollectionStore, MediaItem, MediaKind, MediaProbe, MediaStore,
    SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore,
};
use pharos_jellyfin_api::dto::collection_id_for;
use pharos_server::{api::jellyfin, auth::BuiltinAuth, middleware::LowercasePath, state::AppState};
use pharos_store_sqlx::sqlite::SqliteStore;

async fn seed() -> (web::Data<AppState>, String, SqliteStore) {
    let stores = SqliteStore::connect("sqlite::memory:").await.unwrap();
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
    // Three movies. The box set "Trilogy" holds 3 and 1 (in that order),
    // mirroring the scanner persisting two films' NFO <set> tags.
    for (id, title) in [(1u64, "A"), (2, "B"), (3, "C")] {
        stores
            .put(MediaItem {
                id,
                path: format!("/m/{id}.mkv").into(),
                title: title.into(),
                kind: MediaKind::Movie,
                probe: MediaProbe::default(),
                ..Default::default()
            })
            .await
            .unwrap();
    }
    // NFO-driven: link item 3 first, then item 1 — sort_order, not id, wins.
    stores
        .link_item_collections(3, &["Trilogy".into()])
        .await
        .unwrap();
    stores
        .link_item_collections(1, &["Trilogy".into()])
        .await
        .unwrap();
    let token = stores.issue(uid, "t").await.unwrap();
    let state = web::Data::new(AppState::new(stores.clone(), "srv".into()));
    (state, token.0.expose().to_string(), stores)
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

async fn get_json(state: web::Data<AppState>, token: &str, uri: &str) -> serde_json::Value {
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri(uri)
            .insert_header(("X-Emby-Token", token))
            .to_request(),
    )
    .await;
    serde_json::from_slice(&body).unwrap()
}

#[actix_web::test]
async fn collections_list_emits_boxset_rows_with_wire_id_and_counts() {
    let (state, token, _s) = seed().await;
    let v = get_json(state, &token, "/Collections").await;
    let items = v["Items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    let c = &items[0];
    assert_eq!(c["Name"].as_str().unwrap(), "Trilogy");
    assert_eq!(c["Id"].as_str().unwrap(), collection_id_for("Trilogy"));
    assert_eq!(c["Type"].as_str().unwrap(), "BoxSet");
    assert!(c["IsFolder"].as_bool().unwrap());
    assert_eq!(c["ChildCount"].as_u64().unwrap(), 2);
    assert_eq!(v["TotalRecordCount"].as_u64().unwrap(), 1);
}

#[actix_web::test]
async fn parent_id_collection_resolves_members_in_sort_order() {
    let (state, token, _s) = seed().await;
    let cid = collection_id_for("Trilogy");
    let v = get_json(state, &token, &format!("/Items?ParentId={cid}&Limit=100")).await;
    // Curated order: item 3 (C) was linked first, then item 1 (A) — NOT id
    // order. Proves the sort_order pivot, not the id-ordered store list.
    let titles: Vec<&str> = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap())
        .collect();
    assert_eq!(titles, vec!["C", "A"], "members in curated sort_order");
}

#[actix_web::test]
async fn items_by_collection_wire_id_returns_boxset_dto() {
    let (state, token, _s) = seed().await;
    let cid = collection_id_for("Trilogy");
    let v = get_json(state, &token, &format!("/Items/{cid}")).await;
    assert_eq!(v["Id"].as_str().unwrap(), cid);
    assert_eq!(v["Name"].as_str().unwrap(), "Trilogy");
    assert_eq!(v["Type"].as_str().unwrap(), "BoxSet");
    assert!(v["IsFolder"].as_bool().unwrap());
    assert_eq!(v["ChildCount"].as_u64().unwrap(), 2);
}

#[actix_web::test]
async fn items_include_boxset_lists_collections() {
    let (state, token, _s) = seed().await;
    let v = get_json(state, &token, "/Items?IncludeItemTypes=BoxSet&Limit=100").await;
    let items = v["Items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "the one box set, not the media items");
    assert_eq!(items[0]["Type"].as_str().unwrap(), "BoxSet");
    assert_eq!(items[0]["Name"].as_str().unwrap(), "Trilogy");
}

#[actix_web::test]
async fn manual_crud_create_add_remove_and_browse() {
    let (state, token, _s) = seed().await;
    let app = test::init_service(build_app(state.clone())).await;

    // POST /Collections?Name=MySet&Ids=2,1 — create seeded with [2, 1].
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::post()
            .uri("/Collections?Name=MySet&Ids=2,1")
            .insert_header(("X-Emby-Token", token.clone()))
            .to_request(),
    )
    .await;
    let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let cid = collection_id_for("MySet");
    assert_eq!(created["Id"].as_str().unwrap(), cid);
    assert_eq!(created["Type"].as_str().unwrap(), "BoxSet");
    assert_eq!(created["ChildCount"].as_u64().unwrap(), 2);

    // Browse — members in seed order [2, 1] → titles B, A.
    let v = get_json(
        state.clone(),
        &token,
        &format!("/Items?ParentId={cid}&Limit=100"),
    )
    .await;
    let titles: Vec<&str> = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap())
        .collect();
    assert_eq!(titles, vec!["B", "A"]);

    // POST /Collections/{id}/Items?Ids=3 — add item 3 (appends).
    let app = test::init_service(build_app(state.clone())).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/Collections/{cid}/Items?Ids=3"))
            .insert_header(("X-Emby-Token", token.clone()))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 204);

    let v = get_json(
        state.clone(),
        &token,
        &format!("/Items?ParentId={cid}&Limit=100"),
    )
    .await;
    let titles: Vec<&str> = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap())
        .collect();
    assert_eq!(titles, vec!["B", "A", "C"], "3 appended after the seed");

    // DELETE /Collections/{id}/Items?Ids=1 — remove item 1.
    let app = test::init_service(build_app(state.clone())).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::delete()
            .uri(&format!("/Collections/{cid}/Items?Ids=1"))
            .insert_header(("X-Emby-Token", token.clone()))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 204);

    let v = get_json(state, &token, &format!("/Items?ParentId={cid}&Limit=100")).await;
    let titles: Vec<&str> = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap())
        .collect();
    assert_eq!(titles, vec!["B", "C"], "item 1 removed");
}

#[actix_web::test]
async fn manual_crud_on_unknown_collection_is_404() {
    let (state, token, _s) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let bogus = "ffffffffffffffffffffffffffffffff";
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/Collections/{bogus}/Items?Ids=1"))
            .insert_header(("X-Emby-Token", token.clone()))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 404);
    let resp = test::call_service(
        &app,
        test::TestRequest::delete()
            .uri(&format!("/Collections/{bogus}/Items?Ids=1"))
            .insert_header(("X-Emby-Token", token))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 404);
}

#[actix_web::test]
async fn wire_id_matches_core_helper() {
    // The DTO helper, the store wire_id, and the ParentId pivot all agree.
    assert_eq!(collection_id_for("Trilogy"), collection_wire_id("Trilogy"));
}
