#![allow(clippy::unwrap_used, clippy::expect_used)]
//! DLNA / UPnP HTTP surface (T48 phase 1).

use actix_web::{test, web, App};
use pharos_core::{MediaItem, MediaKind, MediaStore};
use pharos_server::{dlna, middleware::LowercasePath, state::AppState};
use pharos_store_sqlx::sqlite::SqliteStore;

async fn seed_state() -> (web::Data<AppState>, String) {
    let stores = SqliteStore::connect("sqlite::memory:").await.unwrap();
    stores
        .put(MediaItem {
            id: 1,
            path: "/m/1.webm".into(),
            title: "Movie One".into(),
            kind: MediaKind::Movie,
            ..Default::default()
        })
        .await
        .unwrap();
    stores
        .put(MediaItem {
            id: 2,
            path: "/m/2.mp3".into(),
            title: "Track Two".into(),
            kind: MediaKind::Audio,
            ..Default::default()
        })
        .await
        .unwrap();
    let state = AppState::new(stores, "pharos-dlna-test".into());
    let id = state.server_id.clone();
    (web::Data::new(state), id)
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
        .configure(dlna::register)
}

#[actix_web::test]
async fn description_xml_contains_server_id_and_services() {
    let (state, id) = seed_state().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri(&format!("/Dlna/{id}/description.xml"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let ct = resp
        .headers()
        .get(actix_web::http::header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(ct.contains("text/xml"), "ct={ct}");
    let body = test::read_body(resp).await;
    let s = std::str::from_utf8(&body).unwrap();
    assert!(s.contains(&format!("uuid:{id}")), "body missing UDN: {s}");
    assert!(s.contains("MediaServer:1"));
    assert!(s.contains("pharos-dlna-test"));
    assert!(s.contains("/ContentDirectory/control"));
}

#[actix_web::test]
async fn description_xml_404s_on_wrong_server_id() {
    let (state, _id) = seed_state().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Dlna/wrong-id/description.xml")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 404);
}

#[actix_web::test]
async fn content_directory_scpd_renders_xml() {
    let (state, id) = seed_state().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri(&format!("/Dlna/{id}/ContentDirectory/scpd.xml"))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let s = std::str::from_utf8(&body).unwrap();
    assert!(s.contains("<name>Browse</name>"));
    assert!(s.contains("urn:schemas-upnp-org:service-1-0"));
}

#[actix_web::test]
async fn browse_soap_returns_didl_with_library_items() {
    let (state, id) = seed_state().await;
    let app = test::init_service(build_app(state)).await;
    let soap_body = r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body>
    <u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">
      <ObjectID>0</ObjectID>
      <BrowseFlag>BrowseDirectChildren</BrowseFlag>
      <Filter>*</Filter>
      <StartingIndex>0</StartingIndex>
      <RequestedCount>100</RequestedCount>
      <SortCriteria></SortCriteria>
    </u:Browse>
  </s:Body>
</s:Envelope>"#;
    let req = test::TestRequest::post()
        .uri(&format!("/Dlna/{id}/ContentDirectory/control"))
        .insert_header((
            "SOAPACTION",
            "\"urn:schemas-upnp-org:service:ContentDirectory:1#Browse\"",
        ))
        .insert_header((actix_web::http::header::CONTENT_TYPE, "text/xml"))
        .set_payload(soap_body)
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let s = std::str::from_utf8(&body).unwrap();
    assert!(s.contains("<NumberReturned>2</NumberReturned>"));
    // Both items present (escaped — see dlna::tests for exact byte
    // expectations).
    assert!(s.contains("Movie One"));
    assert!(s.contains("Track Two"));
}

#[actix_web::test]
async fn browse_without_soapaction_is_400() {
    let (state, id) = seed_state().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::post()
        .uri(&format!("/Dlna/{id}/ContentDirectory/control"))
        .set_payload("<x/>")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 400);
}

#[actix_web::test]
async fn lowercase_alias_resolves_via_middleware() {
    // jellyfin-web / generic UPnP clients sometimes send lowercase
    // paths. The LowercasePath middleware folds them.
    let (state, id) = seed_state().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri(&format!("/dlna/{id}/description.xml"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
}
