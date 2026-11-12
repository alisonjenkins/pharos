//! `Timing-Allow-Origin: *` on artwork responses.
//!
//! The TV app's page and the web UI on another host load posters
//! cross-origin. Without this header the browser zeroes every phase of their
//! Resource Timing entries (connect, request, response), leaving only a
//! total, so a slow poster cannot be split into queueing, network and server
//! time (B1368, V1338). Artwork is served without a token (keyed tags,
//! ADR-0073), so an anonymous request's timings tell a page nothing it could
//! not measure by loading the image itself. A request carrying a credential
//! (a cookie, an auth header or `api_key`) is left without it: a signed-in
//! viewer's artwork answer depends on what they may see, and a same-site
//! sibling page sending their cookie must not read its size or timing
//! (B1371). Media, playlists and API replies never get it.

use actix_web::{
    body::MessageBody,
    dev::{ServiceRequest, ServiceResponse},
    http::header::{HeaderName, HeaderValue},
    middleware::Next,
    Error,
};

/// The header's name; `http` has no constant for it.
pub const TIMING_ALLOW_ORIGIN: &str = "timing-allow-origin";

/// Whether `path` is an item's or a user's artwork (`/Items/{id}/Images/...`,
/// `/Users/{id}/Images/...`), in any letter case. Other paths that happen to
/// contain `/Images/` (a subtitle's picture, a remote-image search) are not.
pub fn is_artwork(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let mut parts = lower.trim_start_matches('/').split('/');
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some("items" | "users"), Some(id), Some("images")) if !id.is_empty()
    )
}

/// Whether the request could be answered differently for a signed-in viewer.
fn carries_credential(req: &ServiceRequest) -> bool {
    req.headers().contains_key(actix_web::http::header::COOKIE)
        || crate::api::jellyfin::auth_extractor::extract_token(req.request()).is_some()
}

/// Sets the header on an anonymous artwork response that has not set it itself.
pub async fn timing_allow_origin(
    req: ServiceRequest,
    next: Next<impl MessageBody>,
) -> Result<ServiceResponse<impl MessageBody>, Error> {
    let fetch = matches!(
        *req.method(),
        actix_web::http::Method::GET | actix_web::http::Method::HEAD
    );
    let allow = fetch && is_artwork(req.path()) && !carries_credential(&req);
    let mut res = next.call(req).await?;
    let name = HeaderName::from_static(TIMING_ALLOW_ORIGIN);
    if allow && !res.headers().contains_key(&name) {
        res.headers_mut()
            .insert(name, HeaderValue::from_static("*"));
    }
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{middleware::from_fn, test, web, App, HttpResponse};

    /// B1368, V1338: artwork replies (a 404 included) allow timing to any
    /// origin; a stream or API reply does not.
    #[actix_web::test]
    async fn only_artwork_allows_timing_to_any_origin() {
        let app = test::init_service(
            App::new()
                .wrap(from_fn(timing_allow_origin))
                .route(
                    "/Items/{id}/Images/Primary",
                    web::get().to(HttpResponse::Ok),
                )
                .route("/videos/{id}/main.m3u8", web::get().to(HttpResponse::Ok)),
        )
        .await;
        for (uri, status, allowed) in [
            ("/Items/1/Images/Primary?tag=a", 200, true),
            ("/items/1/images/missing", 404, true),
            ("/videos/1/main.m3u8", 200, false),
            ("/System/Info", 404, false),
            ("/Videos/1/Subtitles/2/Images/3", 404, false),
            ("/Items/1/RemoteImages", 404, false),
        ] {
            let resp =
                test::call_service(&app, test::TestRequest::get().uri(uri).to_request()).await;
            assert_eq!(resp.status(), status, "{uri}");
            assert_eq!(
                resp.headers()
                    .get(TIMING_ALLOW_ORIGIN)
                    .map(|v| v.as_bytes()),
                allowed.then_some(&b"*"[..]),
                "{uri}"
            );
        }
    }

    /// B1371: an artwork request that carries a credential gets no header,
    /// since its answer can depend on who is signed in.
    #[actix_web::test]
    async fn credentialed_artwork_does_not_allow_timing() {
        let app = test::init_service(App::new().wrap(from_fn(timing_allow_origin)).route(
            "/Items/{id}/Images/Primary",
            web::get().to(HttpResponse::Ok),
        ))
        .await;
        let uri = "/Items/1/Images/Primary";
        let requests = [
            (
                "cookie",
                test::TestRequest::get()
                    .uri(uri)
                    .insert_header(("Cookie", "JellyfinAuth=t")),
            ),
            (
                "token header",
                test::TestRequest::get()
                    .uri(uri)
                    .insert_header(("X-Emby-Token", "t")),
            ),
            (
                "api_key",
                test::TestRequest::get().uri("/Items/1/Images/Primary?api_key=t"),
            ),
        ];
        for (what, req) in requests {
            let resp = test::call_service(&app, req.to_request()).await;
            assert_eq!(resp.status(), 200, "{what}");
            assert!(resp.headers().get(TIMING_ALLOW_ORIGIN).is_none(), "{what}");
        }
    }

    /// B1371: only a fetch of the artwork is allowed timing, not another
    /// method on the same path (a CORS preflight, an upload).
    #[actix_web::test]
    async fn only_a_get_or_head_of_artwork_allows_timing() {
        let app = test::init_service(
            App::new()
                .wrap(from_fn(timing_allow_origin))
                .route("/Items/{id}/Images/Primary", web::to(HttpResponse::Ok)),
        )
        .await;
        for (method, allowed) in [
            (actix_web::http::Method::GET, true),
            (actix_web::http::Method::HEAD, true),
            (actix_web::http::Method::OPTIONS, false),
            (actix_web::http::Method::POST, false),
        ] {
            let req = test::TestRequest::default()
                .method(method.clone())
                .uri("/Items/1/Images/Primary")
                .to_request();
            let resp = test::call_service(&app, req).await;
            assert_eq!(
                resp.headers().get(TIMING_ALLOW_ORIGIN).is_some(),
                allowed,
                "{method}"
            );
        }
    }
}
