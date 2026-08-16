#![allow(clippy::unwrap_used, clippy::expect_used)]
//! /Users CRUD + /Library/Refresh + dashboard stubs (T46).

use actix_web::{test, web, App};
use pharos_core::{SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

async fn seed(admin_flag: bool) -> (web::Data<AppState>, String, UserId) {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("p")).unwrap();
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "boss".into(),
            password_hash: hash,
            policy: UserPolicy {
                admin: admin_flag,
                ..Default::default()
            },
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

// B96 — the jellyfin-web dashboard user page GETs `/Users/{id}` for the user
// being administered (not the signed-in admin). That must return the target
// user's record; confining every caller to their own record 403'd it and the
// page spun forever (no `.catch` in jellyfin-web).
#[actix_web::test]
async fn admin_can_get_another_users_profile() {
    let (state, token, _uid) = seed(true).await;
    let app = test::init_service(build_app(state)).await;
    let create = test::TestRequest::post()
        .uri("/Users/New")
        .insert_header(("X-Emby-Token", token.as_str()))
        .set_json(serde_json::json!({"Name":"matt","Password":"p"}))
        .to_request();
    let body = test::call_and_read_body(&app, create).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let matt_id = v["Id"].as_str().unwrap().to_string();

    let req = test::TestRequest::get()
        .uri(&format!("/Users/{matt_id}"))
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200, "admin must read any user's profile");
    let body = test::read_body(resp).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["Name"], "matt");
    assert_eq!(v["Id"], matt_id);
}

// A non-admin caller stays scoped to itself: GETting another user's profile is
// still 403 (self-GET is covered by the client-compat flow).
#[actix_web::test]
async fn non_admin_cannot_get_another_users_profile() {
    let (state, admin_token, _uid) = seed(true).await;
    let app = test::init_service(build_app(state)).await;
    // Admin creates matt, then we authenticate as matt to get a non-admin token.
    let create = test::TestRequest::post()
        .uri("/Users/New")
        .insert_header(("X-Emby-Token", admin_token.as_str()))
        .set_json(serde_json::json!({"Name":"matt","Password":"p"}))
        .to_request();
    let body = test::call_and_read_body(&app, create).await;
    let matt: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let matt_id = matt["Id"].as_str().unwrap().to_string();

    let auth = test::TestRequest::post()
        .uri("/Users/AuthenticateByName")
        .set_json(serde_json::json!({"Username":"matt","Pw":"p"}))
        .to_request();
    let body = test::call_and_read_body(&app, auth).await;
    let session: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let matt_token = session["AccessToken"].as_str().unwrap().to_string();

    // matt reading the admin ("boss") — a different user — must be forbidden.
    let req = test::TestRequest::get()
        .uri("/Users/00000000000000000000000000000001")
        .insert_header(("X-Emby-Token", matt_token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 403);

    // But matt reading matt is fine.
    let req = test::TestRequest::get()
        .uri(&format!("/Users/{matt_id}"))
        .insert_header(("X-Emby-Token", matt_token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
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
    assert!(matches!(msg, SocketBroadcast::LibraryChanged { .. }));
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
async fn plugins_and_logs_return_empty_arrays() {
    let (state, token, _uid) = seed(true).await;
    let app = test::init_service(build_app(state)).await;
    for path in ["/Plugins", "/System/Logs"] {
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
async fn scheduled_tasks_advertise_builtin_jobs() {
    // T74 — /ScheduledTasks lists pharos's real background jobs as TaskInfo
    // descriptors (the dashboard panel 404-guards on an empty list).
    let (state, token, _uid) = seed(true).await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/ScheduledTasks")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let tasks = v.as_array().unwrap();
    assert!(!tasks.is_empty(), "must advertise the built-in jobs");
    for t in tasks {
        for field in ["Name", "State", "Id", "Key", "Description", "Category"] {
            assert!(t.get(field).is_some(), "TaskInfo missing {field}: {t}");
        }
        assert_eq!(t["State"], "Idle");
    }
}

#[actix_web::test]
async fn refresh_item_with_library_id_triggers_scan() {
    // A per-library "Scan Library" posts POST /Items/{libraryId}/Refresh with a
    // synth library wire id. Before the fix this 400'd (parse_item_id → None) and
    // no scan ran. Now it resolves the library root, force-scans it, and streams
    // RefreshProgress keyed by the library id.
    use pharos_core::{Library, LibraryKind};
    use pharos_server::state::SocketBroadcast;
    let dir = tempfile::tempdir().unwrap();
    let wire = "0123456789abcdef0123456789abcdef";
    let (state, token, _uid) = seed(true).await;
    state.set_libraries(vec![Library {
        id: 1,
        name: "Films".into(),
        root_path: dir.path().to_string_lossy().into_owned(),
        kind: LibraryKind::Movies,
        wire_id: wire.into(),
    }]);
    let mut bus = state.bus.subscribe();
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::post()
        .uri(&format!(
            "/Items/{wire}/Refresh?Recursive=true&MetadataRefreshMode=Default"
        ))
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 204, "library refresh must trigger, not 400");
    // The tracked scan streams RefreshProgress for the library id.
    let mut saw_refresh_progress = false;
    for _ in 0..8 {
        match tokio::time::timeout(std::time::Duration::from_secs(2), bus.recv()).await {
            Ok(Ok(SocketBroadcast::RefreshProgress { item_id, .. })) => {
                assert_eq!(item_id, wire);
                saw_refresh_progress = true;
                break;
            }
            Ok(Ok(_)) => continue,
            _ => break,
        }
    }
    assert!(
        saw_refresh_progress,
        "a RefreshProgress broadcast must fan out"
    );
}

#[actix_web::test]
async fn refresh_item_unknown_id_still_400() {
    // A non-library, non-media id is still a client error (regression guard: the
    // library branch must not swallow genuinely-invalid ids).
    let (state, token, _uid) = seed(true).await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::post()
        .uri("/Items/not-a-real-id/Refresh")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 400);
}

#[actix_web::test]
async fn scheduled_task_start_refresh_library_runs_and_stops() {
    // "Scan All Libraries" in jellyfin-web starts the RefreshLibrary task via
    // POST /ScheduledTasks/Running/{id}; DELETE cancels; an unknown id is 404.
    use pharos_server::state::SocketBroadcast;
    let (state, token, _uid) = seed(true).await;
    let mut bus = state.bus.subscribe();
    let app = test::init_service(build_app(state)).await;

    let start = test::TestRequest::post()
        .uri("/ScheduledTasks/Running/refresh-library")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    assert_eq!(test::call_service(&app, start).await.status(), 204);
    // The scan (even over zero roots) completes with a LibraryChanged broadcast.
    let msg = tokio::time::timeout(std::time::Duration::from_secs(2), bus.recv())
        .await
        .expect("broadcast timeout")
        .expect("recv");
    assert!(matches!(msg, SocketBroadcast::LibraryChanged { .. }));

    // DELETE is accepted (204) whether or not a run is live.
    let stop = test::TestRequest::delete()
        .uri("/ScheduledTasks/Running/refresh-library")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    assert_eq!(test::call_service(&app, stop).await.status(), 204);

    // Unknown task id → 404.
    let bogus = test::TestRequest::post()
        .uri("/ScheduledTasks/Running/no-such-task")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    assert_eq!(test::call_service(&app, bogus).await.status(), 404);
}

#[actix_web::test]
async fn scheduled_task_start_requires_admin() {
    let (state, token, _uid) = seed(false).await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::post()
        .uri("/ScheduledTasks/Running/refresh-library")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 403);
}

#[actix_web::test]
async fn system_logs_lists_files_in_log_dir() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("pharos.log"), b"hello\n").unwrap();
    std::fs::write(tmp.path().join("scan.log"), b"line\n").unwrap();
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let auth = pharos_server::auth::BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("p")).unwrap();
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "boss".into(),
            password_hash: hash,
            policy: UserPolicy {
                admin: true,
                ..Default::default()
            },
        })
        .await
        .unwrap();
    let token = stores.issue(uid, "test").await.unwrap();
    let token = token.0.expose().to_string();
    let state = pharos_server::state::AppState::new(stores, "t".into())
        .with_log_dir(Some(tmp.path().into()));
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
        .set_payload(r#"{"ServerName":"My Pharos","LoginDisclaimer":"Hello","CustomCss":"body{}"}"#)
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

#[actix_web::test]
async fn dashboard_named_config_and_backup_sections_render() {
    // The dashboard's Networking / Live TV / Backups pages fetch these; a 404
    // made the section throw instead of rendering its (default) form.
    let (state, token, _) = seed(true).await;
    let app = test::init_service(build_app(state)).await;

    for (uri, key_field) in [
        ("/System/Configuration/network", "InternalHttpPort"),
        ("/System/Configuration/livetv", "TunerHosts"),
    ] {
        let req = test::TestRequest::get()
            .uri(uri)
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200, "{uri} should 200");
        let body = test::call_and_read_body(
            &app,
            test::TestRequest::get()
                .uri(uri)
                .insert_header(("X-Emby-Token", token.as_str()))
                .to_request(),
        )
        .await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v.get(key_field).is_some(), "{uri} missing {key_field}: {v}");
    }

    // /Backup lists backups; empty array renders the "no backups" state.
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Backup")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v.is_array(), "/Backup must be an array, got {v}");

    // Dashboard "Paths" panel reads seven fixed named folder slots (NOT a
    // `Folders` array); a missing slot renders a permanent spinner. Assert the
    // slots are present with real byte figures the widget draws its bar from.
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/System/Info/Storage")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    for slot in [
        "CacheFolder",
        "ImageCacheFolder",
        "ProgramDataFolder",
        "LogFolder",
        "InternalMetadataFolder",
        "TranscodingTempFolder",
        "WebFolder",
    ] {
        assert!(
            v[slot].is_object(),
            "/System/Info/Storage needs {slot} object: {v}"
        );
        assert!(
            v[slot]["UsedSpace"].is_number() && v[slot]["FreeSpace"].is_number(),
            "{slot} needs numeric UsedSpace/FreeSpace: {v}"
        );
    }
}

/// T120 — the ROUTE must purge, not just the cache helper.
///
/// `hls_cache`'s own tests prove the quarantine moves bytes and respects a
/// range; they stay green if this endpoint is never registered, and the
/// endpoint is the entire point. Removing one bad segment on 2026-08-16 meant
/// running a busybox pod against the cache PVC and `rm`-ing by hand, three
/// times in one evening.
#[actix_web::test]
async fn an_admin_can_purge_a_time_range_of_an_items_segments() {
    use pharos_core::{MediaItem, MediaKind, MediaProbe, MediaStore};

    let (state, token, _uid) = seed(true).await;
    let dir = tempfile::tempdir().unwrap();
    let cache = pharos_cache::hls_cache::HlsSegmentCache::new(dir.path().to_path_buf(), 1 << 30);

    // 60 s at the 6 s grid: second 30 lands in segment 5.
    pharos_core::MediaStore::put(
        &state.stores,
        MediaItem {
            id: 42,
            path: "/nonexistent/film.mkv".into(),
            title: "film".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(60_000),
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let _ = MediaStore::get(&state.stores, 42).await;

    // A cached segment sitting exactly where the purge is aimed.
    let seg_dir = cache.root().join("42");
    std::fs::create_dir_all(&seg_dir).unwrap();
    let seg = seg_dir.join("5-a0-s0-v2971-bauto-c26.ts");
    std::fs::write(&seg, b"poisoned bytes").unwrap();

    let state = web::Data::new(
        AppState::new(state.stores.clone(), "t".into()).with_hls_cache(cache.clone()),
    );
    let app = test::init_service(build_app(state)).await;

    let req = test::TestRequest::delete()
        .uri("/Admin/Cache/Segments?itemId=42&from=30")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(
        resp.status().is_success(),
        "the purge route must exist and answer: {}",
        resp.status()
    );

    assert!(
        !seg.exists(),
        "the segment the caller named must stop being servable"
    );
    // Quarantined, not destroyed (T118) — the reason the endpoint exists at all
    // is that three by-hand purges left no artefact to examine.
    let pen = cache.root().join(".quarantine");
    let kept: Vec<_> = std::fs::read_dir(&pen)
        .expect("the quarantine exists")
        .flatten()
        .map(|e| std::fs::read(e.path()).unwrap_or_default())
        .collect();
    assert!(
        kept.iter().any(|b| b == b"poisoned bytes"),
        "the purged bytes must be retained for diagnosis"
    );
}

/// A purge is a denial-of-service primitive against the server's own cache:
/// everything it removes has to be re-encoded on demand.
#[actix_web::test]
async fn a_non_admin_cannot_purge_segments() {
    let (state, token, _uid) = seed(false).await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::delete()
        .uri("/Admin/Cache/Segments?itemId=42&from=30")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status().as_u16(),
        403,
        "a non-admin must be refused before anything is touched"
    );
}

/// T123 — the retained bytes must be readable over the API.
///
/// The purge keeps evidence (T118), and on 2026-08-16 the only way to look at
/// it was a busybox pod mounting the cache PVC — twice in one night, once for
/// the segments and once for their provenance. That is the cluster surgery T120
/// existed to remove, still being performed for the half of the job done under
/// time pressure.
#[actix_web::test]
async fn an_admin_can_list_and_fetch_quarantined_artefacts() {
    let (state, token, _uid) = seed(true).await;
    let dir = tempfile::tempdir().unwrap();
    let cache = pharos_cache::hls_cache::HlsSegmentCache::new(dir.path().to_path_buf(), 1 << 30);

    let pen = cache.root().join(".quarantine");
    std::fs::create_dir_all(&pen).unwrap();
    std::fs::write(
        pen.join("42-896-a0-s0-v2971-bauto-c26.ts"),
        b"suspect bytes",
    )
    .unwrap();

    let state = web::Data::new(
        AppState::new(state.stores.clone(), "t".into()).with_hls_cache(cache.clone()),
    );
    let app = test::init_service(build_app(state)).await;

    let raw = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Admin/Cache/Quarantine")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(
        v[0]["Name"], "42-896-a0-s0-v2971-bauto-c26.ts",
        "the listing must name what is held: {v}"
    );
    assert_eq!(v[0]["SizeBytes"], 13, "{v}");

    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Admin/Cache/Quarantine/42-896-a0-s0-v2971-bauto-c26.ts")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    assert_eq!(
        &body[..],
        b"suspect bytes",
        "the artefact must come back byte-for-byte, or it cannot be probed"
    );
}

/// This route is reachable over HTTP and the cache root sits beside the media
/// library, so a name with a path component must be refused rather than
/// normalised.
#[actix_web::test]
async fn a_quarantine_fetch_cannot_escape_its_directory() {
    let (state, token, _uid) = seed(true).await;
    let dir = tempfile::tempdir().unwrap();
    let cache = pharos_cache::hls_cache::HlsSegmentCache::new(dir.path().to_path_buf(), 1 << 30);
    // A secret sitting one level up from the quarantine, as the rate store and
    // the probe-failure memo really do.
    std::fs::create_dir_all(cache.root()).unwrap();
    std::fs::write(cache.root().join("secret.json"), b"not yours").unwrap();

    let state = web::Data::new(
        AppState::new(state.stores.clone(), "t".into()).with_hls_cache(cache.clone()),
    );
    let app = test::init_service(build_app(state)).await;

    for attempt in ["..%2Fsecret.json", "..", "%2Fetc%2Fpasswd"] {
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/Admin/Cache/Quarantine/{attempt}"))
                .insert_header(("X-Emby-Token", token.as_str()))
                .to_request(),
        )
        .await;
        assert!(
            resp.status().is_client_error(),
            "traversal attempt {attempt:?} must be refused, got {}",
            resp.status()
        );
    }
}

/// The quarantine holds whatever was serving when something went wrong, so
/// reading it is admin-only like the purge that created it.
#[actix_web::test]
async fn a_non_admin_cannot_read_the_quarantine() {
    let (state, token, _uid) = seed(false).await;
    let app = test::init_service(build_app(state)).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/Admin/Cache/Quarantine")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 403);
}
