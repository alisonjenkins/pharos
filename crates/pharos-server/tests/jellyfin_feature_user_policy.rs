//! Feature inventory — **user permission / policy** (gap B, backlog T68).
//!
//! jellyfin-web's dashboard writes a large `UserPolicy` on
//! `POST /Users/{id}/Policy` (library access, parental control, disable/hide,
//! session limits, feature flags). pharos's domain `UserPolicy` has one field
//! (`admin`), so every other key is dropped on write and hardcoded on read,
//! and none is enforced.
//!
//! Round-trip tests POST a policy then GET it back and assert the field
//! survived; enforcement tests assert the *behaviour* the policy implies.
//! All assertions are on the Jellyfin wire JSON. The `#[ignore]`d tests are
//! the T68 backlog.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::test;
use serde_json::{json, Value};

mod common;
use common::{build_app, seed_rich};

/// POST a policy body for `uid` (as the admin); return the HTTP status.
async fn post_policy(f: &common::Fixture, uid: &str, policy: Value) -> u16 {
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::post()
        .uri(&format!("/Users/{uid}/Policy"))
        .insert_header(("X-Emby-Token", f.admin_token.as_str()))
        .set_json(&policy)
        .to_request();
    test::call_service(&app, req).await.status().as_u16()
}

/// GET `uid`'s current `Policy` object via the admin `/Users` list (which
/// carries every user's Policy), robust to whether a single-user GET is
/// admin-readable.
async fn read_policy(f: &common::Fixture, uid: &str) -> Value {
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::get()
        .uri("/Users")
        .insert_header(("X-Emby-Token", f.admin_token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let users: Value = serde_json::from_slice(&body).unwrap();
    users
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["Id"] == uid)
        .expect("target user present in /Users")["Policy"]
        .clone()
}

fn guest(f: &common::Fixture) -> String {
    f.user_id.0.simple().to_string()
}

// ---- live: confirmed-working surfaces ----

#[actix_web::test]
async fn policy_roundtrip_is_administrator() {
    let f = seed_rich().await;
    let g = guest(&f);
    assert_eq!(
        post_policy(&f, &g, json!({ "IsAdministrator": true })).await,
        204
    );
    assert_eq!(read_policy(&f, &g).await["IsAdministrator"], true);
}

#[actix_web::test]
async fn policy_grants_syncplay_access() {
    // jellyfin-web hides the group-watch (SyncPlay) UI — and "create a group"
    // silently no-ops — unless Policy.SyncPlayAccess grants it.
    let f = seed_rich().await;
    let g = guest(&f);
    assert_eq!(
        read_policy(&f, &g).await["SyncPlayAccess"],
        "CreateAndJoinGroups"
    );
}

/// The route exists (returns 200) but currently serves an empty array.
#[actix_web::test]
async fn localization_parental_ratings_route_present() {
    let f = seed_rich().await;
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::get()
        .uri("/Localization/ParentalRatings")
        .insert_header(("X-Emby-Token", f.admin_token.as_str()))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 200);
}

#[actix_web::test]
async fn localization_parental_ratings_nonempty() {
    let f = seed_rich().await;
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::get()
        .uri("/Localization/ParentalRatings")
        .insert_header(("X-Emby-Token", f.admin_token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert!(
        !v.as_array().unwrap().is_empty(),
        "parental-ratings picker needs a non-empty rating list"
    );
}

// ---- backlog T68: persist the full policy field set ----

#[actix_web::test]
async fn policy_roundtrip_is_disabled() {
    let f = seed_rich().await;
    let g = guest(&f);
    assert_eq!(
        post_policy(&f, &g, json!({ "IsDisabled": true })).await,
        204
    );
    assert_eq!(read_policy(&f, &g).await["IsDisabled"], true);
}

#[actix_web::test]
async fn policy_roundtrip_is_hidden() {
    let f = seed_rich().await;
    let g = guest(&f);
    assert_eq!(post_policy(&f, &g, json!({ "IsHidden": true })).await, 204);
    assert_eq!(read_policy(&f, &g).await["IsHidden"], true);
}

#[actix_web::test]
async fn policy_roundtrip_enabled_folders() {
    let f = seed_rich().await;
    let g = guest(&f);
    let body = json!({ "EnableAllFolders": false, "EnabledFolders": [f.lib_a_wire] });
    assert_eq!(post_policy(&f, &g, body).await, 204);
    let p = read_policy(&f, &g).await;
    assert_eq!(p["EnableAllFolders"], false);
    assert_eq!(p["EnabledFolders"], json!([f.lib_a_wire]));
}

#[actix_web::test]
async fn policy_roundtrip_parental() {
    let f = seed_rich().await;
    let g = guest(&f);
    let body = json!({
        "MaxParentalRating": 7,
        "BlockUnratedItems": ["Movie"],
        "BlockedTags": ["gore"],
        "AllowedTags": ["family"],
        "AccessSchedules": [{ "DayOfWeek": "Sunday", "StartHour": 8.0, "EndHour": 20.0 }]
    });
    assert_eq!(post_policy(&f, &g, body).await, 204);
    let p = read_policy(&f, &g).await;
    assert_eq!(p["MaxParentalRating"], 7);
    assert_eq!(p["BlockedTags"], json!(["gore"]));
}

#[actix_web::test]
async fn policy_roundtrip_session_limits() {
    let f = seed_rich().await;
    let g = guest(&f);
    let body = json!({
        "MaxActiveSessions": 3,
        "LoginAttemptsBeforeLockout": 5,
        "RemoteClientBitrateLimit": 8_000_000
    });
    assert_eq!(post_policy(&f, &g, body).await, 204);
    let p = read_policy(&f, &g).await;
    assert_eq!(p["MaxActiveSessions"], 3);
    assert_eq!(p["LoginAttemptsBeforeLockout"], 5);
}

#[actix_web::test]
async fn policy_roundtrip_feature_flags() {
    let f = seed_rich().await;
    let g = guest(&f);
    let body = json!({
        "EnableLiveTvAccess": false,
        "EnableContentDownloading": false,
        "SyncPlayAccess": "None"
    });
    assert_eq!(post_policy(&f, &g, body).await, 204);
    let p = read_policy(&f, &g).await;
    assert_eq!(p["EnableLiveTvAccess"], false);
    assert_eq!(p["SyncPlayAccess"], "None");
}

// ---- backlog T68: enforce the policy (behaviour, not just echo) ----

#[actix_web::test]
async fn enforce_disabled_user_cannot_authenticate() {
    let f = seed_rich().await;
    let g = guest(&f);
    assert_eq!(
        post_policy(&f, &g, json!({ "IsDisabled": true })).await,
        204
    );

    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::post()
        .uri("/Users/AuthenticateByName")
        .set_json(json!({ "Username": "guest", "Pw": "hunter2" }))
        .to_request();
    let status = test::call_service(&app, req).await.status();
    assert!(
        status == 401 || status == 403,
        "disabled user must not authenticate, got {status}"
    );
}

#[actix_web::test]
async fn enforce_enabled_folders_filters_items() {
    let f = seed_rich().await;
    let g = guest(&f);
    // Restrict the guest to library A only.
    let body = json!({ "EnableAllFolders": false, "EnabledFolders": [f.lib_a_wire] });
    assert_eq!(post_policy(&f, &g, body).await, 204);

    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::get()
        .uri(&format!(
            "/Users/{g}/Items?IncludeItemTypes=Movie&Recursive=true"
        ))
        .insert_header(("X-Emby-Token", f.user_token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: Value = serde_json::from_slice(&body).unwrap();
    let names: Vec<&str> = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|it| it["Name"].as_str())
        .collect();
    assert!(
        names.contains(&"Rich Movie"),
        "library-A item should be visible"
    );
    assert!(
        !names.contains(&"Other Movie"),
        "library-B item must be hidden from a folder-restricted user"
    );
}

#[actix_web::test]
async fn enforce_max_parental_rating_filters_items() {
    let f = seed_rich().await;
    let g = guest(&f);
    // Rich Movie is rated PG-13; restrict the guest below that.
    let body = json!({ "MaxParentalRating": 0, "BlockUnratedItems": [] });
    assert_eq!(post_policy(&f, &g, body).await, 204);

    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::get()
        .uri(&format!(
            "/Users/{g}/Items?IncludeItemTypes=Movie&Recursive=true"
        ))
        .insert_header(("X-Emby-Token", f.user_token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: Value = serde_json::from_slice(&body).unwrap();
    let names: Vec<&str> = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|it| it["Name"].as_str())
        .collect();
    assert!(
        !names.contains(&"Rich Movie"),
        "PG-13 item must be filtered for a max-rating-0 user"
    );
}
