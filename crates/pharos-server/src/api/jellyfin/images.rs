//! Jellyfin `/Items/{id}/Images/*` — Primary on demand (extracted via
//! ffmpeg), Backdrop / Thumb extracted at a deeper / wider point;
//! Logo / Banner / Art / Disc are upload-only.
//!
//! GET endpoints are intentionally **unauthenticated** to match
//! Jellyfin's reference behaviour — image URLs are passed around in
//! `<img src=…>` tags where header auth isn't an option. POST/DELETE
//! (admin uploads) **do** require an admin bearer (V8/V9).

use crate::{api::jellyfin::auth_extractor::AuthUser, state::AppState};
use actix_web::{error, web, HttpRequest, HttpResponse};
use pharos_cache::image_cache::{ImageCacheError, ImageRole};
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
    let id: u64 = pharos_jellyfin_api::dto::parse_item_id(&id_str)
        .ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
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
    // PERSON photos first — jellyfin-web's favorites tab + cast lists fetch
    // people via /Items/{personWireId}/Images/Primary, and the scanner
    // records a scraped `thumb_url` for most people. 302 to it — the same
    // public-redirect pattern the Live-TV channel logos use (an <img src>
    // can't attach auth; matches Jellyfin's public image routes). Checked
    // before the cache guard: a redirect needs no image cache. Only for
    // non-item ids (a person wire id never parses as an item id), so real
    // items never pay the extra lookup.
    if matches!(role, ImageRole::Primary)
        && pharos_jellyfin_api::dto::parse_item_id(id_str).is_none()
    {
        use pharos_core::PersonStore;
        if let Ok(Some(person)) = state.stores.person_by_wire_id(id_str).await {
            // Only real URLs: the scanner also ingests LEGACY Jellyfin
            // metadata (thumb_url = the old server's local disk path like
            // /config/data/metadata/People/…), which a browser would
            // resolve relative to pharos and 404.
            return Ok(
                match person
                    .thumb_url
                    .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
                {
                    Some(url) => HttpResponse::Found()
                        .insert_header((actix_web::http::header::LOCATION, url))
                        .finish(),
                    // Known person, no photo → 404; the client renders its
                    // initials placeholder.
                    None => HttpResponse::NotFound().body(""),
                },
            );
        }
    }
    let Some(cache) = state.images.as_ref() else {
        return Ok(HttpResponse::NotFound().body(""));
    };
    // A numeric id is a real media row. A 32-hex id is a SYNTHESISED
    // Series/Season (pharos stores no row for those — they're grouped from
    // episodes), so resolve it to a representative episode and serve that
    // episode's frame as the show/season poster. Without this every series
    // tile in the library 404'd its image.
    let item = match pharos_jellyfin_api::dto::parse_item_id(id_str).ok_or(()) {
        Ok(id) => match state.stores.get(id).await {
            Ok(it) => it,
            Err(_) => return Ok(HttpResponse::NotFound().body("")),
        },
        Err(_) => match resolve_synth_image_item(state, id_str).await {
            Some(it) => it,
            None => return Ok(HttpResponse::NotFound().body("")),
        },
    };
    let id = item.id;
    // LIB-D5 — local-sidecar-first resolution. D4 records artwork
    // discovered at scan time (poster.jpg / fanart.jpg / logo.png /…
    // beside the media, or under the series folder for episodes) as
    // `artwork` rows keyed by (item, role). Serve that recorded file
    // directly — ahead of any uploaded asset and the ffmpeg
    // frame-extract fallback — so a user's own artwork wins and
    // upload-only roles (Logo/Banner/Art/Disc) become servable.
    //
    // The artwork table is one row per (item, role) with no index, so
    // only the canonical index (0) can resolve locally; higher Backdrop
    // indices fall through to the existing path. A best-effort lookup:
    // any store error or a recorded-but-missing file silently falls
    // through to the upload / extract path (V6 spirit — never 500 a
    // public image route on a stale row).
    if index == 0 {
        if let Some(local) = local_artwork_path(state, id, role).await {
            return serve_local_artwork(&local, role, head_only, format, state).await;
        }
    }
    // An audio track has no video frames, so Backdrop / Thumb can only ever
    // come from a local sidecar (served just above). Don't spawn the ffmpeg
    // frame-extract fallback for them: it always fails ("Output file does not
    // contain any stream") and needlessly loads the libav worker pool — a
    // storm of these was starving real ops into timeouts. Primary still falls
    // through so embedded cover art extracts.
    if matches!(item.kind, pharos_core::MediaKind::Audio)
        && matches!(role, ImageRole::Backdrop | ImageRole::Thumb)
    {
        return Ok(HttpResponse::NotFound().body(""));
    }
    let jpeg_path = match cache.fetch(id, role, item.kind, &item.path, index).await {
        Ok(p) => p,
        // Upload-only roles (Logo/Banner/Art/Disc) report
        // `UploadOnly` when no upload has happened — surface as 404
        // for the read endpoint, same as a missing file.
        Err(ImageCacheError::UploadOnly) => return Ok(HttpResponse::NotFound().body("")),
        // Source genuinely has no image for this role (e.g. coverless audio).
        // Expected + now negatively-cached, so 404 quietly — no warn, no repeat
        // ffmpeg. Prevents the per-grid-render extract storm on cover-art-less
        // music.
        Err(ImageCacheError::NoContent) => return Ok(HttpResponse::NotFound().body("")),
        Err(e) => {
            tracing::warn!(
                error = %e,
                media.id = id,
                media.path = %item.path.display(),
                ?role,
                "image extraction failed"
            );
            return Ok(HttpResponse::NotFound().body(""));
        }
    };
    // P46 + P48 — optional re-encode to webp / avif via the
    // FfmpegBackend trait (was a direct Command::new in P46; routed
    // through the backend so the swap to ffmpeg-next in P49+ touches
    // exactly one place). Cached as a sibling path so subsequent
    // fetches skip the encode entirely.
    let (final_path, content_type) = match format {
        ImageFormat::Jpeg => (jpeg_path, "image/jpeg"),
        ImageFormat::Webp => match transcode_via_backend(state, &jpeg_path, "webp").await {
            Ok(p) => (p, "image/webp"),
            Err(e) => {
                tracing::warn!(error = %e, "webp transcode failed; serving jpeg");
                (jpeg_path, "image/jpeg")
            }
        },
        ImageFormat::Avif => match transcode_via_backend(state, &jpeg_path, "avif").await {
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

/// Resolve a synthesised Series/Season/Artist/Album wire id (a 32-hex hash,
/// no stored row) to a representative member item whose frame / cover stands
/// in as the group's poster. Series/Season pick the lowest `(season, episode)`
/// so the poster is stable (usually S01E01); Artist/Album pick the first track.
/// Returns `None` for an id matching no group (→ 404).
async fn resolve_synth_image_item(
    state: &AppState,
    id_str: &str,
) -> Option<pharos_core::MediaItem> {
    // Memoised resolution: a TV-library grid fires one image request per tile,
    // and each full `list()` scans the whole library. Cache the synth-id →
    // item-id mapping (including negatives) so only the first request per group
    // pays the scan.
    if let Some(cached) = state.synth_image_cached(id_str) {
        return match cached {
            Some(item_id) => state.stores.get(item_id).await.ok(),
            None => None,
        };
    }
    // Cold miss: warm the ENTIRE synth-id → representative map in one scan,
    // serialised so a grid's burst of concurrent poster requests doesn't each
    // run its own multi-second `list()`. Peers wait here, then hit the memo.
    let _guard = state.synth_image_warm.lock().await;
    if let Some(cached) = state.synth_image_cached(id_str) {
        return match cached {
            Some(item_id) => state.stores.get(item_id).await.ok(),
            None => None,
        };
    }
    let all = state.stores.list().await.ok()?;
    build_synth_image_map(state, &all);
    // The requested id is now memoised if it names a real group; otherwise
    // record a negative so we don't rescan for it.
    match state.synth_image_cached(id_str) {
        Some(Some(item_id)) => all.into_iter().find(|it| it.id == item_id),
        _ => {
            state.synth_image_remember(id_str, None);
            None
        }
    }
}

/// Build the full synth-id → representative-item map from one library scan and
/// store it in `state.synth_image_ids`. Representative choice matches the old
/// per-id resolution: the lowest (season, episode) for a Series/Season, the
/// first track by title for an Artist/Album. A uniform sortable string key
/// expresses both orderings — `S{season}E{episode}` zero-padded for episodes,
/// the raw title for tracks — so one min-by-key pass covers every group.
fn build_synth_image_map(state: &AppState, all: &[pharos_core::MediaItem]) {
    use crate::api::jellyfin::dto::{
        album_id_for, artist_id_for, season_id_for_key, series_id_for_key,
    };
    use std::collections::HashMap;
    // synth id -> (representative item id, sort key). Lowest key wins.
    let mut best: HashMap<String, (u64, String)> = HashMap::new();
    let mut consider = |id: String, item_id: u64, key: String| {
        best.entry(id)
            .and_modify(|e| {
                if key < e.1 {
                    *e = (item_id, key.clone());
                }
            })
            .or_insert((item_id, key));
    };
    for it in all {
        if let Some(s) = it.series.as_ref() {
            let key = format!(
                "{:010}{:010}",
                s.season_number.unwrap_or(0),
                s.episode_number.unwrap_or(0)
            );
            consider(
                series_id_for_key(s.series_folder.as_deref(), &s.series_name),
                it.id,
                key.clone(),
            );
            if let Some(n) = s.season_number {
                consider(
                    season_id_for_key(s.series_folder.as_deref(), &s.series_name, n),
                    it.id,
                    key,
                );
            }
        }
        if let Some(a) = it.probe.artist.as_deref() {
            consider(artist_id_for(a), it.id, it.title.clone());
        }
        if let Some(a) = it.probe.album.as_deref() {
            consider(album_id_for(a), it.id, it.title.clone());
        }
    }
    let mut cache = state
        .synth_image_ids
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for (id, (item_id, _)) in best {
        cache.entry(id).or_insert(Some(item_id));
    }
}

/// The `ArtworkRole::as_str` token (D4 stores these in `artwork.role`)
/// that corresponds to a cache [`ImageRole`]. `ImageRole::as_dir` is
/// private + lowercase; the artwork rows use the capitalised tokens,
/// so map explicitly. Match is exhaustive so a new role can't silently
/// stop resolving locally.
fn artwork_role_token(role: ImageRole) -> &'static str {
    match role {
        ImageRole::Primary => "Primary",
        ImageRole::Backdrop => "Backdrop",
        ImageRole::Thumb => "Thumb",
        ImageRole::Logo => "Logo",
        ImageRole::Banner => "Banner",
        ImageRole::Art => "Art",
        ImageRole::Disc => "Disc",
    }
}

/// LIB-D5 — the recorded local sidecar path for `(id, role)`, if D4
/// scanning found one and the file still exists. Best-effort: a store
/// error or a stale (deleted) sidecar yields `None` so the caller falls
/// through to the upload / frame-extract path rather than 404ing or
/// erroring on a public route (V6 spirit).
async fn local_artwork_path(
    state: &AppState,
    id: u64,
    role: ImageRole,
) -> Option<std::path::PathBuf> {
    let token = artwork_role_token(role);
    let rows = match state.stores.artwork_for(id).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, media.id = id, "artwork lookup failed");
            return None;
        }
    };
    let locator = rows
        .into_iter()
        .find(|(r, source, _)| r.eq_ignore_ascii_case(token) && source == "local")
        .map(|(_, _, locator)| locator)?;
    let path = std::path::PathBuf::from(locator);
    match tokio::fs::try_exists(&path).await {
        Ok(true) => Some(path),
        _ => {
            tracing::warn!(
                media.id = id,
                role = token,
                "recorded local artwork missing on disk"
            );
            None
        }
    }
}

/// Content-type for a local sidecar by extension. Defaults to jpeg for
/// unknown / missing extensions (the bytes still render in browsers,
/// and the D4 detector only records png/jpg/jpeg/webp anyway).
fn content_type_for_ext(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        Some("avif") => "image/avif",
        Some("gif") => "image/gif",
        Some("bmp") => "image/bmp",
        _ => "image/jpeg",
    }
}

/// LIB-D5 — serve a recorded local sidecar file directly. Honours the
/// `?format=` webp/avif negotiation by transcoding through the same
/// backend the frame-extract path uses, but writes the transcoded copy
/// into the cache directory (keyed by a hash of the sidecar path) so we
/// never pollute the user's media folder. A transcode failure — or no
/// cache to write into — falls back to the original sidecar bytes.
async fn serve_local_artwork(
    path: &std::path::Path,
    role: ImageRole,
    head_only: bool,
    format: ImageFormat,
    state: &AppState,
) -> Result<HttpResponse, actix_web::Error> {
    // Downscale big poster/fanart sidecars to a display-appropriate width and
    // serve the cached copy — a full-res multi-MB sidecar read off NFS was
    // costing 1-4s per tile in a library grid. Only opaque photographic roles
    // are scaled; Logo/Banner/Art/Disc are usually small PNGs with alpha that
    // a JPEG re-encode would wreck, so they pass through untouched.
    let target_width: Option<u32> = match role {
        ImageRole::Primary => Some(480),
        ImageRole::Thumb => Some(640),
        ImageRole::Backdrop => Some(1280),
        _ => None,
    };
    let scaled;
    let path = match (target_width, state.images.as_ref()) {
        (Some(width), Some(cache)) => {
            scaled = cache.scaled_artwork(path, width).await;
            scaled.as_path()
        }
        _ => path,
    };
    let ext = match format {
        ImageFormat::Jpeg => None,
        ImageFormat::Webp => Some("webp"),
        ImageFormat::Avif => Some("avif"),
    };
    let (final_path, content_type): (std::path::PathBuf, &'static str) = match ext {
        None => (path.to_path_buf(), content_type_for_ext(path)),
        Some(ext) => match transcode_sidecar(state, path, ext).await {
            Ok(p) => (
                p,
                if ext == "webp" {
                    "image/webp"
                } else {
                    "image/avif"
                },
            ),
            Err(e) => {
                tracing::warn!(error = %e, target_ext = ext, "sidecar transcode failed; serving original");
                (path.to_path_buf(), content_type_for_ext(path))
            }
        },
    };
    if head_only {
        return Ok(HttpResponse::Ok().content_type(content_type).finish());
    }
    match tokio::fs::read(&final_path).await {
        Ok(bytes) => Ok(HttpResponse::Ok().content_type(content_type).body(bytes)),
        Err(_) => Ok(HttpResponse::NotFound().body("")),
    }
}

/// Transcode a local sidecar into `ext` (webp/avif), writing the output
/// into a cache-owned `sidecar/` subdir keyed by a hash of the source
/// path so re-encodes are skipped and the user's media folder is left
/// untouched. Errors if no image cache is configured.
async fn transcode_sidecar(
    state: &AppState,
    src: &std::path::Path,
    ext: &str,
) -> Result<std::path::PathBuf, std::io::Error> {
    let cache = state
        .images
        .as_ref()
        .ok_or_else(|| std::io::Error::other("image cache not configured"))?;
    use xxhash_rust::xxh3::xxh3_64;
    let key = xxh3_64(src.as_os_str().as_encoded_bytes());
    let dir = cache.root().join("sidecar");
    tokio::fs::create_dir_all(&dir).await?;
    let out = dir.join(format!("{key:016x}.{ext}"));
    if tokio::fs::try_exists(&out).await.unwrap_or(false) {
        return Ok(out);
    }
    let tmp = dir.join(format!("{key:016x}.{ext}.tmp"));
    state
        .ffmpeg
        .transcode_image(src, ext, &tmp)
        .await
        .map_err(|e| std::io::Error::other(format!("backend {ext} transcode: {e}")))?;
    tokio::fs::rename(&tmp, &out).await?;
    Ok(out)
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

/// P46 + P48 — transcode the cached jpeg into a sibling `.{ext}`
/// file via the `FfmpegBackend` trait. Atomic via `.tmp → final` so
/// concurrent first-readers don't observe a half-written output.
async fn transcode_via_backend(
    state: &AppState,
    jpeg_path: &std::path::Path,
    ext: &str,
) -> Result<std::path::PathBuf, std::io::Error> {
    let mut out = jpeg_path.to_path_buf();
    out.set_extension(ext);
    if tokio::fs::try_exists(&out).await.unwrap_or(false) {
        return Ok(out);
    }
    let tmp = jpeg_path.with_extension(format!("{ext}.tmp"));
    state
        .ffmpeg
        .transcode_image(jpeg_path, ext, &tmp)
        .await
        .map_err(|e| std::io::Error::other(format!("backend {ext} transcode: {e}")))?;
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
    let id: u64 = pharos_jellyfin_api::dto::parse_item_id(id_str)
        .ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
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
    let id: u64 = pharos_jellyfin_api::dto::parse_item_id(id_str)
        .ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
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
    use crate::state::Stores;
    use actix_web::{test, App};

    #[::core::prelude::v1::test]
    fn parse_image_format_picks_webp_and_avif() {
        // P46 — explicit ?format= overrides; unknown values stay
        // jpeg so a typo doesn't break image rendering. Fully-qualified
        // `::core::prelude::v1::test` so this file's `use actix_web::test`
        // import (the async-test macro) doesn't shadow the builtin.
        assert!(matches!(
            parse_image_format("format=webp"),
            ImageFormat::Webp
        ));
        assert!(matches!(
            parse_image_format("Format=AVIF"),
            ImageFormat::Avif
        ));
        assert!(matches!(
            parse_image_format("format=xxx"),
            ImageFormat::Jpeg
        ));
        assert!(matches!(parse_image_format(""), ImageFormat::Jpeg));
    }

    async fn seed_state() -> web::Data<crate::state::AppState> {
        let stores = Stores::connect("sqlite::memory:").await.unwrap();
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

    // LIB-D5 — a 1x1 PNG, used as a real sidecar fixture. The local
    // branch serves these bytes verbatim (no ffmpeg), so the content is
    // byte-comparable.
    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x62, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    async fn seed_state_with_cache(
        cache_dir: &std::path::Path,
    ) -> web::Data<crate::state::AppState> {
        use pharos_cache::image_cache::ImageCache;
        let stores = Stores::connect("sqlite::memory:").await.unwrap();
        let state = crate::state::AppState::new(stores, "t".into())
            .with_image_cache(ImageCache::new(cache_dir.to_path_buf()));
        web::Data::new(state)
    }

    async fn put_movie(state: &crate::state::AppState, id: u64, path: &std::path::Path) {
        use pharos_core::{MediaItem, MediaKind, MediaStore};
        let item = MediaItem {
            id,
            path: path.to_path_buf(),
            title: "A".into(),
            kind: MediaKind::Movie,
            ..Default::default()
        };
        state.stores.put(item).await.unwrap();
    }

    #[actix_web::test]
    async fn local_primary_artwork_is_served_verbatim() {
        use pharos_core::MediaStore;
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        // A movie file that doesn't exist on disk — frame-extraction
        // would fail, so a 200 here proves the local branch ran.
        let media_path = dir.path().join("movie.mkv");
        let poster = dir.path().join("poster.png");
        std::fs::write(&poster, PNG_1X1).unwrap();

        let state = seed_state_with_cache(cache_dir.path()).await;
        put_movie(&state, 42, &media_path).await;
        state
            .stores
            .set_artwork(42, "Primary", "local", &poster.to_string_lossy())
            .await
            .unwrap();

        let app = test::init_service(App::new().app_data(state.clone()).configure(register)).await;
        let req = test::TestRequest::get()
            .uri("/items/42/images/primary")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers().get("content-type").unwrap(), "image/png");
        let body = test::read_body(resp).await;
        assert_eq!(body.as_ref(), PNG_1X1, "served bytes must be the sidecar");
    }

    #[actix_web::test]
    async fn upload_only_role_served_from_local_sidecar() {
        // Logo is upload-only (no frame-extract). A recorded local
        // sidecar makes it servable.
        use pharos_core::MediaStore;
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let logo = dir.path().join("logo.png");
        std::fs::write(&logo, PNG_1X1).unwrap();
        let state = seed_state_with_cache(cache_dir.path()).await;
        put_movie(&state, 7, &dir.path().join("movie.mkv")).await;
        state
            .stores
            .set_artwork(7, "Logo", "local", &logo.to_string_lossy())
            .await
            .unwrap();
        let app = test::init_service(App::new().app_data(state).configure(register)).await;
        let req = test::TestRequest::get()
            .uri("/items/7/images/logo")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
        let body = test::read_body(resp).await;
        assert_eq!(body.as_ref(), PNG_1X1);
    }

    #[actix_web::test]
    async fn no_local_art_and_no_extract_source_404s() {
        // No artwork row + a non-existent media file => extraction
        // fails => 404 (existing fallback behaviour preserved).
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let state = seed_state_with_cache(cache_dir.path()).await;
        put_movie(&state, 9, &dir.path().join("missing.mkv")).await;
        let app = test::init_service(App::new().app_data(state).configure(register)).await;
        let req = test::TestRequest::get()
            .uri("/items/9/images/primary")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 404);
    }

    #[actix_web::test]
    async fn stale_local_art_row_falls_through_to_404() {
        // A recorded artwork row whose file was deleted must not 500
        // or serve garbage — it falls through to the extract path,
        // which 404s for a missing source.
        use pharos_core::MediaStore;
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let state = seed_state_with_cache(cache_dir.path()).await;
        put_movie(&state, 11, &dir.path().join("missing.mkv")).await;
        state
            .stores
            .set_artwork(
                11,
                "Primary",
                "local",
                &dir.path().join("gone.png").to_string_lossy(),
            )
            .await
            .unwrap();
        let app = test::init_service(App::new().app_data(state).configure(register)).await;
        let req = test::TestRequest::get()
            .uri("/items/11/images/primary")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 404);
    }
}
