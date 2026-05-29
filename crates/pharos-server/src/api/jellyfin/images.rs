//! Jellyfin `/Items/{id}/Images/*` — Primary on demand (extracted via
//! ffmpeg), Backdrop / Thumb extracted at a deeper / wider point;
//! Logo / Banner / Art / Disc are upload-only.
//!
//! GET endpoints are intentionally **unauthenticated** to match
//! Jellyfin's reference behaviour — image URLs are passed around in
//! `<img src=…>` tags where header auth isn't an option. POST/DELETE
//! (admin uploads) **do** require an admin bearer (V8/V9).

use crate::{
    api::jellyfin::auth_extractor::AuthUser,
    image_cache::{ImageCacheError, ImageRole},
    state::AppState,
};
use actix_web::{error, web, HttpResponse};
use pharos_core::MediaStore;
use serde::Deserialize;

pub fn register(cfg: &mut web::ServiceConfig) {
    // T31: lowercase canonical paths. The `image_type` path param is
    // therefore also lowercased by `LowercasePath` — `ImageRole::from_str_ci`
    // accepts both forms anyway.
    cfg.route("/items/{id}/images/{image_type}", web::get().to(get_image))
        .route(
            "/items/{id}/images/{image_type}",
            web::head().to(head_image),
        )
        .route(
            "/items/{id}/images/{image_type}",
            web::post().to(post_image),
        )
        .route(
            "/items/{id}/images/{image_type}",
            web::delete().to(delete_image),
        )
        .route(
            "/items/{id}/images/{image_type}/{image_index}",
            web::get().to(get_image_indexed),
        )
        .route(
            "/items/{id}/images/{image_type}/{image_index}",
            web::head().to(head_image_indexed),
        )
        .route(
            "/items/{id}/images/{image_type}/{image_index}",
            web::post().to(post_image_indexed),
        )
        .route(
            "/items/{id}/images/{image_type}/{image_index}",
            web::delete().to(delete_image_indexed),
        )
        // P32 — chapter image thumbnails. Same shape as the indexed
        // image-type route but dispatches to `ImageCache::chapter`
        // which seeks ffmpeg to the chapter's start_ms.
        .route(
            "/items/{id}/images/chapter/{image_index}",
            web::get().to(get_chapter_image),
        );
}

async fn get_chapter_image(
    state: web::Data<AppState>,
    path: web::Path<(String, u32)>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id_str, idx) = path.into_inner();
    let id: u64 = id_str
        .parse()
        .map_err(|_| error::ErrorBadRequest("invalid id"))?;
    let item = state.stores.get(id).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("not found"),
        other => error::ErrorInternalServerError(other.to_string()),
    })?;
    let chapter = item
        .probe
        .chapters
        .get(idx as usize)
        .ok_or_else(|| error::ErrorNotFound("chapter index out of range"))?;
    let cache = state
        .images
        .as_ref()
        .ok_or_else(|| error::ErrorNotFound("image cache not configured"))?;
    let path = cache
        .chapter(item.id, &item.path, idx, chapter.start_ms)
        .await
        .map_err(|e| error::ErrorInternalServerError(format!("chapter image: {e}")))?;
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|e| error::ErrorInternalServerError(format!("read chapter image: {e}")))?;
    Ok(HttpResponse::Ok().content_type("image/jpeg").body(bytes))
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
    image_index: u32,
}

async fn get_image(
    state: web::Data<AppState>,
    path: web::Path<ImagePath>,
) -> Result<HttpResponse, actix_web::Error> {
    serve_image(&state, &path.id, &path.image_type, 0, false).await
}

async fn head_image(
    state: web::Data<AppState>,
    path: web::Path<ImagePath>,
) -> Result<HttpResponse, actix_web::Error> {
    serve_image(&state, &path.id, &path.image_type, 0, true).await
}

async fn get_image_indexed(
    state: web::Data<AppState>,
    path: web::Path<IndexedImagePath>,
) -> Result<HttpResponse, actix_web::Error> {
    serve_image(&state, &path.id, &path.image_type, path.image_index, false).await
}

async fn head_image_indexed(
    state: web::Data<AppState>,
    path: web::Path<IndexedImagePath>,
) -> Result<HttpResponse, actix_web::Error> {
    serve_image(&state, &path.id, &path.image_type, path.image_index, true).await
}

async fn post_image(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<ImagePath>,
    body: web::Bytes,
) -> Result<HttpResponse, actix_web::Error> {
    upload_image(&state, &user, &path.id, &path.image_type, 0, &body).await
}

async fn post_image_indexed(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<IndexedImagePath>,
    body: web::Bytes,
) -> Result<HttpResponse, actix_web::Error> {
    upload_image(
        &state,
        &user,
        &path.id,
        &path.image_type,
        path.image_index,
        &body,
    )
    .await
}

async fn delete_image(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<ImagePath>,
) -> Result<HttpResponse, actix_web::Error> {
    remove_image(&state, &user, &path.id, &path.image_type, 0).await
}

async fn delete_image_indexed(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<IndexedImagePath>,
) -> Result<HttpResponse, actix_web::Error> {
    remove_image(&state, &user, &path.id, &path.image_type, path.image_index).await
}

async fn serve_image(
    state: &AppState,
    id_str: &str,
    image_type: &str,
    index: u32,
    head_only: bool,
) -> Result<HttpResponse, actix_web::Error> {
    let Some(role) = ImageRole::from_str_ci(image_type) else {
        return Ok(HttpResponse::BadRequest().body("unknown image type"));
    };
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
    let path = match cache.fetch(id, role, item.kind, &item.path, index).await {
        Ok(p) => p,
        // Upload-only roles (Logo/Banner/Art/Disc) report
        // `UploadOnly` when no upload has happened — surface as 404
        // for the read endpoint, same as a missing file.
        Err(ImageCacheError::UploadOnly) => return Ok(HttpResponse::NotFound().body("")),
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

async fn upload_image(
    state: &AppState,
    user: &AuthUser,
    id_str: &str,
    image_type: &str,
    index: u32,
    body: &[u8],
) -> Result<HttpResponse, actix_web::Error> {
    if !user.0.policy.admin {
        return Err(error::ErrorForbidden("admin required"));
    }
    let Some(role) = ImageRole::from_str_ci(image_type) else {
        return Err(error::ErrorBadRequest("unknown image type"));
    };
    let Some(cache) = state.images.as_ref() else {
        return Err(error::ErrorInternalServerError(
            "image cache not configured",
        ));
    };
    let id: u64 = id_str
        .parse()
        .map_err(|_| error::ErrorBadRequest("invalid id"))?;
    let item = state.stores.get(id).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("item not found"),
        other => error::ErrorInternalServerError(other.to_string()),
    })?;
    if body.is_empty() {
        return Err(error::ErrorBadRequest("empty image body"));
    }
    cache
        .upload(id, role, item.kind, index, body)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    state.notify_library_changed();
    Ok(HttpResponse::NoContent().finish())
}

async fn remove_image(
    state: &AppState,
    user: &AuthUser,
    id_str: &str,
    image_type: &str,
    index: u32,
) -> Result<HttpResponse, actix_web::Error> {
    if !user.0.policy.admin {
        return Err(error::ErrorForbidden("admin required"));
    }
    let Some(role) = ImageRole::from_str_ci(image_type) else {
        return Err(error::ErrorBadRequest("unknown image type"));
    };
    let Some(cache) = state.images.as_ref() else {
        return Ok(HttpResponse::NoContent().finish());
    };
    let id: u64 = id_str
        .parse()
        .map_err(|_| error::ErrorBadRequest("invalid id"))?;
    let item = state.stores.get(id).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("item not found"),
        other => error::ErrorInternalServerError(other.to_string()),
    })?;
    cache
        .remove(id, role, item.kind, index)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    state.notify_library_changed();
    Ok(HttpResponse::NoContent().finish())
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
        let app = test::init_service(App::new().app_data(state).configure(register)).await;
        for t in ["primary", "backdrop", "thumb", "logo", "banner", "art"] {
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
        let app = test::init_service(App::new().app_data(state).configure(register)).await;
        let req = test::TestRequest::get()
            .uri("/items/abc/images/backdrop/0")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 404);
    }

    #[actix_web::test]
    async fn unknown_type_returns_400() {
        let state = seed_state().await;
        let app = test::init_service(App::new().app_data(state).configure(register)).await;
        let req = test::TestRequest::get()
            .uri("/items/abc/images/bogus")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 400);
    }

    #[actix_web::test]
    async fn head_request_returns_no_body_404() {
        let state = seed_state().await;
        let app = test::init_service(App::new().app_data(state).configure(register)).await;
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
        let app = test::init_service(App::new().app_data(state).configure(register)).await;
        let req = test::TestRequest::get()
            .uri("/items/abc/images/primary")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_ne!(resp.status(), 401);
    }

    #[actix_web::test]
    async fn post_requires_auth_and_returns_401_without_token() {
        let state = seed_state().await;
        let app = test::init_service(App::new().app_data(state).configure(register)).await;
        let req = test::TestRequest::post()
            .uri("/items/1/images/primary")
            .set_payload(vec![0xFFu8, 0xD8, 0xFF, 0xE0])
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 401);
    }
}
