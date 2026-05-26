//! Jellyfin /Items/{id}/Images/* stubs. T19 phase 1 ships 404 responses
//! so clients gracefully fall back to default posters instead of hammering
//! a 5xx loop. Real thumbnail generation (extract poster via ffmpeg + cache)
//! lands with T19 phase 2.
//!
//! Endpoints are intentionally **unauthenticated** to match Jellyfin's
//! reference behaviour — image URLs are passed around in `<img src=…>`
//! tags where header auth isn't an option. The downstream concern (V9 —
//! no media-path leak) is moot here: 404 carries no path.

use crate::state::AppState;
use actix_web::{web, HttpResponse};
use pharos_core::MediaStore;
use serde::Deserialize;

pub fn register(cfg: &mut web::ServiceConfig) {
    // T31: lowercase canonical paths. The `image_type` path param is
    // therefore also lowercased by `LowercasePath` — `is_known_image_type`
    // + the Primary-only fast-path use case-insensitive comparison
    // against Jellyfin's PascalCase ImageType enum.
    cfg.route("/items/{id}/images/{image_type}", web::get().to(get_image))
        .route("/items/{id}/images/{image_type}", web::head().to(head_image))
        .route(
            "/items/{id}/images/{image_type}/{image_index}",
            web::get().to(get_image_indexed),
        )
        .route(
            "/items/{id}/images/{image_type}/{image_index}",
            web::head().to(head_image_indexed),
        );
}

#[derive(Debug, Deserialize)]
struct ImagePath {
    id: String,
    image_type: String,
}

#[derive(Debug, Deserialize)]
struct IndexedImagePath {
    id: String,
    image_type: String,
    #[allow(dead_code)]
    image_index: u32,
}

async fn get_image(
    state: web::Data<AppState>,
    path: web::Path<ImagePath>,
) -> Result<HttpResponse, actix_web::Error> {
    serve_image(&state, &path.id, &path.image_type, false).await
}

async fn head_image(
    state: web::Data<AppState>,
    path: web::Path<ImagePath>,
) -> Result<HttpResponse, actix_web::Error> {
    serve_image(&state, &path.id, &path.image_type, true).await
}

async fn get_image_indexed(
    state: web::Data<AppState>,
    path: web::Path<IndexedImagePath>,
) -> Result<HttpResponse, actix_web::Error> {
    serve_image(&state, &path.id, &path.image_type, false).await
}

async fn head_image_indexed(
    state: web::Data<AppState>,
    path: web::Path<IndexedImagePath>,
) -> Result<HttpResponse, actix_web::Error> {
    serve_image(&state, &path.id, &path.image_type, true).await
}

async fn serve_image(
    state: &AppState,
    id_str: &str,
    image_type: &str,
    head_only: bool,
) -> Result<HttpResponse, actix_web::Error> {
    if !is_known_image_type(image_type) {
        return Ok(HttpResponse::BadRequest().body("unknown image type"));
    }
    // Case-insensitive: the URI is folded to lowercase by
    // `LowercasePath` before routing, so `image_type` arrives lowercased.
    if !image_type.eq_ignore_ascii_case("Primary") {
        // Backdrop / Thumb / etc. still phase 3.
        return Ok(HttpResponse::NotFound().body(""));
    }
    let Some(cache) = state.images.as_ref() else {
        return Ok(HttpResponse::NotFound().body(""));
    };
    let id: u64 = match id_str.parse() {
        Ok(id) => id,
        Err(_) => return Ok(HttpResponse::NotFound().body("")),
    };
    let item = match state.stores.get(id).await {
        Ok(it) => it,
        Err(_) => return Ok(HttpResponse::NotFound().body("")),
    };
    let path = match cache.primary(id, item.kind, &item.path).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "image extraction failed");
            return Ok(HttpResponse::NotFound().body(""));
        }
    };
    if head_only {
        return Ok(HttpResponse::Ok().content_type("image/jpeg").finish());
    }
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(_) => return Ok(HttpResponse::NotFound().body("")),
    };
    Ok(HttpResponse::Ok().content_type("image/jpeg").body(bytes))
}

/// Jellyfin's `ImageType` enum values. Match here so unknown types
/// surface as 400 instead of generic 404 — eases client-side debugging.
fn is_known_image_type(s: &str) -> bool {
    [
        "primary",
        "backdrop",
        "logo",
        "thumb",
        "art",
        "banner",
        "disc",
        "box",
        "screenshot",
        "menu",
        "chapter",
        "boxrear",
        "profile",
    ]
    .iter()
    .any(|known| s.eq_ignore_ascii_case(known))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use actix_web::{test, App};
    use pharos_store_sqlx::sqlite::SqliteStore;

    async fn seed_state() -> web::Data<crate::state::AppState> {
        let stores = SqliteStore::connect("sqlite::memory:").await.unwrap();
        web::Data::new(crate::state::AppState::new(stores, "t".into()))
    }

    #[actix_web::test]
    async fn known_type_returns_404_not_500() {
        let state = seed_state().await;
        let app =
            test::init_service(App::new().app_data(state).configure(register)).await;
        for t in ["primary", "backdrop", "thumb"] {
            let req = test::TestRequest::get()
                .uri(&format!("/items/abc/images/{t}"))
                .to_request();
            let resp = test::call_service(&app, req).await;
            assert_eq!(resp.status(), 404, "type={t}");
        }
    }

    #[actix_web::test]
    async fn indexed_route_404s() {
        let state = seed_state().await;
        let app =
            test::init_service(App::new().app_data(state).configure(register)).await;
        let req = test::TestRequest::get()
            .uri("/items/abc/images/backdrop/0")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 404);
    }

    #[actix_web::test]
    async fn unknown_type_returns_400() {
        let state = seed_state().await;
        let app =
            test::init_service(App::new().app_data(state).configure(register)).await;
        let req = test::TestRequest::get()
            .uri("/items/abc/images/bogus")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 400);
    }

    #[actix_web::test]
    async fn head_request_returns_no_body_404() {
        let state = seed_state().await;
        let app =
            test::init_service(App::new().app_data(state).configure(register)).await;
        let req = test::TestRequest::default()
            .method(actix_web::http::Method::HEAD)
            .uri("/items/abc/images/primary")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 404);
    }

    #[actix_web::test]
    async fn images_endpoint_is_public() {
        // Important: Jellyfin clients embed image URLs in <img src=…>
        // tags. They cannot inject auth headers and the api_key query
        // param is not always available. Endpoint must respond to
        // unauthenticated GETs (whether 404 or eventually 200).
        let state = seed_state().await;
        let app =
            test::init_service(App::new().app_data(state).configure(register)).await;
        let req = test::TestRequest::get()
            .uri("/items/abc/images/primary")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_ne!(resp.status(), 401);
    }
}
