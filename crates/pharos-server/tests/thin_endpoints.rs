//! Coverage for THIN → real endpoints: item metadata refresh + device
//! management (delete / options custom-name round-trip). Drives the real HTTP
//! surface through the shared fixture.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::test;
use serde_json::Value;

mod common;
use common::{build_app, seed_rich};

async fn send(f: &common::Fixture, method: &str, uri: &str, token: &str) -> u16 {
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = match method {
        "POST" => test::TestRequest::post(),
        "DELETE" => test::TestRequest::delete(),
        _ => test::TestRequest::get(),
    }
    .uri(uri)
    .insert_header(("X-Emby-Token", token))
    .to_request();
    test::call_service(&app, req).await.status().as_u16()
}

#[actix_web::test]
async fn refresh_item_admin_only() {
    let f = seed_rich().await;
    // Admin → 204 (kicks a background re-probe of the item's dir).
    assert_eq!(
        send(
            &f,
            "POST",
            &format!("/Items/{}/Refresh", f.rich_item_id),
            f.admin_token.as_str()
        )
        .await,
        204
    );
    // Non-admin → 403.
    assert_eq!(
        send(
            &f,
            "POST",
            &format!("/Items/{}/Refresh", f.rich_item_id),
            f.user_token.as_str()
        )
        .await,
        403
    );
    // Unknown item → 404.
    assert_eq!(
        send(&f, "POST", "/Items/999999/Refresh", f.admin_token.as_str()).await,
        404
    );
}

#[actix_web::test]
async fn device_options_custom_name_roundtrips() {
    let f = seed_rich().await;
    let app = test::init_service(build_app(f.state.clone())).await;

    // Set a custom name for the fixture's "test" device.
    let post = test::TestRequest::post()
        .uri("/Devices/Options?id=test")
        .insert_header(("X-Emby-Token", f.admin_token.as_str()))
        .set_json(serde_json::json!({ "CustomName": "Living Room TV" }))
        .to_request();
    assert_eq!(test::call_service(&app, post).await.status(), 204);

    // It shows up in the devices list.
    let get = test::TestRequest::get()
        .uri("/Devices")
        .insert_header(("X-Emby-Token", f.admin_token.as_str()))
        .to_request();
    let v: Value = serde_json::from_slice(&test::call_and_read_body(&app, get).await).unwrap();
    let named = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|d| d["Id"] == "test" && d["Name"] == "Living Room TV");
    assert!(named, "custom device name should appear in the list: {v}");
}

#[actix_web::test]
async fn delete_device_revokes_and_is_admin_only() {
    let f = seed_rich().await;
    // Non-admin → 403.
    assert_eq!(
        send(&f, "DELETE", "/Devices?id=test", f.user_token.as_str()).await,
        403
    );
    // Admin → 204.
    assert_eq!(
        send(&f, "DELETE", "/Devices?id=test", f.admin_token.as_str()).await,
        204
    );
}

#[actix_web::test]
async fn delete_device_accepts_repeated_ids() {
    use pharos_core::{TokenStore, UserStore};
    let f = seed_rich().await;
    let uid = f.state.stores.list().await.unwrap()[0].id;
    let a = f.state.stores.issue(uid, "dev-a").await.unwrap();
    let b = f.state.stores.issue(uid, "dev-b").await.unwrap();
    let c = f.state.stores.issue(uid, "dev-c").await.unwrap();
    let (a, b, c) = (
        a.0.expose().to_string(),
        b.0.expose().to_string(),
        c.0.expose().to_string(),
    );
    for t in [&a, &b, &c] {
        assert_eq!(send(&f, "GET", "/System/Info", t).await, 200);
    }
    assert_eq!(
        send(
            &f,
            "DELETE",
            "/Devices?id=dev-a&id=dev-b",
            f.admin_token.as_str()
        )
        .await,
        204
    );
    assert_eq!(send(&f, "GET", "/System/Info", &a).await, 401);
    assert_eq!(send(&f, "GET", "/System/Info", &b).await, 401);
    assert_eq!(send(&f, "GET", "/System/Info", &c).await, 200);
}
