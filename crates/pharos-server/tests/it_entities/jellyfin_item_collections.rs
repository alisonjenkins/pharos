//! Jellyfin 12 `GET /Items/{itemId}/Collections` — the BoxSets that contain
//! one item, as a `BaseItemDtoQueryResult`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{http::StatusCode, test, web, App};
use pharos_core::{
    CollectionStore, MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore,
    UserId, UserPolicy, UserRecord, UserStore,
};
use pharos_jellyfin_api::dto::collection_id_for;
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
    // Item 1 is in two box sets, item 3 in one, item 2 in none.
    stores
        .link_item_collections(1, &["Trilogy".into(), "Duo".into()])
        .await
        .unwrap();
    stores
        .link_item_collections(3, &["Trilogy".into()])
        .await
        .unwrap();
    let token = stores.issue(uid, "t").await.unwrap();
    let state = web::Data::new(AppState::new(stores, "srv".into()));
    (state, token.0.expose().to_string())
}

async fn get(state: &web::Data<AppState>, token: &str, uri: &str) -> (StatusCode, Vec<u8>) {
    let app = test::init_service(
        App::new()
            .app_data(state.clone())
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(uri)
            .insert_header(("X-Emby-Token", token))
            .to_request(),
    )
    .await;
    let status = resp.status();
    (status, test::read_body(resp).await.to_vec())
}

#[actix_web::test]
async fn lists_every_boxset_containing_the_item() {
    let (state, token) = seed().await;
    let (status, body) = get(&state, &token, "/Items/1/Collections").await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let mut got: Vec<(String, String)> = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            assert_eq!(i["Type"].as_str().unwrap(), "BoxSet");
            (
                i["Name"].as_str().unwrap().to_string(),
                i["Id"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    got.sort();
    assert_eq!(
        got,
        vec![
            ("Duo".to_string(), collection_id_for("Duo")),
            ("Trilogy".to_string(), collection_id_for("Trilogy")),
        ]
    );
    assert_eq!(v["TotalRecordCount"].as_u64().unwrap(), 2);
    assert_eq!(v["StartIndex"].as_u64().unwrap(), 0);
}

#[actix_web::test]
async fn only_boxsets_containing_this_item_are_listed() {
    let (state, token) = seed().await;
    let (_, body) = get(&state, &token, "/Items/3/Collections?Fields=Overview").await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let names: Vec<&str> = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["Trilogy"]);
}

#[actix_web::test]
async fn item_in_no_collection_gets_an_empty_result_not_404() {
    let (state, token) = seed().await;
    let (status, body) = get(&state, &token, "/Items/2/Collections").await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["Items"].as_array().unwrap().len(), 0);
    assert_eq!(v["TotalRecordCount"].as_u64().unwrap(), 0);
    assert_eq!(v["StartIndex"].as_u64().unwrap(), 0);
}

#[actix_web::test]
async fn unknown_item_is_404_and_missing_token_is_401() {
    let (state, token) = seed().await;
    let (status, _) = get(&state, &token, "/Items/999/Collections").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get(&state, "", "/Items/1/Collections").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
