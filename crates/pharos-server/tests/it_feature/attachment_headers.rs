//! `X-Content-Type-Options: nosniff` on every response (V194).

use actix_web::{http::header, test};

use crate::common::{build_app, seed_rich};

fn header_of(resp: &actix_web::dev::ServiceResponse, name: header::HeaderName) -> Option<String> {
    resp.headers()
        .get(name)
        .map(|v| v.to_str().unwrap().to_owned())
}

/// Every route answers `nosniff`, byte routes and JSON alike, errors
/// included. Disarm check: with `nosniff()` unwrapped from the app, this
/// fails.
#[actix_web::test]
async fn every_response_is_nosniff() {
    let f = seed_rich().await;
    let id = f.rich_item_id;
    let app = test::init_service(build_app(f.state.clone())).await;
    for uri in [
        "/Users/Me".to_owned(),
        format!("/Items/{id}/Images"),
        "/Items/999999999/Images/Primary".to_owned(),
        format!("/Videos/{id}/Subtitles/0/Stream.vtt"),
    ] {
        let req = test::TestRequest::get()
            .uri(&uri)
            .insert_header(("X-Emby-Token", f.admin_token.as_str()))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(
            header_of(&resp, header::X_CONTENT_TYPE_OPTIONS).as_deref(),
            Some("nosniff"),
            "{uri}: status {}",
            resp.status()
        );
    }
}
