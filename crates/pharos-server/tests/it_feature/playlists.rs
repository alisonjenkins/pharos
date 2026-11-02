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

use crate::common;
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

    // B92 — the Android TV kotlin SDK re-serialises the playlist id DASHED; the
    // store matches wire_id exactly, so a dashed id 404'd every playlist route.
    // It must resolve identically to the dashless form.
    let dashed = format!(
        "{}-{}-{}-{}-{}",
        &pid[0..8],
        &pid[8..12],
        &pid[12..16],
        &pid[16..20],
        &pid[20..32],
    );
    let (ds, dv) = get_json(&f, &format!("/Playlists/{dashed}")).await;
    assert_eq!(ds, 200, "dashed playlist id must resolve");
    assert_eq!(dv["Id"], pid);
    let (dis, _) = get_json(&f, &format!("/Playlists/{dashed}/Items")).await;
    assert_eq!(dis, 200, "dashed playlist items must resolve");

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

fn playlist_denied(snapshotter: &pharos_test_metrics::CumulativeSnapshotter, action: &str) -> u64 {
    use metrics_util::debugging::DebugValue;
    snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .filter(|(k, ..)| {
            let key = k.key();
            key.name() == "pharos_playlist_access_denied_total"
                && key
                    .labels()
                    .any(|l| l.key() == "action" && l.value() == action)
        })
        .map(|(.., v)| match v {
            DebugValue::Counter(n) => n,
            other => panic!("{other:?}"),
        })
        .sum()
}

/// B259: a playlist answers only its owner. Another signed-in user gets what
/// a playlist that does not exist answers on every playlist route, and
/// cannot read, change or delete it. A server-owned playlist (no owner) is
/// readable by everyone and changed only by an admin. Each refusal counts
/// under `pharos_playlist_access_denied_total{action}`.
/// Disarm check: with `services::access::check_playlist` answering `Ok`
/// first, the other user's requests succeed and the counter reads 0.
#[actix_web::test]
async fn a_playlist_answers_only_its_owner() {
    use pharos_core::PlaylistStore as _;
    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);

    let f = seed_rich().await;
    let (_, v) = get_json_post(
        &f,
        &format!(
            "/Playlists?Name=Mine&Ids={},{}&MediaType=Video",
            f.rich_item_id, f.other_item_id
        ),
    )
    .await;
    let pid = v["Id"].as_str().unwrap().to_string();
    let entry = items(&f, &pid).await[0]["PlaylistItemId"]
        .as_str()
        .unwrap()
        .to_string();

    let app = test::init_service(build_app(f.state.clone())).await;
    let as_user = |method: actix_web::http::Method, uri: String| {
        test::TestRequest::default()
            .method(method)
            .uri(&uri)
            .insert_header(("X-Emby-Token", f.user_token.as_str()))
            .to_request()
    };
    use actix_web::http::Method;
    let other = f.other_item_id;
    let mut answered = Vec::new();
    for (method, uri) in [
        (Method::GET, format!("/Playlists/{pid}")),
        (Method::GET, format!("/Playlists/{pid}/Items")),
        (Method::GET, format!("/Items/{pid}")),
        (Method::POST, format!("/Playlists/{pid}/Items?Ids={other}")),
        (
            Method::DELETE,
            format!("/Playlists/{pid}/Items?EntryIds={entry}"),
        ),
        (
            Method::POST,
            format!("/Playlists/{pid}/Items/{entry}/Move/1"),
        ),
        (Method::DELETE, format!("/Playlists/{pid}")),
    ] {
        let status = test::call_service(&app, as_user(method.clone(), uri.clone()))
            .await
            .status()
            .as_u16();
        // `/Items/{id}` answers another user's playlist as an id that names
        // nothing: 400 "invalid id" (playlist_edges_answer_like_a_missing_playlist).
        let refused = if uri.starts_with("/Items/") { 400 } else { 404 };
        if status != refused {
            answered.push(format!("{method} {uri} → {status}"));
        }
    }
    assert!(
        answered.is_empty(),
        "another user's playlist answered:\n{}",
        answered.join("\n")
    );
    assert_eq!(playlist_denied(&snapshotter, "read"), 3);
    assert_eq!(playlist_denied(&snapshotter, "write"), 4);
    let kept = items(&f, &pid).await;
    assert_eq!(kept.len(), 2, "owner's playlist changed: {kept:?}");
    assert_eq!(kept[0]["PlaylistItemId"], entry.as_str());

    let shared = f
        .state
        .stores
        .create_playlist("Shared", None, "Video", &[f.other_item_id])
        .await
        .unwrap()
        .wire_id;
    for (method, uri, want) in [
        (Method::GET, format!("/Playlists/{shared}"), 200),
        (Method::GET, format!("/Playlists/{shared}/Items"), 200),
        (
            Method::POST,
            format!("/Playlists/{shared}/Items?Ids={other}"),
            404,
        ),
        (Method::DELETE, format!("/Playlists/{shared}"), 404),
    ] {
        let status = test::call_service(&app, as_user(method.clone(), uri.clone()))
            .await
            .status()
            .as_u16();
        assert_eq!(status, want, "{method} {uri}");
    }
    assert_eq!(
        send(&f, "DELETE", &format!("/Playlists/{shared}")).await,
        204
    );
}

/// A request as `token`.
fn call(method: actix_web::http::Method, uri: &str, token: &str) -> test::TestRequest {
    test::TestRequest::default()
        .method(method)
        .uri(uri)
        .insert_header(("X-Emby-Token", token))
}

/// A playlist owned by the fixture's non-admin user, holding one item from
/// each library.
async fn users_playlist(f: &common::Fixture) -> String {
    use pharos_core::PlaylistStore as _;
    let owner = f.user_id.0.simple().to_string();
    f.state
        .stores
        .create_playlist(
            "Theirs",
            Some(&owner),
            "Video",
            &[f.rich_item_id, f.other_item_id],
        )
        .await
        .unwrap()
        .wire_id
}

/// B259: an admin does not read another user's playlist, but manages one
/// whose owner has been deleted; a non-admin may not.
#[actix_web::test]
async fn an_admin_manages_only_an_orphaned_playlist() {
    use actix_web::http::Method;
    use pharos_core::PlaylistStore as _;
    let f = seed_rich().await;
    let app = test::init_service(build_app(f.state.clone())).await;
    let theirs = users_playlist(&f).await;
    let admin = f.admin_token.as_str();
    for uri in [
        format!("/Playlists/{theirs}"),
        format!("/Playlists/{theirs}/Items"),
    ] {
        let status = test::call_service(&app, call(Method::GET, &uri, admin).to_request())
            .await
            .status();
        assert_eq!(status.as_u16(), 404, "admin read {uri}");
    }

    let gone = pharos_core::UserId::new().0.simple().to_string();
    let orphan = f
        .state
        .stores
        .create_playlist("Orphan", Some(&gone), "Video", &[])
        .await
        .unwrap()
        .wire_id;
    let uri = format!("/Playlists/{orphan}");
    let status = test::call_service(&app, call(Method::DELETE, &uri, &f.user_token).to_request())
        .await
        .status();
    assert_eq!(
        status.as_u16(),
        404,
        "a non-admin deleted an orphaned playlist"
    );
    let status = test::call_service(&app, call(Method::DELETE, &uri, admin).to_request())
        .await
        .status();
    assert_eq!(
        status.as_u16(),
        204,
        "an admin could not delete an orphaned playlist"
    );
}

/// B259: `/Items/{id}` answers another user's playlist exactly as an id that
/// names nothing, so it does not confirm the playlist exists.
#[actix_web::test]
async fn items_by_id_answers_another_users_playlist_as_nothing() {
    use actix_web::http::Method;
    let f = seed_rich().await;
    let app = test::init_service(build_app(f.state.clone())).await;
    let theirs = users_playlist(&f).await;
    let missing = "0123456789abcdef0123456789abcdef";
    let mut answers = Vec::new();
    for id in [theirs.as_str(), missing] {
        let resp = test::call_service(
            &app,
            call(Method::GET, &format!("/Items/{id}"), &f.admin_token).to_request(),
        )
        .await;
        let status = resp.status().as_u16();
        answers.push((status, test::read_body(resp).await));
    }
    assert_eq!(
        answers[0], answers[1],
        "/Items/{{id}} tells another's playlist from nothing"
    );
}

/// V197: a playlist's `ChildCount` counts only the entries its reader may
/// open, on the header and on `/Items/{id}`.
#[actix_web::test]
async fn a_playlists_child_count_is_what_its_reader_may_open() {
    use actix_web::http::Method;
    use pharos_core::{UserPolicy, UserStore as _};
    let f = seed_rich().await;
    let app = test::init_service(build_app(f.state.clone())).await;
    let theirs = users_playlist(&f).await;
    f.state
        .stores
        .set_policy(
            f.user_id,
            UserPolicy {
                enable_all_folders: false,
                enabled_folders: vec![f.lib_b_wire.clone()],
                ..UserPolicy::default()
            },
        )
        .await
        .unwrap();
    for uri in [format!("/Playlists/{theirs}"), format!("/Items/{theirs}")] {
        let resp =
            test::call_service(&app, call(Method::GET, &uri, &f.user_token).to_request()).await;
        let body: Value = test::read_body_json(resp).await;
        assert_eq!(body["ChildCount"], 1, "{uri}: {body}");
    }
}
