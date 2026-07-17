#![allow(clippy::unwrap_used, clippy::expect_used)]
//! /Users/{userId}/PlayedItems + /FavoriteItems + Resume (T33).

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaStore, SecretString, TokenStore, UserDataStore, UserId, UserPolicy,
    UserRecord, UserStore,
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
    let hash = auth.hash_password(&SecretString::new("hunter2")).unwrap();
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
    for (id, kind, title) in [
        (300_u64, MediaKind::Movie, "Movie A"),
        (301, MediaKind::Movie, "Movie B"),
        (302, MediaKind::Audio, "Track C"),
    ] {
        stores
            .put(MediaItem {
                id,
                path: format!("/m/{id}.x").into(),
                title: title.into(),
                kind,
                ..Default::default()
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
    App::new()
        .app_data(state)
        .wrap(LowercasePath)
        .configure(jellyfin::configure)
}

#[actix_web::test]
async fn mark_played_sets_played_and_increments_count() {
    let (state, token, uid) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::post()
        .uri(&format!("/Users/{}/PlayedItems/300", uid.0.simple()))
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["Played"], true);
    assert_eq!(v["PlayCount"], 1);
    assert_eq!(v["Key"], "300");
}

#[actix_web::test]
async fn userid_less_played_endpoint_marks_and_returns_dto() {
    // B84 — the modern jellyfin-sdk-kotlin (Android TV / Google TV) calls the
    // userId-less POST /UserPlayedItems/{id}; the user is the bearer. Missing
    // this route 404'd and CRASHED the app on the error response. The id
    // arrives as a dashed UUID (the SDK re-serialises ids dashed), so exercise
    // that exact shape.
    let (state, token, _uid) = seed().await;
    let app = test::init_service(build_app(state)).await;
    // 300 -> canonical dashless is 16 zeros + 000000000000012c; dashed form:
    let dashed = "00000000-0000-0000-0000-00000000012c";
    let req = test::TestRequest::post()
        .uri(&format!("/UserPlayedItems/{dashed}"))
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["Played"], true, "userId-less mark-played must succeed");
    assert_eq!(v["PlayCount"], 1);
    // DELETE unmarks via the same userId-less route.
    let req = test::TestRequest::delete()
        .uri("/UserPlayedItems/300")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["Played"], false);
    // Favourite variant too.
    let req = test::TestRequest::post()
        .uri("/UserFavoriteItems/302")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["IsFavorite"], true);
}

#[actix_web::test]
async fn mark_played_is_idempotent_after_unmark() {
    let (state, token, uid) = seed().await;
    let app = test::init_service(build_app(state)).await;
    // Mark twice -> play_count = 2.
    for _ in 0..2 {
        let req = test::TestRequest::post()
            .uri(&format!("/Users/{}/PlayedItems/300", uid.0.simple()))
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
    }
    // Then unmark -> played=false but play_count stays at 2.
    let req = test::TestRequest::delete()
        .uri(&format!("/Users/{}/PlayedItems/300", uid.0.simple()))
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["Played"], false);
    assert_eq!(v["PlayCount"], 2);
}

#[actix_web::test]
async fn played_item_endpoint_rejects_user_mismatch() {
    let (state, token, _uid) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::post()
        .uri("/Users/deadbeefdeadbeefdeadbeefdeadbeef/PlayedItems/300")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 403);
}

#[actix_web::test]
async fn played_item_404_for_unknown_item() {
    let (state, token, uid) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::post()
        .uri(&format!("/Users/{}/PlayedItems/9999", uid.0.simple()))
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 404);
}

#[actix_web::test]
async fn favorite_toggle_round_trips_via_get_item() {
    let (state, token, uid) = seed().await;
    let app = test::init_service(build_app(state)).await;
    // POST favorite.
    let req = test::TestRequest::post()
        .uri(&format!("/Users/{}/FavoriteItems/301", uid.0.simple()))
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    // Verify via GET /Items/{id} that UserData.IsFavorite is true.
    let req = test::TestRequest::get()
        .uri("/Items/301")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["UserData"]["IsFavorite"], true);

    // DELETE clears it.
    let req = test::TestRequest::delete()
        .uri(&format!("/Users/{}/FavoriteItems/301", uid.0.simple()))
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["IsFavorite"], false);
}

#[actix_web::test]
async fn resume_endpoint_lists_items_with_progress() {
    let (state, token, uid) = seed().await;
    let app = test::init_service(build_app(state)).await;
    // Drive progress on item 300 so it lands in Resume.
    let req = test::TestRequest::post()
        .uri("/Sessions/Playing/Progress")
        .insert_header(("X-Emby-Token", token.as_str()))
        .set_json(serde_json::json!({
            "ItemId": "300",
            "PlaySessionId": "sess-1",
            "PositionTicks": 50_000_000u64,
            "IsPaused": false
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 204);

    let req = test::TestRequest::get()
        .uri(&format!("/Users/{}/Items/Resume", uid.0.simple()))
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"], 1);
    let items = v["Items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["Id"], "0000000000000000000000000000012c");
    assert_eq!(items[0]["UserData"]["PlaybackPositionTicks"], 50_000_000u64);
}

#[actix_web::test]
async fn played_items_endpoint_excludes_item_from_resume() {
    let (state, token, uid) = seed().await;
    let app = test::init_service(build_app(state)).await;
    // First set a position, then mark played.
    let progress = test::TestRequest::post()
        .uri("/Sessions/Playing/Progress")
        .insert_header(("X-Emby-Token", token.as_str()))
        .set_json(serde_json::json!({
            "ItemId": "300",
            "PlaySessionId": "sess",
            "PositionTicks": 1u64,
        }))
        .to_request();
    let _ = test::call_service(&app, progress).await;
    let mark = test::TestRequest::post()
        .uri(&format!("/Users/{}/PlayedItems/300", uid.0.simple()))
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let _ = test::call_service(&app, mark).await;
    // Resume should now be empty.
    let req = test::TestRequest::get()
        .uri(&format!("/Users/{}/Items/Resume", uid.0.simple()))
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"], 0);
}

#[actix_web::test]
async fn mark_played_broadcasts_user_data_changed_on_bus() {
    use pharos_server::state::SocketBroadcast;
    let (state, token, uid) = seed().await;
    let mut bus = state.bus.subscribe();
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::post()
        .uri(&format!("/Users/{}/PlayedItems/300", uid.0.simple()))
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());
    let msg = tokio::time::timeout(std::time::Duration::from_millis(500), bus.recv())
        .await
        .expect("broadcast timeout")
        .expect("broadcast recv");
    match msg {
        SocketBroadcast::UserDataChanged { user_id, entries } => {
            assert_eq!(user_id, uid.0.simple().to_string());
            // B36 — the broadcast must carry the FULL DTO: jellyfin-web
            // matches cards by the 32-hex wire ItemId (a decimal id
            // matches nothing) and applies Played/IsFavorite in place.
            assert_eq!(entries.len(), 1);
            let e = &entries[0];
            assert_eq!(e["ItemId"], pharos_jellyfin_api::dto::wire_item_id(300));
            assert_eq!(e["Key"], "300");
            assert_eq!(e["Played"], true);
            assert_eq!(e["PlayCount"], 1);
            assert_eq!(e["IsFavorite"], false);
        }
        other => panic!("expected UserDataChanged, got {other:?}"),
    }
}

#[actix_web::test]
async fn unmark_played_broadcasts_played_false() {
    use pharos_server::state::SocketBroadcast;
    let (state, token, uid) = seed().await;
    let app = test::init_service(build_app(state.clone())).await;
    let mark = test::TestRequest::post()
        .uri(&format!("/Users/{}/PlayedItems/300", uid.0.simple()))
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let _ = test::call_service(&app, mark).await;
    let mut bus = state.bus.subscribe();
    let unmark = test::TestRequest::delete()
        .uri(&format!("/Users/{}/PlayedItems/300", uid.0.simple()))
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    assert!(test::call_service(&app, unmark).await.status().is_success());
    let msg = tokio::time::timeout(std::time::Duration::from_millis(500), bus.recv())
        .await
        .expect("broadcast timeout")
        .expect("broadcast recv");
    match msg {
        SocketBroadcast::UserDataChanged { entries, .. } => {
            assert_eq!(entries[0]["Played"], false);
            assert_eq!(
                entries[0]["ItemId"],
                pharos_jellyfin_api::dto::wire_item_id(300)
            );
        }
        other => panic!("expected UserDataChanged, got {other:?}"),
    }
}

#[actix_web::test]
async fn favorite_toggle_broadcasts_is_favorite() {
    use pharos_server::state::SocketBroadcast;
    let (state, token, uid) = seed().await;
    let mut bus = state.bus.subscribe();
    let app = test::init_service(build_app(state.clone())).await;
    let fav = test::TestRequest::post()
        .uri(&format!("/Users/{}/FavoriteItems/301", uid.0.simple()))
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    assert!(test::call_service(&app, fav).await.status().is_success());
    let msg = tokio::time::timeout(std::time::Duration::from_millis(500), bus.recv())
        .await
        .expect("broadcast timeout")
        .expect("broadcast recv");
    match msg {
        SocketBroadcast::UserDataChanged { entries, .. } => {
            assert_eq!(entries[0]["IsFavorite"], true);
            assert_eq!(entries[0]["Played"], false);
            assert_eq!(
                entries[0]["ItemId"],
                pharos_jellyfin_api::dto::wire_item_id(301)
            );
        }
        other => panic!("expected UserDataChanged, got {other:?}"),
    }
    let unfav = test::TestRequest::delete()
        .uri(&format!("/Users/{}/FavoriteItems/301", uid.0.simple()))
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    assert!(test::call_service(&app, unfav).await.status().is_success());
    let msg = tokio::time::timeout(std::time::Duration::from_millis(500), bus.recv())
        .await
        .expect("broadcast timeout")
        .expect("broadcast recv");
    match msg {
        SocketBroadcast::UserDataChanged { entries, .. } => {
            assert_eq!(entries[0]["IsFavorite"], false);
        }
        other => panic!("expected UserDataChanged, got {other:?}"),
    }
}

/// Seed a two-season show for the synthetic-folder cascade tests (B36).
async fn seed_show() -> (web::Data<AppState>, String, UserId) {
    use pharos_core::SeriesInfo;
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("hunter2")).unwrap();
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
    for (id, season, ep) in [(400_u64, 1, 1), (401, 1, 2), (402, 2, 1)] {
        stores
            .put(MediaItem {
                id,
                path: format!("/tv/Buffy/S{season:02}E{ep:02}.mkv").into(),
                title: format!("s{season:02}e{ep:02}"),
                kind: MediaKind::Episode,
                series: Some(SeriesInfo {
                    series_name: "Buffy".into(),
                    season_number: Some(season),
                    episode_number: Some(ep),
                    series_folder: Some("/tv/Buffy".into()),
                    series_year: None,
                }),
                ..Default::default()
            })
            .await
            .unwrap();
    }
    let state = web::Data::new(AppState::new(stores, "test".into()));
    (state, token.0.expose().to_string(), uid)
}

#[actix_web::test]
async fn mark_season_played_cascades_to_episodes_and_broadcasts_batch() {
    use pharos_server::state::SocketBroadcast;
    let (state, token, uid) = seed_show().await;
    let mut bus = state.bus.subscribe();
    let app = test::init_service(build_app(state.clone())).await;
    let season_id = pharos_jellyfin_api::dto::season_id_for_key(Some("/tv/Buffy"), "Buffy", 1);
    let req = test::TestRequest::post()
        .uri(&format!(
            "/Users/{}/PlayedItems/{season_id}",
            uid.0.simple()
        ))
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["Played"], true, "folder-level response DTO: {v}");
    // Season 1 episodes flip played; season 2 untouched.
    for id in [400_u64, 401] {
        let ud = state.stores.get_user_data(uid, id).await.unwrap();
        assert!(ud.played, "episode {id} must cascade to played");
    }
    assert!(!state.stores.get_user_data(uid, 402).await.unwrap().played);
    // One frame, all changed entries + the folder entry keyed by the synth id.
    let msg = tokio::time::timeout(std::time::Duration::from_millis(500), bus.recv())
        .await
        .expect("broadcast timeout")
        .expect("broadcast recv");
    match msg {
        SocketBroadcast::UserDataChanged { user_id, entries } => {
            assert_eq!(user_id, uid.0.simple().to_string());
            let ids: Vec<_> = entries.iter().map(|e| e["ItemId"].clone()).collect();
            assert!(
                ids.contains(&serde_json::json!(pharos_jellyfin_api::dto::wire_item_id(
                    400
                )))
            );
            assert!(
                ids.contains(&serde_json::json!(pharos_jellyfin_api::dto::wire_item_id(
                    401
                )))
            );
            assert!(ids.contains(&serde_json::json!(season_id)));
            assert!(entries.iter().all(|e| e["Played"] == true), "{entries:?}");
        }
        other => panic!("expected UserDataChanged, got {other:?}"),
    }
}

#[actix_web::test]
async fn mark_series_favorite_cascades_all_episodes() {
    let (state, token, uid) = seed_show().await;
    let app = test::init_service(build_app(state.clone())).await;
    let series_id = pharos_jellyfin_api::dto::series_id_for_key(Some("/tv/Buffy"), "Buffy");
    let req = test::TestRequest::post()
        .uri(&format!(
            "/Users/{}/FavoriteItems/{series_id}",
            uid.0.simple()
        ))
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["IsFavorite"], true, "folder-level response DTO: {v}");
    for id in [400_u64, 401, 402] {
        let ud = state.stores.get_user_data(uid, id).await.unwrap();
        assert!(ud.is_favorite, "episode {id} must cascade to favorite");
    }
}

#[actix_web::test]
async fn unknown_synth_id_is_404_not_400() {
    // GUID-shaped id that matches no series/season — must 404 (item not
    // found), not 400: jellyfin-web treats 400 as a client bug.
    let (state, token, uid) = seed_show().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::post()
        .uri(&format!(
            "/Users/{}/PlayedItems/ffffffffffffffffffffffffffffffff",
            uid.0.simple()
        ))
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 404);
}

#[actix_web::test]
async fn requires_auth_on_played_items() {
    let (state, _token, uid) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::post()
        .uri(&format!("/Users/{}/PlayedItems/300", uid.0.simple()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401);
}
