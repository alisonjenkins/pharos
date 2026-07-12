//! T70 — `/Playlists` controller end-to-end: create → get → list (with
//! per-entry `PlaylistItemId`) → add (duplicate) → move → remove → delete,
//! plus the unknown-playlist 404 paths. Drives the real HTTP surface through
//! the same `build_app` wiring jellyfin-web hits.
//!
//! Each helper re-inits the app over the shared `Fixture::state` (a cloned
//! `web::Data`, so the store is shared) — the same pattern the inventory
//! suite uses, which sidesteps naming actix's opaque test-service type.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::test;
use serde_json::Value;

mod common;
use common::{build_app, seed_rich};

async fn get_json(f: &common::Fixture, uri: &str) -> (u16, Value) {
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::get()
        .uri(uri)
        .insert_header(("X-Emby-Token", f.admin_token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    let status = resp.status().as_u16();
    let body = test::read_body(resp).await;
    let v = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (status, v)
}

async fn send(f: &common::Fixture, method: &str, uri: &str) -> u16 {
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = match method {
        "POST" => test::TestRequest::post(),
        "DELETE" => test::TestRequest::delete(),
        _ => test::TestRequest::get(),
    }
    .uri(uri)
    .insert_header(("X-Emby-Token", f.admin_token.as_str()))
    .to_request();
    test::call_service(&app, req).await.status().as_u16()
}

async fn items(f: &common::Fixture, pid: &str) -> Vec<Value> {
    let (_, v) = get_json(f, &format!("/Playlists/{pid}/Items")).await;
    v["Items"].as_array().cloned().unwrap_or_default()
}

#[actix_web::test]
async fn playlist_full_lifecycle() {
    let f = seed_rich().await;

    // 1. Create seeded with both fixture items, in order.
    let (status, v) = get_json_post(
        &f,
        &format!(
            "/Playlists?Name=Mix&Ids={},{}&MediaType=Video",
            f.rich_item_id, f.other_item_id
        ),
    )
    .await;
    assert_eq!(status, 200);
    let pid = v["Id"].as_str().expect("create returns Id").to_string();
    assert!(!pid.is_empty());

    // 2. Header item.
    let (hs, hv) = get_json(&f, &format!("/Playlists/{pid}")).await;
    assert_eq!(hs, 200);
    assert_eq!(hv["Type"], "Playlist");
    assert_eq!(hv["Id"], pid);
    assert_eq!(hv["ChildCount"], 2);

    // 3. Items in curated order, each with a PlaylistItemId.
    let list = items(&f, &pid).await;
    assert_eq!(list.len(), 2);
    assert_eq!(
        list[0]["Id"].as_str().unwrap(),
        format!("{:032x}", f.rich_item_id)
    );
    assert_eq!(
        list[1]["Id"].as_str().unwrap(),
        format!("{:032x}", f.other_item_id)
    );
    assert!(list[0]["PlaylistItemId"].as_str().is_some());

    // 4. Append the first item again — a playlist may hold it twice.
    assert_eq!(
        send(
            &f,
            "POST",
            &format!("/Playlists/{pid}/Items?Ids={}", f.rich_item_id)
        )
        .await,
        204
    );
    let list = items(&f, &pid).await;
    assert_eq!(list.len(), 3);
    assert_eq!(
        list[2]["Id"].as_str().unwrap(),
        format!("{:032x}", f.rich_item_id)
    );
    let appended_entry = list[2]["PlaylistItemId"].as_str().unwrap().to_string();

    // 5. Move the appended entry to the front.
    assert_eq!(
        send(
            &f,
            "POST",
            &format!("/Playlists/{pid}/Items/{appended_entry}/Move/0")
        )
        .await,
        204
    );
    let list = items(&f, &pid).await;
    assert_eq!(list[0]["PlaylistItemId"].as_str().unwrap(), appended_entry);

    // 6. Remove that entry by its EntryId.
    assert_eq!(
        send(
            &f,
            "DELETE",
            &format!("/Playlists/{pid}/Items?EntryIds={appended_entry}")
        )
        .await,
        204
    );
    assert_eq!(items(&f, &pid).await.len(), 2);

    // 7. Delete the playlist.
    assert_eq!(send(&f, "DELETE", &format!("/Playlists/{pid}")).await, 204);
    let (gone, _) = get_json(&f, &format!("/Playlists/{pid}")).await;
    assert_eq!(gone, 404);
}

#[actix_web::test]
async fn unknown_playlist_is_404() {
    let f = seed_rich().await;
    assert_eq!(send(&f, "GET", "/Playlists/deadbeef").await, 404);
    assert_eq!(send(&f, "GET", "/Playlists/deadbeef/Items").await, 404);
    assert_eq!(send(&f, "DELETE", "/Playlists/deadbeef").await, 404);
    assert_eq!(
        send(
            &f,
            "POST",
            &format!("/Playlists/deadbeef/Items?Ids={}", f.rich_item_id)
        )
        .await,
        404
    );
}

#[actix_web::test]
async fn playlist_appears_in_items_listing_and_resolves() {
    let f = seed_rich().await;
    let (_, v) = get_json_post(&f, &format!("/Playlists?Name=Faves&Ids={}", f.rich_item_id)).await;
    let pid = v["Id"].as_str().unwrap().to_string();

    // IncludeItemTypes=Playlist lists it (Playlists library view).
    let (s, list) = get_json(&f, "/Items?IncludeItemTypes=Playlist&Recursive=true").await;
    assert_eq!(s, 200);
    let items = list["Items"].as_array().unwrap();
    assert!(
        items
            .iter()
            .any(|i| i["Id"].as_str() == Some(&pid) && i["Type"].as_str() == Some("Playlist")),
        "created playlist should appear in the Playlist listing"
    );

    // /Items/{id} resolves the playlist header item.
    let (rs, rv) = get_json(&f, &format!("/Items/{pid}")).await;
    assert_eq!(rs, 200);
    assert_eq!(rv["Type"], "Playlist");
    assert_eq!(rv["Id"], pid);
}

/// POST returning JSON — the create endpoint's shape.
async fn get_json_post(f: &common::Fixture, uri: &str) -> (u16, Value) {
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::post()
        .uri(uri)
        .insert_header(("X-Emby-Token", f.admin_token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    let status = resp.status().as_u16();
    let body = test::read_body(resp).await;
    let v = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (status, v)
}
