#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaStore, SecretString, TokenStore, UserId, UserPolicy, UserRecord,
    UserStore,
};
use pharos_server::{
    api::jellyfin, auth::BuiltinAuth, state::AppState,
};
use pharos_store_sqlx::sqlite::SqliteStore;

async fn seed() -> (web::Data<AppState>, String, UserId) {
    let stores = SqliteStore::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("hunter2")).unwrap();
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "ali".into(),
            password_hash: hash,
            policy: UserPolicy { admin: true },
        })
        .await
        .unwrap();
    let token = stores.issue(uid, "test").await.unwrap();

    for (i, k) in [
        MediaKind::Movie,
        MediaKind::Audio,
        MediaKind::Episode,
        MediaKind::Movie,
    ]
    .iter()
    .enumerate()
    {
        stores
            .put(MediaItem {
                id: (100 + i) as u64,
                path: format!("/m/{i}.x").into(),
                title: format!("title-{i}"),
                kind: *k,
            })
            .await
            .unwrap();
    }
    let state = web::Data::new(AppState::new(stores, "test".into()));
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
    App::new().app_data(state).configure(jellyfin::configure)
}

#[actix_web::test]
async fn list_items_requires_auth() {
    let (state, _t, _u) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get().uri("/Items").to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401);
}

#[actix_web::test]
async fn list_items_returns_all_with_total_count() {
    let (state, token, _u) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Items")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"], 4);
    assert_eq!(v["StartIndex"], 0);
    assert_eq!(v["Items"].as_array().unwrap().len(), 4);
    let first = &v["Items"][0];
    assert!(first.get("Id").is_some());
    assert!(first.get("Name").is_some());
    assert!(first.get("Type").is_some());
    assert!(first.get("ServerId").is_some());
}

#[actix_web::test]
async fn list_items_pagination() {
    let (state, token, _u) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Items?StartIndex=1&Limit=2")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"], 4);
    assert_eq!(v["StartIndex"], 1);
    assert_eq!(v["Items"].as_array().unwrap().len(), 2);
}

#[actix_web::test]
async fn get_item_by_id_returns_pascalcase_dto() {
    let (state, token, _u) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Items/100")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["Id"], "100");
    assert_eq!(v["Name"], "title-0");
    assert_eq!(v["Type"], "Movie");
}

#[actix_web::test]
async fn get_item_unknown_id_is_404() {
    let (state, token, _u) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Items/9999")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 404);
}

#[actix_web::test]
async fn list_user_items_rejects_mismatched_user() {
    let (state, token, _u) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Users/deadbeefdeadbeefdeadbeefdeadbeef/Items")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 403);
}

#[actix_web::test]
async fn list_user_items_accepts_matching_user() {
    let (state, token, uid) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri(&format!("/Users/{}/Items", uid.0.simple()))
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());
}

#[actix_web::test]
async fn get_user_item_matches_bearer_returns_dto() {
    let (state, token, uid) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri(&format!("/Users/{}/Items/100", uid.0.simple()))
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["Id"], "100");
    assert_eq!(v["Name"], "title-0");
}

#[actix_web::test]
async fn get_user_item_rejects_other_user_id_in_path() {
    let (state, token, _uid) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Users/deadbeefdeadbeefdeadbeefdeadbeef/Items/100")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 403);
}

#[actix_web::test]
async fn list_items_filters_by_search_term() {
    let (state, token, _uid) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Items?SearchTerm=title-2")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"], 1);
    assert_eq!(v["Items"][0]["Name"], "title-2");
}

#[actix_web::test]
async fn list_items_filters_by_include_item_types() {
    let (state, token, _uid) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Items?IncludeItemTypes=Audio")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"], 1);
    assert_eq!(v["Items"][0]["Type"], "Audio");
}

#[actix_web::test]
async fn list_items_filters_with_two_types() {
    let (state, token, _uid) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Items?IncludeItemTypes=Movie,Episode")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    // 2 movies + 1 episode in the seed.
    assert_eq!(v["TotalRecordCount"], 3);
}

#[actix_web::test]
async fn list_items_sorts_descending_when_requested() {
    let (state, token, _uid) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Items?SortOrder=Descending")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let arr = v["Items"].as_array().unwrap();
    let names: Vec<String> = arr
        .iter()
        .map(|i| i["Name"].as_str().unwrap().to_string())
        .collect();
    // Default is title- prefix; descending lexicographic puts -3 first.
    assert_eq!(names.first().map(|s| s.as_str()), Some("title-3"));
}

#[actix_web::test]
async fn virtual_folders_returns_synth_library() {
    let (state, token, _u) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Library/VirtualFolders")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["Name"], "All Media");
}
