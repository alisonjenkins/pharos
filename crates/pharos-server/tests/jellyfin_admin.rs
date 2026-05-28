#![allow(clippy::unwrap_used, clippy::expect_used)]
//! /Users CRUD + /Library/Refresh + dashboard stubs (T46).

use actix_web::{test, web, App};
use pharos_core::{SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore};
use pharos_server::{api::jellyfin, auth::BuiltinAuth, middleware::LowercasePath, state::AppState};
use pharos_store_sqlx::sqlite::SqliteStore;

async fn seed(admin_flag: bool) -> (web::Data<AppState>, String, UserId) {
    let stores = SqliteStore::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("p")).unwrap();
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "boss".into(),
            password_hash: hash,
            policy: UserPolicy { admin: admin_flag },
        })
        .await
        .unwrap();
    let token = stores.issue(uid, "test").await.unwrap();
    let state = web::Data::new(AppState::new(stores, "t".into()));
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
async fn list_users_admin_returns_users_array() {
    let (state, token, _uid) = seed(true).await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Users")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["Name"], "boss");
    assert_eq!(arr[0]["Policy"]["IsAdministrator"], true);
}

#[actix_web::test]
async fn list_users_non_admin_rejected_403() {
    let (state, token, _uid) = seed(false).await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Users")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 403);
}

#[actix_web::test]
async fn create_user_then_list_returns_new_user() {
    let (state, token, _uid) = seed(true).await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::post()
        .uri("/Users/New")
        .insert_header(("X-Emby-Token", token.as_str()))
        .set_json(serde_json::json!({"Name":"alice","Password":"p"}))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["Name"], "alice");

    let req = test::TestRequest::get()
        .uri("/Users")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 2);
}

#[actix_web::test]
async fn create_duplicate_user_409() {
    let (state, token, _uid) = seed(true).await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::post()
        .uri("/Users/New")
        .insert_header(("X-Emby-Token", token.as_str()))
        .set_json(serde_json::json!({"Name":"boss","Password":"p"}))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 409);
}

#[actix_web::test]
async fn cannot_delete_self() {
    let (state, token, uid) = seed(true).await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::delete()
        .uri(&format!("/Users/{}", uid.0.simple()))
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 400);
}

#[actix_web::test]
async fn delete_other_user_succeeds() {
    let (state, token, _uid) = seed(true).await;
    let app = test::init_service(build_app(state)).await;
    // Create a second user.
    let create = test::TestRequest::post()
        .uri("/Users/New")
        .insert_header(("X-Emby-Token", token.as_str()))
        .set_json(serde_json::json!({"Name":"alice","Password":"p"}))
        .to_request();
    let body = test::call_and_read_body(&app, create).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let alice_id = v["Id"].as_str().unwrap().to_string();
    let req = test::TestRequest::delete()
        .uri(&format!("/Users/{alice_id}"))
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 204);
    // List now has 1 user (boss).
    let req = test::TestRequest::get()
        .uri("/Users")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 1);
}

#[actix_web::test]
async fn set_user_policy_flips_admin_bit() {
    let (state, token, _uid) = seed(true).await;
    let app = test::init_service(build_app(state)).await;
    // Create alice (non-admin).
    let create = test::TestRequest::post()
        .uri("/Users/New")
        .insert_header(("X-Emby-Token", token.as_str()))
        .set_json(serde_json::json!({"Name":"alice","Password":"p"}))
        .to_request();
    let body = test::call_and_read_body(&app, create).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let alice_id = v["Id"].as_str().unwrap().to_string();
    // Promote.
    let req = test::TestRequest::post()
        .uri(&format!("/Users/{alice_id}/Policy"))
        .insert_header(("X-Emby-Token", token.as_str()))
        .set_json(serde_json::json!({"IsAdministrator":true}))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 204);
}

#[actix_web::test]
async fn library_refresh_admin_only_and_broadcasts() {
    use pharos_server::state::SocketBroadcast;
    let (state, token, _uid) = seed(true).await;
    let mut bus = state.bus.subscribe();
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::post()
        .uri("/Library/Refresh")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 204);
    let msg = tokio::time::timeout(std::time::Duration::from_millis(500), bus.recv())
        .await
        .expect("broadcast timeout")
        .expect("recv");
    assert!(matches!(msg, SocketBroadcast::LibraryChanged));
}

#[actix_web::test]
async fn library_refresh_non_admin_403() {
    let (state, token, _uid) = seed(false).await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::post()
        .uri("/Library/Refresh")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 403);
}

#[actix_web::test]
async fn scheduled_tasks_and_plugins_return_empty_arrays() {
    let (state, token, _uid) = seed(true).await;
    let app = test::init_service(build_app(state)).await;
    for path in ["/ScheduledTasks", "/Plugins", "/System/Logs"] {
        let req = test::TestRequest::get()
            .uri(path)
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request();
        let body = test::call_and_read_body(&app, req).await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v.as_array().unwrap().is_empty(), "{path}");
    }
}

#[actix_web::test]
async fn system_logs_lists_files_in_log_dir() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("pharos.log"), b"hello\n").unwrap();
    std::fs::write(tmp.path().join("scan.log"), b"line\n").unwrap();
    let stores = pharos_store_sqlx::sqlite::SqliteStore::connect("sqlite::memory:")
        .await
        .unwrap();
    let auth = pharos_server::auth::BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("p")).unwrap();
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "boss".into(),
            password_hash: hash,
            policy: UserPolicy { admin: true },
        })
        .await
        .unwrap();
    let token = stores.issue(uid, "test").await.unwrap();
    let token = token.0.expose().to_string();
    let state =
        pharos_server::state::AppState::new(stores, "t".into()).with_log_dir(Some(tmp.path().into()));
    let state = web::Data::new(state);
    let app = test::init_service(build_app(state)).await;

    let req = test::TestRequest::get()
        .uri("/System/Logs")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    let names: Vec<String> = arr
        .iter()
        .map(|e| e["Name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"pharos.log".to_string()));
    assert!(names.contains(&"scan.log".to_string()));
    for entry in arr {
        let size = entry["Size"].as_u64().unwrap();
        assert!(size > 0, "{entry}");
    }

    // Fetch a specific file.
    let req = test::TestRequest::get()
        .uri("/System/Logs/Log?Name=pharos.log")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    assert_eq!(body.as_ref(), b"hello\n");

    // Path traversal blocked.
    let req = test::TestRequest::get()
        .uri("/System/Logs/Log?Name=..%2Fetc%2Fpasswd")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_client_error(), "{}", resp.status());
}

#[actix_web::test]
async fn system_logs_returns_empty_when_log_dir_unset() {
    let (state, token, _uid) = seed(true).await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/System/Logs")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v.as_array().unwrap().is_empty());
}

#[actix_web::test]
async fn system_configuration_post_persists_server_name_into_info() {
    let (state, token, _uid) = seed(true).await;
    let app = test::init_service(build_app(state)).await;

    // Baseline: /System/Info reports the seed name.
    let req = test::TestRequest::get()
        .uri("/System/Info")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["ServerName"], "t");

    // POST /System/Configuration with new ServerName.
    let req = test::TestRequest::post()
        .uri("/System/Configuration")
        .insert_header(("X-Emby-Token", token.as_str()))
        .insert_header(("Content-Type", "application/json"))
        .set_payload(
            r#"{"ServerName":"My Pharos","LoginDisclaimer":"Hello","CustomCss":"body{}"}"#,
        )
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 204);

    // /System/Info reflects the new name.
    let req = test::TestRequest::get()
        .uri("/System/Info")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["ServerName"], "My Pharos");

    // /Branding/Configuration carries the disclaimer + css.
    let req = test::TestRequest::get()
        .uri("/Branding/Configuration")
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["LoginDisclaimer"], "Hello");
    assert_eq!(v["CustomCss"], "body{}");
}

#[actix_web::test]
async fn system_configuration_post_admin_only() {
    let (state, non_admin_token, _) = seed(false).await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::post()
        .uri("/System/Configuration")
        .insert_header(("X-Emby-Token", non_admin_token.as_str()))
        .insert_header(("Content-Type", "application/json"))
        .set_payload(r#"{"ServerName":"hax"}"#)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 403);
}

#[actix_web::test]
async fn api_key_create_lists_then_revoke_drops_it() {
    let (state, token, _uid) = seed(true).await;
    let app = test::init_service(build_app(state)).await;

    // List starts empty.
    let req = test::TestRequest::get()
        .uri("/Auth/Keys")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"], 0);

    // Create a new key.
    let req = test::TestRequest::post()
        .uri("/Auth/Keys?App=cli")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(created["AppName"], "cli");
    assert_eq!(created["Id"], "apikey:cli");
    let new_token = created["AccessToken"].as_str().unwrap().to_string();
    assert!(!new_token.is_empty());

    // List now reports it.
    let req = test::TestRequest::get()
        .uri("/Auth/Keys")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"], 1);
    assert_eq!(v["Items"][0]["AppName"], "cli");
    // Token string never surfaces via list.
    assert_eq!(v["Items"][0]["AccessToken"], "");

    // Revoke.
    let req = test::TestRequest::delete()
        .uri("/Auth/Keys/apikey%3Acli")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 204);

    // List empty again.
    let req = test::TestRequest::get()
        .uri("/Auth/Keys")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"], 0);

    // Revoking unknown id 404s.
    let req = test::TestRequest::delete()
        .uri("/Auth/Keys/apikey%3Anope")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 404);
}

#[actix_web::test]
async fn auth_providers_admin_only() {
    let (state, admin_token, _) = seed(true).await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Auth/Providers")
        .insert_header(("X-Emby-Token", admin_token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 1);
    assert_eq!(v[0]["Name"], "Default");

    let (state2, non_admin_token, _) = seed(false).await;
    let app2 = test::init_service(build_app(state2)).await;
    let req = test::TestRequest::get()
        .uri("/Auth/Providers")
        .insert_header(("X-Emby-Token", non_admin_token.as_str()))
        .to_request();
    let resp = test::call_service(&app2, req).await;
    assert_eq!(resp.status(), 403);
}

#[actix_web::test]
async fn api_key_create_requires_admin() {
    let (state, token, _uid) = seed(false).await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::post()
        .uri("/Auth/Keys?App=cli")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 403);
}

#[actix_web::test]
async fn api_key_create_rejects_empty_app() {
    let (state, token, _uid) = seed(true).await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::post()
        .uri("/Auth/Keys?App=")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 400);
}
