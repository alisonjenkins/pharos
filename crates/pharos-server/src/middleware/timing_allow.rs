//! `Timing-Allow-Origin: *` on artwork responses.
//!
//! The TV app's page and the web UI on another host load posters
//! cross-origin. Without this header the browser zeroes every phase of their
//! Resource Timing entries (connect, request, response), leaving only a
//! total, so a slow poster cannot be split into queueing, network and server
//! time (B1368, V1338). Artwork is served without a token (keyed tags,
//! ADR-0073), so its timings tell a page nothing it could not measure by
//! loading the image itself. Media, playlists and API replies, which can ride
//! a cookie, are left without it: a same-site page must not learn their
//! sizes and timings.

use actix_web::{
    body::MessageBody,
    dev::{ServiceRequest, ServiceResponse},
    http::header::{HeaderName, HeaderValue},
    middleware::Next,
    Error,
};

/// The header's name; `http` has no constant for it.
pub const TIMING_ALLOW_ORIGIN: &str = "timing-allow-origin";

/// Whether `path` is an artwork route (`/Items/{id}/Images/...`,
/// `/Users/{id}/Images/...` and the like), in any letter case.
pub fn is_artwork(path: &str) -> bool {
    path.to_ascii_lowercase().contains("/images/")
}

/// Sets the header on an artwork response that has not set it itself.
pub async fn timing_allow_origin(
    req: ServiceRequest,
    next: Next<impl MessageBody>,
) -> Result<ServiceResponse<impl MessageBody>, Error> {
    let artwork = is_artwork(req.path());
    let mut res = next.call(req).await?;
    let name = HeaderName::from_static(TIMING_ALLOW_ORIGIN);
    if artwork && !res.headers().contains_key(&name) {
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
}
