//! `Timing-Allow-Origin: *` on every response.
//!
//! The TV app's page and the web UI on another host load posters, playlists
//! and segments cross-origin. Without this header the browser zeroes every
//! phase of their Resource Timing entries (DNS, connect, request, response),
//! leaving only a total, so a slow poster cannot be split into queueing,
//! network and server time (B1368, V1338). CORS already lets any origin read
//! these bodies, so the timings disclose nothing a page could not measure.

use actix_web::middleware::DefaultHeaders;

/// The header's name; `http` has no constant for it.
pub const TIMING_ALLOW_ORIGIN: &str = "timing-allow-origin";

/// Sets the header on any response that has not set it itself.
pub fn timing_allow_origin() -> DefaultHeaders {
    DefaultHeaders::new().add((TIMING_ALLOW_ORIGIN, "*"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{test, web, App, HttpResponse};

    /// B1368, V1338: a served reply and a 404 both carry the header.
    #[actix_web::test]
    async fn every_reply_allows_timing_to_any_origin() {
        let app = test::init_service(
            App::new()
                .wrap(timing_allow_origin())
                .route("/img", web::get().to(HttpResponse::Ok)),
        )
        .await;
        for (uri, status) in [("/img", 200), ("/missing", 404)] {
            let resp =
                test::call_service(&app, test::TestRequest::get().uri(uri).to_request()).await;
            assert_eq!(resp.status(), status, "{uri}");
            assert_eq!(
                resp.headers()
                    .get(TIMING_ALLOW_ORIGIN)
                    .map(|v| v.as_bytes()),
                Some(&b"*"[..]),
                "{uri}"
            );
        }
    }
}
