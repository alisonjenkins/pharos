//! Feature inventory — **item metadata richness** (gap A, backlog T67).
//!
//! jellyfin-web renders cast/crew, studios, tags, external links and the
//! metadata editor from item payloads. pharos enriches people/studios/tags
//! only on the single-item **detail** handler; **list** responses ship them
//! empty, and `external_urls` / `remote_trailers` / `production_locations`
//! plus `GET /Items/{id}/MetadataEditor` are absent.
//!
//! The `#[ignore]`d tests are the T67 backlog: enabling one and turning it
//! green is the task. The live tests pin what already works so a future
//! change can't silently regress it. Assertions are on the Jellyfin wire
//! JSON only.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::test;
use serde_json::Value;

mod common;
use common::{build_app, seed_rich};

/// Fetch `/Users/{uid}/Items?…` as the admin and return the parsed body.
async fn list_items(f: &common::Fixture, query: &str) -> Value {
    let uid = f.admin_id.0.simple().to_string();
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::get()
        .uri(&format!("/Users/{uid}/Items?{query}"))
        .insert_header(("X-Emby-Token", f.admin_token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    serde_json::from_slice(&body).unwrap()
}

/// Find the rich item in an ItemsResult body by its Name.
fn rich_row(items: &Value) -> &Value {
    items["Items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|it| it["Name"] == "Rich Movie")
        .expect("rich item present in list")
}

/// GET the rich item's detail payload (id resolved from the list row so the
/// test is agnostic to the wire-id encoding).
async fn rich_detail(f: &common::Fixture) -> Value {
    let list = list_items(f, "IncludeItemTypes=Movie&Recursive=true").await;
    let id = rich_row(&list)["Id"].as_str().unwrap().to_string();
    let uid = f.admin_id.0.simple().to_string();
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::get()
        .uri(&format!("/Users/{uid}/Items/{id}"))
        .insert_header(("X-Emby-Token", f.admin_token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    serde_json::from_slice(&body).unwrap()
}

// ---- live: confirmed-working surfaces (guard against regression) ----

#[actix_web::test]
async fn item_detail_enriches_people_studios_tags() {
    let f = seed_rich().await;
    let d = rich_detail(&f).await;
    assert!(
        !d["People"].as_array().unwrap().is_empty(),
        "detail should carry linked cast/crew"
    );
    assert!(
        !d["Studios"].as_array().unwrap().is_empty(),
        "detail should carry linked studios"
    );
    assert!(
        !d["Tags"].as_array().unwrap().is_empty(),
        "detail should carry linked tags"
    );
}

#[actix_web::test]
async fn list_and_detail_populate_provider_ids() {
    let f = seed_rich().await;
    let list = list_items(
        &f,
        "IncludeItemTypes=Movie&Recursive=true&Fields=ProviderIds",
    )
    .await;
    assert_eq!(rich_row(&list)["ProviderIds"]["Imdb"], "tt1234567");
    let d = rich_detail(&f).await;
    assert_eq!(d["ProviderIds"]["Imdb"], "tt1234567");
}

// ---- backlog T67: enrich list responses + metadata-editor endpoint ----

#[actix_web::test]
#[ignore = "gap: list responses omit People even with Fields=People (T67)"]
async fn list_items_populate_people_when_requested() {
    let f = seed_rich().await;
    let list = list_items(&f, "IncludeItemTypes=Movie&Recursive=true&Fields=People").await;
    assert!(
        !rich_row(&list)["People"].as_array().unwrap().is_empty(),
        "list rows should carry People when Fields=People is requested"
    );
}

#[actix_web::test]
#[ignore = "gap: list responses omit Studios/Tags even when requested (T67)"]
async fn list_items_populate_studios_and_tags() {
    let f = seed_rich().await;
    let list = list_items(
        &f,
        "IncludeItemTypes=Movie&Recursive=true&Fields=Studios,Tags",
    )
    .await;
    let row = rich_row(&list);
    assert!(
        !row["Studios"].as_array().unwrap().is_empty(),
        "Studios on list"
    );
    assert!(!row["Tags"].as_array().unwrap().is_empty(), "Tags on list");
}

#[actix_web::test]
async fn item_external_urls_populated() {
    let f = seed_rich().await;
    let d = rich_detail(&f).await;
    assert!(
        !d["ExternalUrls"].as_array().unwrap().is_empty(),
        "an item with provider ids should expose ExternalUrls"
    );
}

#[actix_web::test]
#[ignore = "gap: RemoteTrailers never populated (T67)"]
async fn item_remote_trailers_populated() {
    let f = seed_rich().await;
    let d = rich_detail(&f).await;
    assert!(
        !d["RemoteTrailers"].as_array().unwrap().is_empty(),
        "expected at least one remote trailer"
    );
}

#[actix_web::test]
#[ignore = "gap: ProductionLocations never populated (T67)"]
async fn item_production_locations_populated() {
    let f = seed_rich().await;
    let d = rich_detail(&f).await;
    assert!(
        !d["ProductionLocations"].as_array().unwrap().is_empty(),
        "expected production locations"
    );
}

#[actix_web::test]
#[ignore = "gap: GET /Items/{id}/MetadataEditor endpoint absent (T67)"]
async fn metadata_editor_endpoint_returns_cultures_and_external_ids() {
    let f = seed_rich().await;
    let list = list_items(&f, "IncludeItemTypes=Movie&Recursive=true").await;
    let id = rich_row(&list)["Id"].as_str().unwrap().to_string();
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::get()
        .uri(&format!("/Items/{id}/MetadataEditor"))
        .insert_header(("X-Emby-Token", f.admin_token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200, "MetadataEditor should resolve");
    let body = test::read_body(resp).await;
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert!(v.get("Cultures").is_some(), "bundles Cultures");
    assert!(
        v.get("ExternalIdInfos").is_some(),
        "bundles ExternalIdInfos"
    );
}
