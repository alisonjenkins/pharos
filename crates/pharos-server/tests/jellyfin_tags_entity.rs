//! LIB-C6 — tags as real entities. /Tags lists tag rows (name + 32-hex
//! wire id Id) with counts; /Items?ParentId=<tag id> resolves through the
//! item_tags indexed join; ?Tags=a,b filters with AND semantics; an item's
//! /Items/{id} DTO carries its tags under `Tags`; and POST/DELETE
//! /Items/{id}/Tags mutate the join manually. Replaces the old hardcoded
//! empty `Tags`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{
    tag_wire_id, MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TagStore, TokenStore,
    UserId, UserPolicy, UserRecord, UserStore,
};
use pharos_jellyfin_api::dto::tag_id_for;
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
    // Two items. Item 1 carries 1080p + cyberpunk; item 2 carries 1080p
    // only, so 1080p's count is 2 and cyberpunk's is 1.
    for (id, title) in [(1u64, "A"), (2, "B")] {
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
    stores
        .link_item_tags(1, &["1080p".into(), "cyberpunk".into()])
        .await
        .unwrap();
    stores.link_item_tags(2, &["1080p".into()]).await.unwrap();
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
async fn tags_list_emits_rows_with_wire_id_and_counts() {
    let (state, token) = seed().await;
    let v = get_json(state, &token, "/Tags").await;
    let items = v["Items"].as_array().unwrap();
    // Name-ordered: 1080p (count 2), cyberpunk (count 1).
    let names: Vec<&str> = items.iter().map(|i| i["Name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["1080p", "cyberpunk"]);
    let hd = &items[0];
    assert_eq!(hd["Id"].as_str().unwrap(), tag_id_for("1080p"));
    assert_eq!(hd["Type"].as_str().unwrap(), "Tag");
    assert!(hd["IsFolder"].as_bool().unwrap());
    assert_eq!(hd["ChildCount"].as_u64().unwrap(), 2);
    let cp = &items[1];
    assert_eq!(cp["Id"].as_str().unwrap(), tag_id_for("cyberpunk"));
    assert_eq!(cp["ChildCount"].as_u64().unwrap(), 1);
    assert_eq!(v["TotalRecordCount"].as_u64().unwrap(), 2);
}

#[actix_web::test]
async fn parent_id_tag_resolves_to_tagged_items() {
    let (state, token) = seed().await;
    // ParentId = 1080p → items 1 and 2.
    let hd = tag_id_for("1080p");
    let v = get_json(
        state.clone(),
        &token,
        &format!("/Items?ParentId={hd}&Limit=100"),
    )
    .await;
    let mut titles: Vec<&str> = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap())
        .collect();
    titles.sort_unstable();
    assert_eq!(titles, vec!["A", "B"], "both 1080p items");
    // ParentId = cyberpunk → only item 1.
    let cp = tag_id_for("cyberpunk");
    let v = get_json(state, &token, &format!("/Items?ParentId={cp}&Limit=100")).await;
    let titles: Vec<&str> = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap())
        .collect();
    assert_eq!(titles, vec!["A"], "only the cyberpunk item");
}

#[actix_web::test]
async fn tags_filter_is_and_across_names() {
    let (state, token) = seed().await;
    // ?Tags=1080p → both items.
    let v = get_json(state.clone(), &token, "/Items?Tags=1080p&Limit=100").await;
    let mut titles: Vec<&str> = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap())
        .collect();
    titles.sort_unstable();
    assert_eq!(titles, vec!["A", "B"]);
    // ?Tags=1080p,cyberpunk → AND → only item 1 carries both.
    let v = get_json(
        state.clone(),
        &token,
        "/Items?Tags=1080p,cyberpunk&Limit=100",
    )
    .await;
    let titles: Vec<&str> = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap())
        .collect();
    assert_eq!(titles, vec!["A"], "AND: only the item with both tags");
    // A tag no item carries → empty.
    let v = get_json(state, &token, "/Items?Tags=nope&Limit=100").await;
    assert!(v["Items"].as_array().unwrap().is_empty());
}

#[actix_web::test]
async fn item_dto_carries_its_tags() {
    let (state, token) = seed().await;
    let v = get_json(state, &token, "/Items/1").await;
    let tags = v["Tags"].as_array().unwrap();
    // Name-ordered flat string list.
    let names: Vec<&str> = tags.iter().map(|t| t.as_str().unwrap()).collect();
    assert_eq!(names, vec!["1080p", "cyberpunk"]);
}

#[actix_web::test]
async fn manual_add_and_remove_tags_persist() {
    let (state, token) = seed().await;
    let app = test::init_service(build_app(state.clone())).await;

    // Add a new tag to item 2 (incremental — keeps 1080p).
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/Items/2/Tags?Tags=director-cut,1080p")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 204);

    // Item 2 now carries both 1080p (kept) + director-cut (added).
    let v = get_json(state.clone(), &token, "/Items/2").await;
    let names: Vec<&str> = v["Tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["1080p", "director-cut"]);

    // ParentId = director-cut → item 2 resolves via the new link.
    let dc = tag_id_for("director-cut");
    let v = get_json(state.clone(), &token, &format!("/Items?ParentId={dc}")).await;
    let titles: Vec<&str> = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap())
        .collect();
    assert_eq!(titles, vec!["B"]);

    // Remove 1080p from item 2 — director-cut stays.
    let resp = test::call_service(
        &app,
        test::TestRequest::delete()
            .uri("/Items/2/Tags?Tags=1080p")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 204);
    let v = get_json(state.clone(), &token, "/Items/2").await;
    let names: Vec<&str> = v["Tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["director-cut"], "1080p removed, rest intact");

    // Mutating a non-existent item → 404.
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/Items/9999/Tags?Tags=x")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 404);
}

#[actix_web::test]
async fn wire_id_matches_core_helper() {
    // The DTO helper, the store wire_id, and the ParentId pivot all agree.
    assert_eq!(tag_id_for("cyberpunk"), tag_wire_id("cyberpunk"));
}
