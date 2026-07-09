//! Feature inventory — **media library settings** (gap C, backlog T69).
//!
//! jellyfin-web's library dashboard configures a rich `LibraryOptions`
//! (metadata/image/subtitle fetchers + ordering, realtime monitor, photos,
//! preferred language, per-type options) and edits libraries via
//! `LibraryOptions` / `Name` / `Paths` sub-endpoints plus a
//! `Libraries/AvailableOptions` catalogue and an `Environment` folder picker.
//! pharos's `add_virtual_folder` keeps only `PathInfos[].Path`; the read side
//! emits a two-field default; the sub-endpoints + catalogue + picker are
//! absent.
//!
//! Assertions are on the Jellyfin wire JSON. `#[ignore]`d tests are the T69
//! backlog.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::test;
use serde_json::{json, Value};

mod common;
use common::{build_app, seed_rich};

/// GET `/Library/VirtualFolders` as the admin.
async fn list_folders(f: &common::Fixture) -> Value {
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::get()
        .uri("/Library/VirtualFolders")
        .insert_header(("X-Emby-Token", f.admin_token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    serde_json::from_slice(&body).unwrap()
}

fn folder<'a>(list: &'a Value, name: &str) -> Option<&'a Value> {
    list.as_array()
        .unwrap()
        .iter()
        .find(|vf| vf["Name"] == name)
}

// ---- live: confirmed-working surfaces ----

#[actix_web::test]
async fn list_virtual_folders_returns_seeded_libraries() {
    let f = seed_rich().await;
    let list = list_folders(&f).await;
    assert!(folder(&list, "Movies A").is_some(), "library A listed");
    assert!(folder(&list, "Movies B").is_some(), "library B listed");
}

#[actix_web::test]
async fn remove_virtual_folder_deletes() {
    let f = seed_rich().await;
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::delete()
        .uri("/Library/VirtualFolders?name=Movies%20B")
        .insert_header(("X-Emby-Token", f.admin_token.as_str()))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 204);
    let list = list_folders(&f).await;
    assert!(folder(&list, "Movies B").is_none(), "library B removed");
    assert!(folder(&list, "Movies A").is_some(), "library A untouched");
}

// ---- backlog T69: honour + expose LibraryOptions ----

#[actix_web::test]
#[ignore = "gap: add_virtual_folder drops every LibraryOptions field but Path (T69)"]
async fn add_virtual_folder_persists_library_options() {
    let f = seed_rich().await;
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::post()
        .uri("/Library/VirtualFolders?name=Fresh&collectionType=movies")
        .insert_header(("X-Emby-Token", f.admin_token.as_str()))
        .set_json(json!({
            "LibraryOptions": {
                "PathInfos": [{ "Path": "/freshlib" }],
                "EnablePhotos": true,
                "PreferredMetadataLanguage": "fr"
            }
        }))
        .to_request();
    let status = test::call_service(&app, req).await.status();
    assert!(status.is_success(), "add should succeed, got {status}");

    let list = list_folders(&f).await;
    let fresh = folder(&list, "Fresh").expect("new library present");
    assert_eq!(
        fresh["LibraryOptions"]["EnablePhotos"], true,
        "EnablePhotos from the request should round-trip"
    );
    assert_eq!(
        fresh["LibraryOptions"]["PreferredMetadataLanguage"], "fr",
        "PreferredMetadataLanguage should round-trip"
    );
}

#[actix_web::test]
#[ignore = "gap: POST /Library/VirtualFolders/LibraryOptions endpoint absent (T69)"]
async fn update_virtual_folder_options_roundtrip() {
    let f = seed_rich().await;
    let list = list_folders(&f).await;
    let id = folder(&list, "Movies A").unwrap()["ItemId"]
        .as_str()
        .unwrap()
        .to_string();

    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::post()
        .uri("/Library/VirtualFolders/LibraryOptions")
        .insert_header(("X-Emby-Token", f.admin_token.as_str()))
        .set_json(json!({ "Id": id, "LibraryOptions": { "EnableRealtimeMonitor": true } }))
        .to_request();
    let status = test::call_service(&app, req).await.status();
    assert!(
        status.is_success(),
        "update-options should be a real endpoint, got {status}"
    );

    let list = list_folders(&f).await;
    assert_eq!(
        folder(&list, "Movies A").unwrap()["LibraryOptions"]["EnableRealtimeMonitor"],
        true
    );
}

#[actix_web::test]
#[ignore = "gap: POST /Library/VirtualFolders/Name (rename) endpoint absent (T69)"]
async fn rename_virtual_folder() {
    let f = seed_rich().await;
    let list = list_folders(&f).await;
    let id = folder(&list, "Movies A").unwrap()["ItemId"]
        .as_str()
        .unwrap()
        .to_string();

    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::post()
        .uri(&format!(
            "/Library/VirtualFolders/Name?id={id}&newName=Films"
        ))
        .insert_header(("X-Emby-Token", f.admin_token.as_str()))
        .to_request();
    let status = test::call_service(&app, req).await.status();
    assert!(
        status.is_success(),
        "rename should be a real endpoint, got {status}"
    );

    let list = list_folders(&f).await;
    assert!(folder(&list, "Films").is_some(), "library renamed to Films");
}

#[actix_web::test]
#[ignore = "gap: POST/DELETE /Library/VirtualFolders/Paths endpoints absent (T69)"]
async fn add_and_remove_media_path() {
    let f = seed_rich().await;
    let list = list_folders(&f).await;
    let name = folder(&list, "Movies A").unwrap()["Name"]
        .as_str()
        .unwrap()
        .to_string();

    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::post()
        .uri("/Library/VirtualFolders/Paths")
        .insert_header(("X-Emby-Token", f.admin_token.as_str()))
        .set_json(json!({ "Name": name, "PathInfo": { "Path": "/libA-extra" } }))
        .to_request();
    let status = test::call_service(&app, req).await.status();
    assert!(
        status.is_success(),
        "add-path should be a real endpoint, got {status}"
    );

    let list = list_folders(&f).await;
    let locations = folder(&list, "Movies A").unwrap()["Locations"]
        .as_array()
        .unwrap();
    assert!(
        locations.iter().any(|l| l == "/libA-extra"),
        "added path should appear in Locations"
    );
}

#[actix_web::test]
#[ignore = "gap: GET /Libraries/AvailableOptions (fetcher/TypeOptions catalogue) absent (T69)"]
async fn available_options_lists_fetchers_and_typeoptions() {
    let f = seed_rich().await;
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::get()
        .uri("/Libraries/AvailableOptions?LibraryContentType=movies")
        .insert_header(("X-Emby-Token", f.admin_token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200, "AvailableOptions should resolve");
    let body = test::read_body(resp).await;
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert!(
        v.get("TypeOptions").is_some(),
        "carries per-type fetcher options"
    );
    assert!(v.get("MetadataSavers").is_some(), "carries metadata savers");
}

#[actix_web::test]
#[ignore = "gap: GET /Environment/DirectoryContents (folder picker) absent (T69)"]
async fn environment_directory_contents() {
    let f = seed_rich().await;
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::get()
        .uri("/Environment/DirectoryContents?path=/&includeDirectories=true")
        .insert_header(("X-Emby-Token", f.admin_token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200, "folder picker should resolve");
}
