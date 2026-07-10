#![allow(clippy::unwrap_used, clippy::expect_used)]
//! T29 phase 1A — Jellyfin client-compat smoke.
//!
//! Spawns pharos-server via `actix_test::start` on an ephemeral port,
//! drives `pharos-jellyfin-test-client` through:
//!   1. GET /System/Info/Public         (unauth)
//!   2. POST /Users/AuthenticateByName  (Emby/MediaBrowser headers + token receipt)
//!   3. GET /Users/Me                   (token auth)
//!   4. GET /Items                      (browse)
//!   5. GET /Items/{id}                 (detail)
//!   6. GET /Library/VirtualFolders     (collections)
//!   7. HEAD /Videos/{id}/stream        (direct play range probe)
//!   8. GET /Sessions                   (active session list)
//!
//! Strict deserialization on every response — a missing or wrong-cased
//! field surfaces as `ClientError::Parse`. That's the bit
//! `tests/jellyfin_api.rs` could not assert via the in-process actix
//! harness.

use actix_test::TestServer;
use actix_web::{web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaStore, SecretString, UserId, UserPolicy, UserRecord, UserStore,
};
use pharos_jellyfin_test_client::{DeviceInfo, JellyfinClient};
use pharos_server::{api::jellyfin, auth::BuiltinAuth, middleware::LowercasePath, state::AppState};
use pharos_store_sqlx::sqlite::SqliteStore;
use pharos_sync::GroupRegistry;

async fn boot_server() -> (TestServer, String) {
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

    for (i, kind) in [
        MediaKind::Movie,
        MediaKind::Episode,
        MediaKind::Audio,
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
                kind: *kind,
                ..Default::default()
            })
            .await
            .unwrap();
    }
    let state = web::Data::new(AppState::new(stores, "pharos-compat".into()));
    let registry = web::Data::new(GroupRegistry::spawn());
    let server = actix_test::start(move || {
        App::new()
            .app_data(state.clone())
            .app_data(registry.clone())
            .app_data(web::Data::new(pharos_sync::SessionHub::new()))
            .wrap(LowercasePath)
            .configure(jellyfin::configure)
    });
    let url = server.url("").trim_end_matches('/').to_string();
    (server, url)
}

#[actix_web::test]
async fn full_jellyfin_client_flow() {
    let (_server, base) = boot_server().await;
    let mut client = JellyfinClient::new(base, DeviceInfo::default());

    // 1. Public system info — no auth required.
    let info = client.system_info_public().await.unwrap();
    assert_eq!(info.product_name, "Jellyfin Server");
    assert_eq!(info.server_name, "pharos-compat");
    assert!(!info.id.is_empty());
    assert!(!info.version.is_empty());

    // 2. Authenticate. Client stashes the token internally.
    let auth = client.authenticate_by_name("ali", "hunter2").await.unwrap();
    assert!(!auth.access_token.is_empty());
    assert_eq!(auth.user.name, "ali");
    assert!(auth.user.policy.is_administrator);
    assert!(auth.user.policy.enable_media_playback);
    assert_eq!(auth.server_id, info.id);
    assert!(client.token().is_some());

    // 3. /Users/Me with token.
    let me = client.users_me().await.unwrap();
    assert_eq!(me.id, auth.user.id);
    assert_eq!(me.name, "ali");

    // 4. Browse — pagination + total count are required fields.
    let items = client.items().await.unwrap();
    assert_eq!(items.total_record_count, 4);
    assert_eq!(items.items.len(), 4);
    let first = &items.items[0];
    assert!(!first.id.is_empty());
    assert!(!first.name.is_empty());
    // Type one of the strings clients pattern-match on.
    assert!(matches!(first.kind.as_str(), "Movie" | "Episode" | "Audio"));

    // 5. Detail by id.
    let one = client.item(&first.id).await.unwrap();
    assert_eq!(one.id, first.id);
    assert_eq!(one.server_id, info.id);

    // 6. Collections summary — clients pull this on startup.
    let folders = client.library_virtual_folders().await.unwrap();
    assert!(!folders.is_empty());
    assert_eq!(folders[0].name, "All Media");

    // 7. Direct-play HEAD. Path on disk does not exist — server should
    //    surface that as 404 rather than 500 or hang.
    let status = client.videos_stream_head(&first.id).await.unwrap();
    assert!(
        status.is_client_error() || status.is_success(),
        "unexpected stream status: {status}"
    );

    // 8. Sessions snapshot. Empty until a POST /Sessions/Playing fires
    //    — what matters here is that the endpoint deserializes to a list.
    let sessions = client.sessions().await.unwrap();
    assert!(sessions.is_empty());

    // 9-15: extended endpoints added since this test was written
    // (Genres / Artists / Albums / Suggestions / NextUp / SyncPlay
    // / Sessions remote). Drive them via awc with the same bearer
    // token; assert (status < 500, well-formed JSON envelope where
    // applicable). Catches mid-roll wire-shape regressions.
    let token = client.token().expect("authenticated").to_string();
    let base = client.base_url().to_string();
    let c = awc::Client::default();
    for path in [
        "/Genres",
        "/Artists",
        "/Albums",
        "/Studios",
        "/Search/Suggestions",
        &format!("/Users/{}/Suggestions", auth.user.id),
        "/Shows/NextUp?Limit=5",
        "/SyncPlay/List",
        &format!("/Items/{}/Similar", first.id),
    ] {
        let mut resp = c
            .get(&format!("{base}{path}"))
            .insert_header(("X-Emby-Token", token.as_str()))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "{path} → {}", resp.status());
        let body = resp.body().await.unwrap();
        let _v: serde_json::Value =
            serde_json::from_slice(&body).unwrap_or_else(|e| panic!("{path}: bad json {e}"));
    }
    // PlaybackInfo with a realistic device profile must return
    // SupportsDirectPlay alongside Container/MediaStreams.
    let mut pi = c
        .post(&format!("{base}/Items/{}/PlaybackInfo", first.id))
        .insert_header(("X-Emby-Token", token.as_str()))
        .insert_header(("content-type", "application/json"))
        .send_body(
            r#"{"DeviceProfile":{
              "DirectPlayProfiles":[{"Container":"webm","Type":"Video"}]
            }}"#,
        )
        .await
        .unwrap();
    assert!(pi.status().is_success(), "PlaybackInfo: {}", pi.status());
    let body = pi.body().await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v["MediaSources"][0]["Container"].is_string());
    assert!(v["PlaySessionId"].is_string());
    // Sessions remote-control: Pause command should 204 even with
    // no live session matching the id (the bus delivers; nobody
    // listens).
    let pause = c
        .post(&format!("{base}/Sessions/no-such/Playing/Pause"))
        .insert_header(("X-Emby-Token", token.as_str()))
        .insert_header(("content-type", "application/json"))
        .send_body("{}")
        .await
        .unwrap();
    assert_eq!(pause.status().as_u16(), 204);
}

#[actix_web::test]
async fn wrong_credentials_return_401() {
    let (_server, base) = boot_server().await;
    let mut client = JellyfinClient::new(base, DeviceInfo::default());
    let err = client
        .authenticate_by_name("ali", "wrong-password")
        .await
        .expect_err("must reject wrong password");
    match err {
        pharos_jellyfin_test_client::ClientError::Status { status, .. } => {
            assert_eq!(status, 401);
        }
        other => panic!("expected 401 status error, got {other:?}"),
    }
}

#[actix_web::test]
async fn unauthenticated_calls_return_401() {
    let (_server, base) = boot_server().await;
    let client = JellyfinClient::new(base, DeviceInfo::default());
    let err = client.users_me().await.expect_err("must require auth");
    match err {
        pharos_jellyfin_test_client::ClientError::Status { status, .. } => {
            assert_eq!(status, 401);
        }
        other => panic!("expected 401, got {other:?}"),
    }
}
