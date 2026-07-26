#![allow(clippy::unwrap_used, clippy::expect_used)]
//! End-to-end Jellyfin-compat smoke: /System/Info, /Users/AuthenticateByName,
//! /Users/Me. Validates V1 (clients work unmodified) at the smallest scale
//! that is meaningful: shape of the JSON and the auth flow.

use actix_web::{test, web, App};
use pharos_core::{SecretString, UserId, UserPolicy, UserRecord, UserStore};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    router,
    state::{AppState, Stores},
};

async fn seed_state() -> web::Data<AppState> {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("hunter2")).unwrap();
    stores
        .create(UserRecord {
            id: UserId::new(),
            name: "ali".into(),
            password_hash: hash,
            policy: UserPolicy {
                admin: true,
                ..Default::default()
            },
        })
        .await
        .unwrap();
    web::Data::new(AppState::new(stores, "pharos-test".into()))
}

#[actix_web::test]
async fn system_info_returns_pascalcase_shape() {
    let state = seed_state().await;
    let app = test::init_service(
        App::new()
            .app_data(state.clone())
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    // The anonymous variant — full /System/Info now requires auth (matches
    // real Jellyfin); the public subset carries the same PascalCase core.
    let req = test::TestRequest::get()
        .uri("/System/Info/Public")
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let txt = std::str::from_utf8(&body).unwrap();
    assert!(txt.contains("\"ServerName\":\"pharos-test\""), "{txt}");
    assert!(txt.contains("\"ProductName\":\"Jellyfin Server\""), "{txt}");
    assert!(txt.contains("\"Version\""), "{txt}");
    assert!(txt.contains("\"Id\""), "{txt}");
}

#[actix_web::test]
async fn system_info_public_alias_works() {
    let state = seed_state().await;
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    let req = test::TestRequest::get()
        .uri("/System/Info/Public")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());
}

#[actix_web::test]
async fn authenticate_by_name_returns_token_and_user() {
    let state = seed_state().await;
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    let req = test::TestRequest::post()
        .uri("/Users/AuthenticateByName")
        .insert_header((
            "X-Emby-Authorization",
            r#"MediaBrowser Client="rust-test", Device="cli", DeviceId="dev-1", Version="0""#,
        ))
        .set_json(serde_json::json!({"Username":"ali","Pw":"hunter2"}))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let txt = std::str::from_utf8(&body).unwrap();
    assert!(txt.contains("\"AccessToken\""), "{txt}");
    assert!(txt.contains("\"User\""), "{txt}");
    assert!(txt.contains("\"ServerId\""), "{txt}");
    assert!(txt.contains("\"DeviceId\":\"dev-1\""), "{txt}");
}

#[actix_web::test]
async fn authenticate_with_wrong_password_is_401() {
    let state = seed_state().await;
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    let req = test::TestRequest::post()
        .uri("/Users/AuthenticateByName")
        .set_json(serde_json::json!({"Username":"ali","Pw":"wrong"}))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401);
}

#[actix_web::test]
async fn me_without_token_is_401() {
    let state = seed_state().await;
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    let req = test::TestRequest::get().uri("/Users/Me").to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401);
}

#[actix_web::test]
async fn full_login_then_me_with_token_returns_user() {
    let state = seed_state().await;
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    // Login.
    let login = test::TestRequest::post()
        .uri("/Users/AuthenticateByName")
        .set_json(serde_json::json!({"Username":"ali","Pw":"hunter2"}))
        .to_request();
    let body = test::call_and_read_body(&app, login).await;
    let parsed: serde_json::Value =
        serde_json::from_slice(&body).expect("login body is valid JSON");
    let token = parsed["AccessToken"].as_str().unwrap().to_string();

    // /Users/Me with X-Emby-Token.
    let me = test::TestRequest::get()
        .uri("/Users/Me")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, me).await;
    assert!(resp.status().is_success());
    let body = test::read_body(resp).await;
    let txt = std::str::from_utf8(&body).unwrap();
    assert!(txt.contains("\"Name\":\"ali\""), "{txt}");
    assert!(txt.contains("\"IsAdministrator\":true"), "{txt}");
}

#[actix_web::test]
async fn router_mounts_jellyfin_scope_alongside_metrics_and_health() {
    // Sanity: the master router boots and serves Jellyfin endpoints next to
    // /metrics, /healthz, etc.
    let _ = pharos_server::obs::init("info", None);
    let state = seed_state().await;
    let readiness = pharos_server::health::ReadinessHandle::spawn(&["process"]);
    readiness.mark("process").await.unwrap();
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(readiness))
            .app_data(state)
            .wrap(LowercasePath)
            .configure(router::configure),
    )
    .await;
    for path in ["/", "/metrics", "/healthz", "/info", "/System/Info/Public"] {
        let req = test::TestRequest::get().uri(path).to_request();
        let resp = test::call_service(&app, req).await;
        assert!(
            resp.status().is_success(),
            "{path} returned {}",
            resp.status()
        );
    }
}

#[actix_web::test]
async fn user_configuration_persists_across_request() {
    use pharos_core::{SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore};
    use pharos_server::{
        auth::BuiltinAuth,
        middleware::LowercasePath,
        state::{AppState, Stores},
    };
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
    let state = actix_web::web::Data::new(AppState::new(stores, "t".into()));
    let app = actix_web::test::init_service(
        actix_web::App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(pharos_server::api::jellyfin::configure),
    )
    .await;

    // POST a non-default config.
    let req = actix_web::test::TestRequest::post()
        .uri(&format!("/Users/{}/Configuration", uid.0.simple()))
        .insert_header(("X-Emby-Token", token.0.expose()))
        .insert_header(("content-type", "application/json"))
        .set_payload(r#"{"AudioLanguagePreference":"de","SubtitleMode":"Always"}"#)
        .to_request();
    let resp = actix_web::test::call_service(&app, req).await;
    assert_eq!(resp.status(), 204);

    // GET /Users/Me echoes it back via UserDto.Configuration.
    let req = actix_web::test::TestRequest::get()
        .uri("/Users/Me")
        .insert_header(("X-Emby-Token", token.0.expose()))
        .to_request();
    let body = actix_web::test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["Configuration"]["AudioLanguagePreference"], "de");
    assert_eq!(v["Configuration"]["SubtitleMode"], "Always");
}

/// An ADMIN may set another account's preferences — jellyfin-web's
/// "Edit user -> Playback" page posts here for the account being edited, and
/// an operator setting a household member up should not have to log in as
/// them. A NON-admin still may not touch anyone else's.
#[actix_web::test]
async fn admin_may_write_another_users_configuration_but_a_peer_may_not() {
    use pharos_core::{SecretString, TokenStore, UserPolicy, UserRecord, UserStore};
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("p")).unwrap();

    let admin_id = UserId::new();
    stores
        .create(UserRecord {
            id: admin_id,
            name: "admin".into(),
            password_hash: hash.clone(),
            policy: UserPolicy {
                admin: true,
                ..Default::default()
            },
        })
        .await
        .unwrap();
    let target_id = UserId::new();
    stores
        .create(UserRecord {
            id: target_id,
            name: "target".into(),
            password_hash: hash.clone(),
            policy: UserPolicy::default(),
        })
        .await
        .unwrap();
    let peer_id = UserId::new();
    stores
        .create(UserRecord {
            id: peer_id,
            name: "peer".into(),
            password_hash: hash,
            policy: UserPolicy::default(),
        })
        .await
        .unwrap();

    let admin_token = stores.issue(admin_id, "t").await.unwrap();
    let peer_token = stores.issue(peer_id, "t").await.unwrap();
    let state = actix_web::web::Data::new(AppState::new(stores.clone(), "t".into()));
    let app = actix_web::test::init_service(
        actix_web::App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(pharos_server::api::jellyfin::configure),
    )
    .await;

    let post = |token: String, target: UserId| {
        actix_web::test::TestRequest::post()
            .uri(&format!("/Users/{}/Configuration", target.0.simple()))
            .insert_header(("X-Emby-Token", token))
            .insert_header(("content-type", "application/json"))
            .set_payload(r#"{"AudioLanguagePreference":"OriginalLanguage","SubtitleMode":"Smart"}"#)
            .to_request()
    };

    let resp =
        actix_web::test::call_service(&app, post(admin_token.0.expose().to_string(), target_id))
            .await;
    assert_eq!(resp.status(), 204, "an admin may configure another account");

    let resp =
        actix_web::test::call_service(&app, post(peer_token.0.expose().to_string(), target_id))
            .await;
    assert_eq!(
        resp.status(),
        403,
        "a non-admin must not reach another account"
    );

    // The admin's write landed on the TARGET, not on the admin.
    use pharos_core::PreferenceStore;
    let stored = stores
        .get_user_configuration(target_id)
        .await
        .unwrap()
        .expect("target has a configuration");
    assert!(stored.contains("OriginalLanguage"));
    assert_eq!(stores.get_user_configuration(admin_id).await.unwrap(), None);
}

#[actix_web::test]
async fn display_preferences_round_trip_per_user() {
    use pharos_core::{SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore};
    use pharos_server::{
        auth::BuiltinAuth,
        middleware::LowercasePath,
        state::{AppState, Stores},
    };
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
    let state = actix_web::web::Data::new(AppState::new(stores, "t".into()));
    let app = actix_web::test::init_service(
        actix_web::App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(pharos_server::api::jellyfin::configure),
    )
    .await;

    // POST a prefs payload.
    let req = actix_web::test::TestRequest::post()
        .uri("/DisplayPreferences/home?client=emby")
        .insert_header(("X-Emby-Token", token.0.expose()))
        .insert_header(("content-type", "application/json"))
        .set_payload(r#"{"ViewType":"poster","SortBy":"DateAdded"}"#)
        .to_request();
    let resp = actix_web::test::call_service(&app, req).await;
    assert_eq!(resp.status(), 204);

    // GET returns the stored payload, not the default-stub.
    let req = actix_web::test::TestRequest::get()
        .uri("/DisplayPreferences/home?client=emby")
        .insert_header(("X-Emby-Token", token.0.expose()))
        .to_request();
    let body = actix_web::test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["ViewType"], "poster");
    assert_eq!(v["SortBy"], "DateAdded");
}
