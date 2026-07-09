//! Feature inventory — **rest of the jellyfin-web surface**.
//!
//! One placeholder per remaining gap area, tagged with its backlog task id,
//! plus live guards for confirmed-working surfaces and a consistency check
//! tying the `#[ignore]` scaffold to `docs/jellyfin-web-feature-matrix.md`.
//!
//! Backlog ids: T70 playlists · T72 named-configuration persistence · T73
//! activity log · T74 scheduled-task execution · T75 plugin/package install ·
//! T76 item ops (merge / content type / remote images / remote subtitles /
//! lyrics / instant mix). DisplayPreferences (once reserved as T71) turned
//! out already implemented and is covered by a live round-trip test below.
//!
//! Assertions are on the Jellyfin wire JSON. Enabling an `#[ignore]`d test
//! and turning it green is the implementation task named by its `(Txx)` tag.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::test;
use serde_json::Value;

mod common;
use common::{build_app, seed_rich};

async fn get_status(f: &common::Fixture, uri: &str) -> u16 {
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::get()
        .uri(uri)
        .insert_header(("X-Emby-Token", f.admin_token.as_str()))
        .to_request();
    test::call_service(&app, req).await.status().as_u16()
}

async fn post_status(f: &common::Fixture, uri: &str) -> u16 {
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::post()
        .uri(uri)
        .insert_header(("X-Emby-Token", f.admin_token.as_str()))
        .to_request();
    test::call_service(&app, req).await.status().as_u16()
}

fn admin_uid(f: &common::Fixture) -> String {
    f.admin_id.0.simple().to_string()
}

// ================= live guards (confirmed-working) =================

#[actix_web::test]
async fn search_hints_endpoint() {
    let f = seed_rich().await;
    assert_eq!(get_status(&f, "/Search/Hints?searchTerm=Rich").await, 200);
}

#[actix_web::test]
async fn quick_connect_enabled_present() {
    let f = seed_rich().await;
    assert_eq!(get_status(&f, "/QuickConnect/Enabled").await, 200);
}

#[actix_web::test]
async fn api_keys_endpoint_present() {
    let f = seed_rich().await;
    assert_eq!(get_status(&f, "/Auth/Keys").await, 200);
}

#[actix_web::test]
async fn favorite_toggle_roundtrip() {
    let f = seed_rich().await;
    let uid = admin_uid(&f);
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::post()
        .uri(&format!("/Users/{uid}/FavoriteItems/{}", f.rich_item_id))
        .insert_header(("X-Emby-Token", f.admin_token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["IsFavorite"], true);
}

#[actix_web::test]
async fn played_toggle_roundtrip() {
    let f = seed_rich().await;
    let uid = admin_uid(&f);
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::post()
        .uri(&format!("/Users/{uid}/PlayedItems/{}", f.rich_item_id))
        .insert_header(("X-Emby-Token", f.admin_token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["Played"], true);
}

// ===================== backlog placeholders =======================

#[actix_web::test]
#[ignore = "gap: no /Playlists controller — create/add/reorder absent (T70)"]
async fn playlists_crud() {
    let f = seed_rich().await;
    let status = post_status(&f, &format!("/Playlists?Name=Mix&Ids={}", f.rich_item_id)).await;
    assert!(
        (200..300).contains(&status),
        "create playlist should succeed, got {status}"
    );
}

/// DisplayPreferences persist per (user, id, client) — live round-trip guard.
#[actix_web::test]
async fn display_preferences_roundtrip() {
    let f = seed_rich().await;
    let app = test::init_service(build_app(f.state.clone())).await;
    let post = test::TestRequest::post()
        .uri("/DisplayPreferences/usersettings?client=emby")
        .insert_header(("X-Emby-Token", f.admin_token.as_str()))
        .set_json(serde_json::json!({
            "Id": "usersettings",
            "CustomPrefs": { "homesection0": "latestmedia" }
        }))
        .to_request();
    assert_eq!(test::call_service(&app, post).await.status(), 204);

    let get = test::TestRequest::get()
        .uri("/DisplayPreferences/usersettings?client=emby")
        .insert_header(("X-Emby-Token", f.admin_token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, get).await;
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["CustomPrefs"]["homesection0"], "latestmedia");
}

/// POST a named configuration and assert the change survives a re-GET. The
/// GET already serves well-shaped defaults; the gap is that POST is a no-op
/// (pharos's config is the read-only toml), so nothing persists.
async fn named_config_roundtrip_survives(f: &common::Fixture, key: &str, patch: Value) -> bool {
    let app = test::init_service(build_app(f.state.clone())).await;
    let post = test::TestRequest::post()
        .uri(&format!("/System/Configuration/{key}"))
        .insert_header(("X-Emby-Token", f.admin_token.as_str()))
        .set_json(&patch)
        .to_request();
    let _ = test::call_service(&app, post).await;
    let get = test::TestRequest::get()
        .uri(&format!("/System/Configuration/{key}"))
        .insert_header(("X-Emby-Token", f.admin_token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, get).await;
    let got: Value = serde_json::from_slice(&body).unwrap();
    let obj = patch.as_object().unwrap();
    obj.iter().all(|(k, want)| got.get(k) == Some(want))
}

#[actix_web::test]
#[ignore = "gap: POST /System/Configuration/encoding is a no-op — transcoding settings don't persist (T72)"]
async fn named_configuration_encoding() {
    let f = seed_rich().await;
    assert!(
        named_config_roundtrip_survives(
            &f,
            "encoding",
            serde_json::json!({ "HardwareAccelerationType": "nvenc" })
        )
        .await,
        "encoding config change should persist"
    );
}

#[actix_web::test]
#[ignore = "gap: POST /System/Configuration/network is a no-op — networking settings don't persist (T72)"]
async fn named_configuration_network() {
    let f = seed_rich().await;
    assert!(
        named_config_roundtrip_survives(&f, "network", serde_json::json!({ "EnableHttps": true }))
            .await,
        "network config change should persist"
    );
}

#[actix_web::test]
#[ignore = "gap: POST /System/Configuration/metadata is a no-op — library display settings don't persist (T72)"]
async fn named_configuration_metadata() {
    let f = seed_rich().await;
    assert!(
        named_config_roundtrip_survives(
            &f,
            "metadata",
            serde_json::json!({ "PreferredMetadataLanguage": "fr" })
        )
        .await,
        "metadata config change should persist"
    );
}

#[actix_web::test]
#[ignore = "gap: POST /System/Configuration/livetv is a no-op — live TV settings don't persist (T72)"]
async fn named_configuration_livetv() {
    let f = seed_rich().await;
    assert!(
        named_config_roundtrip_survives(&f, "livetv", serde_json::json!({ "GuideDays": 7 })).await,
        "livetv config change should persist"
    );
}

#[actix_web::test]
#[ignore = "gap: activity log is an empty stub — real events not recorded (T73)"]
async fn activity_log_entries() {
    let f = seed_rich().await;
    // A real login should leave an activity entry.
    let app = test::init_service(build_app(f.state.clone())).await;
    let login = test::TestRequest::post()
        .uri("/Users/AuthenticateByName")
        .set_json(serde_json::json!({ "Username": "ali", "Pw": "hunter2" }))
        .to_request();
    let _ = test::call_service(&app, login).await;

    let req = test::TestRequest::get()
        .uri("/System/ActivityLog/Entries")
        .insert_header(("X-Emby-Token", f.admin_token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert!(
        v["TotalRecordCount"].as_u64().unwrap_or(0) >= 1,
        "expected at least one recorded activity entry"
    );
}

#[actix_web::test]
async fn scheduled_task_execution() {
    let f = seed_rich().await;
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::get()
        .uri("/ScheduledTasks")
        .insert_header(("X-Emby-Token", f.admin_token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert!(
        !v.as_array().unwrap().is_empty(),
        "a server should advertise built-in scheduled tasks"
    );
}

#[actix_web::test]
#[ignore = "gap: no plugin/package install pipeline (T75)"]
async fn plugins_install() {
    let f = seed_rich().await;
    let status = post_status(&f, "/Packages/Installed/Example").await;
    assert!(
        (200..300).contains(&status),
        "package install should succeed, got {status}"
    );
}

#[actix_web::test]
#[ignore = "gap: POST /Videos/MergeVersions absent (T76)"]
async fn item_merge_versions() {
    let f = seed_rich().await;
    let status = post_status(
        &f,
        &format!(
            "/Videos/MergeVersions?Ids={},{}",
            f.rich_item_id, f.other_item_id
        ),
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "merge should succeed, got {status}"
    );
}

#[actix_web::test]
#[ignore = "gap: POST /Items/{id}/ContentType absent (T76)"]
async fn item_content_type() {
    let f = seed_rich().await;
    let status = post_status(
        &f,
        &format!("/Items/{}/ContentType?contentType=Movies", f.rich_item_id),
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "set content-type should succeed, got {status}"
    );
}

#[actix_web::test]
#[ignore = "gap: GET /Items/{id}/RemoteImages absent (T76)"]
async fn remote_image_search() {
    let f = seed_rich().await;
    assert_eq!(
        get_status(
            &f,
            &format!("/Items/{}/RemoteImages?Type=Primary", f.rich_item_id)
        )
        .await,
        200
    );
}

#[actix_web::test]
#[ignore = "gap: GET /Items/{id}/RemoteSearch/Subtitles/{lang} absent (T76)"]
async fn remote_subtitle_search() {
    let f = seed_rich().await;
    assert_eq!(
        get_status(
            &f,
            &format!("/Items/{}/RemoteSearch/Subtitles/eng", f.rich_item_id)
        )
        .await,
        200
    );
}

#[actix_web::test]
#[ignore = "gap: lyrics endpoints absent (T76)"]
async fn lyrics_crud() {
    let f = seed_rich().await;
    assert_eq!(
        get_status(&f, &format!("/Audio/{}/Lyrics", f.rich_item_id)).await,
        200
    );
}

#[actix_web::test]
#[ignore = "gap: GET /Items/{id}/InstantMix absent (T76)"]
async fn item_instant_mix() {
    let f = seed_rich().await;
    assert_eq!(
        get_status(&f, &format!("/Items/{}/InstantMix", f.rich_item_id)).await,
        200
    );
}

// ===================== scaffold consistency =======================

/// Every `#[ignore = "… (Txx)"]` backlog id across the four feature suites
/// must be documented in the feature matrix — keeps doc and scaffold in
/// lockstep so an added gap can't silently escape the inventory.
// `use actix_web::test` shadows the bare `#[test]` attribute; qualify it.
#[::core::prelude::v1::test]
fn matrix_doc_lists_every_ignored_test() {
    const MANIFEST: &str = env!("CARGO_MANIFEST_DIR");
    let doc = std::fs::read_to_string(format!(
        "{MANIFEST}/../../docs/jellyfin-web-feature-matrix.md"
    ))
    .expect("feature matrix doc present");

    let files = [
        "jellyfin_feature_metadata.rs",
        "jellyfin_feature_user_policy.rs",
        "jellyfin_feature_library_options.rs",
        "jellyfin_feature_inventory.rs",
    ];
    let mut ids: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for file in files {
        let src = std::fs::read_to_string(format!("{MANIFEST}/tests/{file}")).unwrap();
        for line in src.lines().filter(|l| l.contains("#[ignore")) {
            // Extract the "(Txx)" backlog tag from the ignore reason.
            if let Some(open) = line.find("(T") {
                let rest = &line[open + 1..];
                if let Some(close) = rest.find(')') {
                    let tag = &rest[..close];
                    if tag.len() >= 2 && tag[1..].chars().all(|c| c.is_ascii_digit()) {
                        ids.insert(tag.to_string());
                    }
                }
            }
        }
    }

    assert!(
        !ids.is_empty(),
        "expected backlog ids in the ignore reasons"
    );
    for id in &ids {
        assert!(
            doc.contains(id.as_str()),
            "feature matrix doc is missing backlog id {id}"
        );
    }
}
