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
use actix_web::{error, web, HttpRequest, HttpResponse};
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
    req: HttpRequest,
    path: web::Path<ImagePath>,
) -> Result<HttpResponse, actix_web::Error> {
    serve_image(
        &state,
        &path.id,
        &path.image_type,
        0,
        false,
        parse_image_format(req.query_string()),
    )
    .await
}

async fn head_image(
    state: web::Data<AppState>,
    req: HttpRequest,
    path: web::Path<ImagePath>,
) -> Result<HttpResponse, actix_web::Error> {
    serve_image(
        &state,
        &path.id,
        &path.image_type,
        0,
        true,
        parse_image_format(req.query_string()),
    )
    .await
}

async fn get_image_indexed(
    state: web::Data<AppState>,
    req: HttpRequest,
    path: web::Path<IndexedImagePath>,
) -> Result<HttpResponse, actix_web::Error> {
    serve_image(
        &state,
        &path.id,
        &path.image_type,
        path.image_index,
        false,
        parse_image_format(req.query_string()),
    )
    .await
}

async fn head_image_indexed(
    state: web::Data<AppState>,
    req: HttpRequest,
    path: web::Path<IndexedImagePath>,
) -> Result<HttpResponse, actix_web::Error> {
    serve_image(
        &state,
        &path.id,
        &path.image_type,
        path.image_index,
        true,
        parse_image_format(req.query_string()),
    )
    .await
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
    format: ImageFormat,
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
    let jpeg_path = match cache.fetch(id, role, item.kind, &item.path, index).await {
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
    // P46 — optional re-encode to webp / avif. Cached as a sibling
    // path next to the source jpeg so subsequent fetches skip the
    // ffmpeg spawn entirely.
    let (final_path, content_type) = match format {
        ImageFormat::Jpeg => (jpeg_path, "image/jpeg"),
        ImageFormat::Webp => match transcode_image(&jpeg_path, "webp").await {
            Ok(p) => (p, "image/webp"),
            Err(e) => {
                tracing::warn!(error = %e, "webp transcode failed; serving jpeg");
                (jpeg_path, "image/jpeg")
            }
        },
        ImageFormat::Avif => match transcode_image(&jpeg_path, "avif").await {
            Ok(p) => (p, "image/avif"),
            Err(e) => {
                tracing::warn!(error = %e, "avif transcode failed; serving jpeg");
                (jpeg_path, "image/jpeg")
            }
        },
    };
    if head_only {
        return Ok(HttpResponse::Ok().content_type(content_type).finish());
    }
    let bytes = match tokio::fs::read(&final_path).await {
        Ok(b) => b,
        Err(_) => return Ok(HttpResponse::NotFound().body("")),
    };
    Ok(HttpResponse::Ok().content_type(content_type).body(bytes))
}

/// P46 — client-requested image encoding. Modern web clients +
/// jellyfin-web can hint a preferred format via `?format=`; pharos
/// returns jpeg for any unknown / unsupported value so a typo
/// can't break image rendering on existing clients.
#[derive(Debug, Clone, Copy)]
enum ImageFormat {
    Jpeg,
    Webp,
    Avif,
}

fn parse_image_format(qs: &str) -> ImageFormat {
    for kv in qs.split('&') {
        if let Some((k, v)) = kv.split_once('=') {
            if k.eq_ignore_ascii_case("format") {
                return match v.to_ascii_lowercase().as_str() {
                    "webp" => ImageFormat::Webp,
                    "avif" => ImageFormat::Avif,
                    _ => ImageFormat::Jpeg,
                };
            }
        }
    }
    ImageFormat::Jpeg
}

/// P46 — transcode the cached jpeg into a sibling `.{ext}` file when
/// requested. Returns the sibling path. Atomic via `.tmp → final`.
async fn transcode_image(
    jpeg_path: &std::path::Path,
    ext: &str,
) -> Result<std::path::PathBuf, std::io::Error> {
    let mut out = jpeg_path.to_path_buf();
    out.set_extension(ext);
    if tokio::fs::try_exists(&out).await.unwrap_or(false) {
        return Ok(out);
    }
    let tmp = jpeg_path.with_extension(format!("{ext}.tmp"));
    let codec = match ext {
        "webp" => "libwebp",
        "avif" => "libaom-av1",
        _ => "mjpeg",
    };
    let mut cmd = tokio::process::Command::new("ffmpeg");
    cmd.args([
        "-y",
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-i",
    ]);
    cmd.arg(jpeg_path);
    cmd.args(["-c:v", codec]);
    // avif still-picture needs a single frame + tuning flags so the
    // libaom encoder produces a usable still rather than a 1-frame
    // video clip the browser refuses to decode.
    if ext == "avif" {
        cmd.args(["-still-picture", "1", "-cpu-used", "8"]);
    } else {
        cmd.args(["-quality", "80"]);
    }
    cmd.arg(&tmp);
    let status = cmd.status().await?;
    if !status.success() {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(std::io::Error::other(format!(
            "ffmpeg {ext} transcode failed",
        )));
    }
    tokio::fs::rename(&tmp, &out).await?;
    Ok(out)
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

    #[::core::prelude::v1::test]
    fn parse_image_format_picks_webp_and_avif() {
        // P46 — explicit ?format= overrides; unknown values stay
        // jpeg so a typo doesn't break image rendering. Fully-qualified
        // `::core::prelude::v1::test` so this file's `use actix_web::test`
        // import (the async-test macro) doesn't shadow the builtin.
        assert!(matches!(parse_image_format("format=webp"), ImageFormat::Webp));
        assert!(matches!(parse_image_format("Format=AVIF"), ImageFormat::Avif));
        assert!(matches!(parse_image_format("format=xxx"), ImageFormat::Jpeg));
        assert!(matches!(parse_image_format(""), ImageFormat::Jpeg));
    }

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
