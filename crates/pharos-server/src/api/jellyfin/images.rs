//! Jellyfin `/Items/{id}/Images/*` — Primary on demand (extracted via
//! ffmpeg), Backdrop / Thumb extracted at a deeper / wider point;
//! Logo / Banner / Art / Disc are upload-only.
//!
//! GET/HEAD endpoints are anonymous like Jellyfin's: `<img src=…>` and the
//! native apps' image loaders send no credential (V225). A request that does
//! carry one (header, `api_key` or the `JellyfinAuth` cookie) only sees items
//! its user may reach (V189). POST/DELETE (admin uploads) require an admin
//! bearer (V8/V9).

use crate::{api::jellyfin::auth_extractor::AuthUser, state::AppState};
use actix_web::{error, web, HttpRequest, HttpResponse};
use pharos_cache::image_cache::{ImageCacheError, ImageRole};
use pharos_core::MediaStore;
use serde::Deserialize;
use tracing::Instrument as _;

/// B89 — how long a client may reuse a cached image without revalidating.
/// pharos's image URLs are `(item, role)`-stable and carry a stable `?tag=`, so
/// the bytes are safely cacheable; a moderate TTL keeps repeat gallery renders
/// instant while a re-scan / uploaded poster is still picked up within the
/// window. Before B89 image responses carried NO cache headers at all, so a
/// grid client re-downloaded every poster on every render.
const IMAGE_MAX_AGE_SECS: u32 = 604_800; // 7 days

fn image_cache_control() -> String {
    format!("private, max-age={IMAGE_MAX_AGE_SECS}")
}

/// A JPEG answering a `?format=avif|webp` request while that encode is still
/// queued. It must not be kept for the week the real format would be: the
/// browser revalidates it each time, and the changed ETag brings the encoded
/// copy once it exists.
const IMAGE_PENDING_CACHE_CONTROL: &str = "private, no-cache";

/// How long a client may keep an image response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageFreshness {
    Settled,
    /// The JPEG stand-in for a format whose encode is pending.
    Pending,
}

impl ImageFreshness {
    fn cache_control(self) -> String {
        match self {
            Self::Settled => image_cache_control(),
            Self::Pending => IMAGE_PENDING_CACHE_CONTROL.to_owned(),
        }
    }
}

/// A content ETag from the served file's length + mtime — stable while the
/// cached bytes are, and rotates when the cache re-extracts / re-encodes.
/// `None` if the file vanished (the caller then 404s on the body read).
#[tracing::instrument(name = "image.etag", skip_all)]
async fn file_etag(path: &std::path::Path) -> Option<String> {
    let m = tokio::fs::metadata(path).await.ok()?;
    let len = m.len();
    let mtime = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    Some(format!("\"{len:x}-{mtime:x}\""))
}

/// True when the client's `If-None-Match` already holds the current `etag`.
fn if_none_match_hit(if_none_match: Option<&str>, etag: Option<&str>) -> bool {
    match (if_none_match, etag) {
        (Some(inm), Some(etag)) => inm.split(',').map(str::trim).any(|t| t == etag || t == "*"),
        _ => false,
    }
}

/// The request's `If-None-Match` header value, if any (borrows `req`).
fn if_none_match_of(req: &HttpRequest) -> Option<&str> {
    req.headers()
        .get(actix_web::http::header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
}

/// B89 — serve an already-resolved cache/sidecar image file with client-cache
/// headers: `Cache-Control` + a content `ETag`, a `304 Not Modified` when the
/// client's `If-None-Match` still matches (so a lapsed cache revalidates
/// cheaply), and the bytes otherwise (or just headers for a HEAD). A vanished
/// file → 404. Centralising this is what makes every read path — extracted
/// frame, scaled copy, webp/avif re-encode, local sidecar, chapter thumb —
/// cacheable through one door.
async fn deliver_image(
    final_path: &std::path::Path,
    content_type: &'static str,
    freshness: ImageFreshness,
    head_only: bool,
    if_none_match: Option<&str>,
) -> HttpResponse {
    use actix_web::http::header::{CACHE_CONTROL, ETAG};
    let etag = file_etag(final_path).await;
    if if_none_match_hit(if_none_match, etag.as_deref()) {
        let mut resp = HttpResponse::NotModified();
        resp.insert_header((CACHE_CONTROL, freshness.cache_control()));
        if let Some(t) = &etag {
            resp.insert_header((ETAG, t.clone()));
        }
        return resp.finish();
    }
    let mut resp = HttpResponse::Ok();
    resp.content_type(content_type);
    resp.insert_header((CACHE_CONTROL, freshness.cache_control()));
    if let Some(t) = &etag {
        resp.insert_header((ETAG, t.clone()));
    }
    if head_only {
        // B166 — a HEAD must advertise the length the GET would send. actix's
        // encoder derives `Content-Length` from the body's declared
        // `BodySize` and DROPS any manually-inserted header, so `.finish()` —
        // an empty body, `BodySize::Sized(0)` — advertised `Content-Length: 0`
        // for every image. Same trap as B101 on the video path, where a
        // zero-length HEAD collapsed Firefox's seekability probe.
        //
        // `SizedStream` declares the real length while its stream is never
        // polled for a HEAD, so the bytes are still not read off disk.
        let len = tokio::fs::metadata(final_path)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        return resp.body(actix_web::body::SizedStream::new(
            len,
            futures_util::stream::empty::<Result<actix_web::web::Bytes, actix_web::Error>>(),
        ));
    }
    match tokio::fs::read(final_path)
        .instrument(tracing::info_span!("image.read"))
        .await
    {
        Ok(bytes) => resp.body(bytes),
        Err(_) => HttpResponse::NotFound().body(""),
    }
}

pub fn register(cfg: &mut web::ServiceConfig) {
    // T31: lowercase canonical paths. The `image_type` path param is
    // therefore also lowercased by `LowercasePath` — `ImageRole::from_str_ci`
    // accepts both forms anyway.
    // The bare LIST route jellyfin-web's "Edit Images" dialog calls first
    // (`getItemImageInfos`). Its absence 404'd → the dialog spun forever
    // (unguarded ajax, no .catch).
    //
    // Registration order IS load-bearing between routes of the same segment
    // count: actix takes the first pattern that matches, so a literal segment
    // only wins over a parameter if it is registered first. Every `chapter`
    // route below is therefore listed ahead of the `{image_type}` pattern of
    // the same arity.
    cfg.route("/items/{id}/images", web::get().to(get_item_images))
        // P32 — chapter thumbnails, BOTH client spellings, and both registered
        // ahead of the `{image_type}` patterns of the same arity. `chapter` is
        // not an `ImageRole`, so whichever generic route matches first answers
        // `400 unknown image type` from a handler the request never reached.
        // Dispatches to `ImageCache::chapter`, which seeks ffmpeg to the
        // chapter's start_ms.
        //
        // jellyfin-web:            /images/chapter/{index}
        // jellyfin-sdk-kotlin:     /images/chapter?imageIndex={index}
        .route(
            "/items/{id}/images/chapter",
            web::get().to(get_chapter_image_by_query),
        )
        .route(
            "/items/{id}/images/chapter/{image_index}",
            web::get().to(get_chapter_image),
        )
        .route("/items/{id}/images/{image_type}", web::get().to(get_image))
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
        );
}

/// `GET /Items/{id}/Images` — the ImageInfo LIST jellyfin-web's "Edit Images"
/// dialog fetches first. Returns the roles the item advertises (matching its
/// `BaseItemDto` tags) so every editor thumbnail resolves. Anonymous like
/// the other GET image routes. A non-item id (person / synthesised container)
/// or an unknown item yields `[]` — an empty grid, never a hang.
async fn get_item_images(
    state: web::Data<AppState>,
    req: HttpRequest,
    path: web::Path<String>,
) -> Result<HttpResponse, actix_web::Error> {
    let id_str = path.into_inner();
    let user = crate::api::jellyfin::optional_bearer(&state, &req, &id_str, "image").await?;
    let canonical = crate::api::jellyfin::items::canonical_wire_id(&id_str);
    let Some(id) = pharos_jellyfin_api::dto::parse_item_id(canonical.as_ref()) else {
        // Person portraits / synthesised Series-Season containers don't resolve
        // as a stored item id; the editor isn't reachable for them via a video
        // context. Empty list keeps any caller from spinning.
        return Ok(crate::api::jellyfin::wire::json(&Vec::<
            pharos_jellyfin_api::dto::ImageInfoDto,
        >::new()));
    };
    crate::api::jellyfin::require_item_access_if_signed_in(&state, user.as_ref(), id, "image")
        .await?;
    let item = match state.stores.get(id).await {
        Ok(item) => item,
        Err(pharos_core::DomainError::NotFound(_)) => {
            return Ok(crate::api::jellyfin::wire::json(&Vec::<
                pharos_jellyfin_api::dto::ImageInfoDto,
            >::new()));
        }
        Err(e) => return Err(error::ErrorInternalServerError(e.to_string())),
    };
    // LIB-D5 local sidecar roles (Logo/Banner/Art/Disc, or a role overriding a
    // frame-extract default). Best-effort — a store error just means the base
    // frame-extract roles, never a 500 that would re-hang the dialog.
    let local_roles: Vec<String> = match state.stores.artwork_for(id).await {
        Ok(rows) => rows
            .into_iter()
            .filter(|(_, source, _)| source == "local")
            .map(|(role, _, _)| role)
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e, media.id = id, "artwork lookup for image list failed");
            Vec::new()
        }
    };
    let infos = pharos_jellyfin_api::dto::image_infos_for(&item, &local_roles);
    Ok(crate::api::jellyfin::wire::json(&infos))
}

/// `GET /Items/{id}/Images/chapter?imageIndex=N` — the kotlin-SDK spelling of
/// the chapter thumbnail, index in the query rather than the path.
async fn get_chapter_image_by_query(
    state: web::Data<AppState>,
    req: HttpRequest,
    path: web::Path<String>,
) -> Result<HttpResponse, actix_web::Error> {
    let idx = requested_image_index(req.query_string());
    let id = path.into_inner();
    serve_chapter_image(&state, &req, &id, idx).await
}

async fn get_chapter_image(
    state: web::Data<AppState>,
    req: HttpRequest,
    path: web::Path<(String, u32)>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id_str, idx) = path.into_inner();
    serve_chapter_image(&state, &req, &id_str, idx).await
}

/// One chapter-thumbnail implementation behind both URL spellings.
async fn serve_chapter_image(
    state: &AppState,
    req: &HttpRequest,
    id_str: &str,
    idx: u32,
) -> Result<HttpResponse, actix_web::Error> {
    let user = crate::api::jellyfin::optional_bearer(state, req, id_str, "image").await?;
    let id: u64 = pharos_jellyfin_api::dto::parse_item_id(id_str)
        .ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
    crate::api::jellyfin::require_item_access_if_signed_in(state, user.as_ref(), id, "image")
        .await?;
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
        .chapter_3d(
            item.id,
            &item.path,
            idx,
            chapter.start_ms,
            item.probe.video_3d_format,
        )
        .await
        .map_err(|e| error::ErrorInternalServerError(format!("chapter image: {e}")))?;
    // B89 — chapter thumbs are the heaviest per-item image after B88 turned
    // them on; cache them client-side like every other artwork.
    Ok(deliver_image(
        &path,
        "image/jpeg",
        ImageFreshness::Settled,
        false,
        if_none_match_of(req),
    )
    .await)
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
    let user = crate::api::jellyfin::optional_bearer(&state, &req, &path.id, "image").await?;
    serve_image(
        &state,
        user.as_ref(),
        &path.id,
        &path.image_type,
        0,
        false,
        parse_image_format(req.query_string()),
        requested_width(req.query_string()),
        if_none_match_of(&req),
    )
    .await
}

async fn head_image(
    state: web::Data<AppState>,
    req: HttpRequest,
    path: web::Path<ImagePath>,
) -> Result<HttpResponse, actix_web::Error> {
    let user = crate::api::jellyfin::optional_bearer(&state, &req, &path.id, "image").await?;
    serve_image(
        &state,
        user.as_ref(),
        &path.id,
        &path.image_type,
        0,
        true,
        parse_image_format(req.query_string()),
        requested_width(req.query_string()),
        if_none_match_of(&req),
    )
    .await
}

async fn get_image_indexed(
    state: web::Data<AppState>,
    req: HttpRequest,
    path: web::Path<IndexedImagePath>,
) -> Result<HttpResponse, actix_web::Error> {
    let user = crate::api::jellyfin::optional_bearer(&state, &req, &path.id, "image").await?;
    serve_image(
        &state,
        user.as_ref(),
        &path.id,
        &path.image_type,
        path.image_index,
        false,
        parse_image_format(req.query_string()),
        requested_width(req.query_string()),
        if_none_match_of(&req),
    )
    .await
}

async fn head_image_indexed(
    state: web::Data<AppState>,
    req: HttpRequest,
    path: web::Path<IndexedImagePath>,
) -> Result<HttpResponse, actix_web::Error> {
    let user = crate::api::jellyfin::optional_bearer(&state, &req, &path.id, "image").await?;
    serve_image(
        &state,
        user.as_ref(),
        &path.id,
        &path.image_type,
        path.image_index,
        true,
        parse_image_format(req.query_string()),
        requested_width(req.query_string()),
        if_none_match_of(&req),
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

#[allow(clippy::too_many_arguments)]
async fn serve_image(
    state: &AppState,
    user: Option<&pharos_core::User>,
    id_str: &str,
    image_type: &str,
    index: u32,
    head_only: bool,
    format: ImageFormat,
    req_width: Option<u32>,
    if_none_match: Option<&str>,
) -> Result<HttpResponse, actix_web::Error> {
    // B85 — accept the dashed synth/person id the kotlin SDK (Android TV) sends:
    // person + Series/Season image resolution below compares the raw id to the
    // dashless stored hash, so a dashed id 404'd all show/person artwork.
    let canonical = crate::api::jellyfin::items::canonical_wire_id(id_str);
    let id_str: &str = canonical.as_ref();
    let Some(role) = ImageRole::from_str_ci(image_type) else {
        return Ok(HttpResponse::BadRequest().body("unknown image type"));
    };
    if let Some(id) = pharos_jellyfin_api::dto::parse_item_id(id_str) {
        crate::api::jellyfin::require_item_access_if_signed_in(state, user, id, "image").await?;
    }
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
            // A hidden person answers as one with no photo (V200). A failed
            // check withholds the photo too, like the lookup above.
            use crate::services::library::{visible_entity, EntityKind, EntityView};
            let visible = match user {
                None => true,
                Some(user) => match visible_entity(state, user, EntityKind::Person, id_str).await {
                    Ok(view) => !matches!(view, EntityView::Hidden),
                    Err(e) => {
                        tracing::warn!(error = %e, person = id_str, "person visibility check failed");
                        false
                    }
                },
            };
            if !visible {
                return Ok(HttpResponse::NotFound().body(""));
            }
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
    let mut synth_id = false;
    let item = match pharos_jellyfin_api::dto::parse_item_id(id_str).ok_or(()) {
        Ok(id) => match state
            .stores
            .get(id)
            .instrument(tracing::info_span!("image.item_lookup"))
            .await
        {
            Ok(it) => it,
            Err(_) => return Ok(HttpResponse::NotFound().body("")),
        },
        Err(_) => {
            synth_id = true;
            match resolve_synth_image_item(state, id_str)
                .instrument(tracing::info_span!("image.synth_resolve"))
                .await
            {
                Some(it) => it,
                None => return Ok(HttpResponse::NotFound().body("")),
            }
        }
    };
    let id = item.id;
    // A synth group answers through its representative member (V187).
    if synth_id {
        crate::api::jellyfin::require_item_access_if_signed_in(state, user, id, "image").await?;
    }
    // SEASON-specific sidecars. A synth Season id resolves to a representative
    // episode above, and that episode's own artwork rows record the SERIES
    // poster (the sidecar provider probes the show folder for episodes) — so
    // without this, every season of a show served the identical series
    // poster.jpg. Probe the Kodi/Jellyfin series-root convention
    // (`season02-poster.jpg`, `season-specials-poster.jpg`, `…-fanart`,
    // `…-banner`) for the season the requested id names. Only for synth ids
    // (a real item id is never a season), and only when the file exists —
    // otherwise fall through to the episode-artwork path (series poster),
    // which matches Jellyfin's own season fallback.
    if synth_id && index == 0 {
        if let Some(local) = season_sidecar_path(&item, id_str, role).await {
            return serve_local_artwork(
                &local,
                role,
                head_only,
                format,
                state,
                req_width,
                if_none_match,
            )
            .await;
        }
    }
    // T9-series — a synth Series/Season tile prefers the enricher's cached TVDB
    // poster/backdrop over the representative episode's own frame/still (an
    // episode still is a poor show tile). Ranked after a season-specific local
    // sidecar (above) but before the episode-artwork path (below). Bare shows
    // (never matched) fall straight through, exactly as before.
    if synth_id && index == 0 {
        if let Some(series_art) = series_metadata_art_path(state, &item, role).await {
            return serve_local_artwork(
                &series_art,
                role,
                head_only,
                format,
                state,
                req_width,
                if_none_match,
            )
            .await;
        }
    }
    // An episode's own Primary row is the series-folder poster (the sidecar
    // provider probes the show folder for episodes, and synth series/season
    // tiles rely on that), so serve the episode's own still ahead of it.
    if !synth_id
        && index == 0
        && matches!(role, ImageRole::Primary)
        && matches!(item.kind, pharos_core::MediaKind::Episode)
    {
        if let Some(still) = episode_still_path(state, &item).await {
            return serve_local_artwork(
                &still,
                role,
                head_only,
                format,
                state,
                req_width,
                if_none_match,
            )
            .await;
        }
    }
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
            if matches!(role, ImageRole::Thumb) {
                metrics::counter!("pharos_image_thumb_source_total", "source" => "artwork")
                    .increment(1);
            }
            return serve_local_artwork(
                &local,
                role,
                head_only,
                format,
                state,
                req_width,
                if_none_match,
            )
            .await;
        }
    }
    // B122 — a film's Thumb. Neither provider publishes a thumb image for a
    // movie (`parse_tmdb_images` maps posters→Primary, backdrops→Backdrop,
    // logos→Logo, and TVDB the same), so an enriched movie carries a Backdrop
    // row and NO Thumb row, and the frame-extract fallback below served an
    // arbitrary frame from `image_seek_seconds` — the dark title card users see
    // on every landscape card. A backdrop IS the curated landscape still a
    // Thumb wants, so serve it. Frame extraction stays the last resort, for
    // items with no artwork at all (unmatched, no sidecar).
    if index == 0 && matches!(role, ImageRole::Thumb) {
        if let Some(backdrop) = local_artwork_path(state, id, ImageRole::Backdrop).await {
            metrics::counter!("pharos_image_thumb_source_total", "source" => "backdrop")
                .increment(1);
            return serve_local_artwork(
                &backdrop,
                role,
                head_only,
                format,
                state,
                req_width,
                if_none_match,
            )
            .await;
        }
        metrics::counter!("pharos_image_thumb_source_total", "source" => "frame").increment(1);
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
    let jpeg_path = match cache
        .fetch_3d(
            id,
            role,
            item.kind,
            &item.path,
            index,
            item.probe.duration_ms,
            item.probe.video_3d_format,
        )
        .await
    {
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
    // The extracted/stored cache jpeg is full source resolution (a 1080p video
    // frame, a full-size embedded cover); it is sized and encoded for the
    // client exactly like a local sidecar.
    serve_local_artwork(
        &jpeg_path,
        role,
        head_only,
        format,
        state,
        req_width,
        if_none_match,
    )
    .await
}

/// The `ImageTags.Primary` a synthesised folder (library / album / artist)
/// must advertise for jellyfin-web to fetch its image at all.
///
/// V82 — a capability field a client GATES a request on is answered from the
/// same source that would serve the request. `SynthItemDto::folder` hardcoded
/// `image_tags: None`, so jellyfin-web never called
/// `/Items/{id}/Images/Primary` for an album or a library and rendered the
/// flat icon fallback instead. The endpoint worked the whole time: an Elbow
/// album whose twelve tracks all carry embedded art showed a blank tile, while
/// the SAME art rendered in the now-playing bar (which requests the TRACK id,
/// carrying a real tag). The advertisement, not the image, was missing.
///
/// Resolution goes through `resolve_synth_image_item`, i.e. the same lookup
/// that serves the request, so the tag cannot promise an image the endpoint
/// would 404 on. `None` when the group has no representative — the icon
/// fallback is then correct rather than a guaranteed 404 per tile.
///
/// The tag VALUE is the representative item's id: opaque to the client (it
/// only round-trips it as `?tag=`), but stable, and it changes if the
/// representative does, which is what lets a cached tile refresh.
pub(crate) async fn synth_primary_tag(
    state: &AppState,
    id_str: &str,
) -> Option<std::collections::BTreeMap<String, String>> {
    let item = resolve_synth_image_item(state, id_str).await?;
    // B149 — the same rule real items follow (`dto::image_tags_for`): a video
    // representative can always serve a Primary (a frame is extracted on
    // demand), but an AUDIO track's only Primary source is cached artwork, so
    // advertising one it does not have makes every grid render 404. The synth
    // path resolved a representative and stamped a tag unconditionally, which
    // is how a library with no covers still told the Google TV app to fetch one
    // per tile. A book has no frame either (B251).
    let has_frames = matches!(
        item.kind,
        pharos_core::MediaKind::Movie | pharos_core::MediaKind::Episode
    );
    if !has_frames && !item.has_primary_art {
        return None;
    }
    let mut tags = std::collections::BTreeMap::new();
    tags.insert("Primary".to_string(), item.id.to_string());
    Some(tags)
}

/// Stamp `ImageTags.Primary` onto every synth folder in a list that has a
/// representative image. One warm of the synth map covers the whole page, so
/// a 279-artist grid costs one scan rather than 279.
pub(crate) async fn stamp_synth_image_tags(
    state: &AppState,
    items: &mut [pharos_jellyfin_api::dto::SynthItemDto],
) {
    for it in items.iter_mut() {
        // EMPTY counts as absent. The album/artist builders set
        // `Some(Default::default())` — an empty map — which serialises as
        // `"ImageTags": {}` and reads to jellyfin-web exactly like no image at
        // all. Testing only for `None` skipped every one of them.
        let missing = match it.image_tags.as_ref() {
            None => true,
            Some(m) => m.is_empty(),
        };
        if missing {
            if let Some(tags) = synth_primary_tag(state, &it.id).await {
                it.image_tags = Some(tags);
            }
        }
    }
}

/// Resolve a synthesised Series/Season/Artist/Album wire id (a 32-hex hash,
/// no stored row) to a representative member item whose frame / cover stands
/// in as the group's poster. Series/Season pick the lowest `(season, episode)`
/// so the poster is stable (usually S01E01); Artist/Album pick the first track.
/// Returns `None` for an id matching no group (→ 404).
pub(crate) async fn resolve_synth_image_item(
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
    let mut confirmed_at = state.synth_image_warm.lock().await;
    if let Some(cached) = state.synth_image_cached(id_str) {
        return match cached {
            Some(item_id) => state.stores.get(item_id).await.ok(),
            None => None,
        };
    }
    let all = state.list_items_cached().await.ok()?;
    build_synth_image_map(state, &all);
    if let Some(Some(item_id)) = state.synth_image_cached(id_str) {
        return all.iter().find(|it| it.id == item_id).cloned();
    }
    // The cached list can be a minute behind a new album, and the negative
    // below lasts until artwork changes, so confirm it against the store,
    // once per generation: a page of unknown ids must not scan per id.
    let generation = state.list_generation();
    if *confirmed_at == Some(generation) {
        state.synth_image_remember(id_str, None);
        return None;
    }
    let all = state.stores.list().await.ok()?;
    *confirmed_at = Some(generation);
    build_synth_image_map(state, &all);
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
        // B154 — an artist/album tile can only ever show a track's CACHED
        // cover (`synth_primary_tag` refuses to advertise a Primary for an
        // audio row without one), so the representative must be a track that
        // HAS art whenever the group contains one. Ranking by title alone
        // picked the alphabetically-first track, which is rarely the one the
        // MusicBrainz backfill matched: 24 albums held a cover on some track
        // and still rendered as blank tiles. Same `has_primary_art`-first key
        // the library tiles below already use; title still breaks ties, so the
        // choice stays stable across restarts.
        let art_rank = if it.has_primary_art { '0' } else { '1' };
        let track_key = format!("{art_rank}{}", it.title.to_lowercase());
        if let Some(a) = it.probe.artist.as_deref() {
            consider(artist_id_for(a), it.id, track_key.clone());
        }
        // The artist listing files a track under its album artist, so an
        // album-artist-only name ("Various Artists") needs an entry too, or
        // every lookup of it misses the map and rescans the store.
        if let Some(a) = crate::api::jellyfin::items::filed_artist(it)
            .filter(|a| Some(*a) != it.probe.artist.as_deref())
        {
            consider(artist_id_for(a), it.id, track_key.clone());
        }
        if let Some(a) = it.probe.album.as_deref() {
            consider(album_id_for(a), it.id, track_key);
        }
    }
    // A LIBRARY is a group too, and until now the only one without a
    // representative image: jellyfin-web renders a CollectionFolder with no
    // Primary as a flat coloured icon tile, which is why "My Media" looked
    // nothing like Jellyfin's. Same machinery, one more id per item.
    //
    // The key is prefixed by whether the item HAS art, so an item with a real
    // poster always outranks one whose image would have to be extracted from a
    // video frame — a library tile is the first thing seen on the home screen
    // and a black frame there is worse than none. Title breaks ties, so the
    // choice is stable across restarts rather than wandering per scan.
    for lib in state.libraries().iter() {
        let root = std::path::Path::new(&lib.root_path);
        for it in all {
            if !it.path.starts_with(root) {
                continue;
            }
            let rank = if it.has_primary_art { '0' } else { '1' };
            consider(
                lib.wire_id.clone(),
                it.id,
                format!("{rank}{}", it.title.to_lowercase()),
            );
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

/// Resolve a SEASON-level sidecar for a synth season id: when the requested
/// wire id is the season id of the representative episode's (folder, name,
/// season), probe the series root for the Kodi/Jellyfin per-season art
/// convention — `season{NN}-poster.jpg` (zero-padded AND bare), with
/// `season-specials-*` as the season-0 alias, across the same extension set
/// the scanner's sidecar provider accepts. Series ids (and everything else)
/// return `None` and keep the existing resolution. A handful of `stat`s per
/// season-image request; season pages are rare enough that no memo is needed.
async fn season_sidecar_path(
    item: &pharos_core::MediaItem,
    id_str: &str,
    role: ImageRole,
) -> Option<std::path::PathBuf> {
    use crate::api::jellyfin::dto::season_id_for_key;
    let s = item.series.as_ref()?;
    let n = s.season_number?;
    let folder = s.series_folder.as_deref()?;
    if !season_id_for_key(Some(folder), &s.series_name, n).eq_ignore_ascii_case(id_str) {
        return None; // a series (or foreign) id — not this episode's season
    }
    let suffix = match role {
        ImageRole::Primary => "poster",
        ImageRole::Backdrop => "fanart",
        ImageRole::Banner => "banner",
        _ => return None,
    };
    let mut bases = vec![
        format!("season{n:02}-{suffix}"),
        format!("season{n}-{suffix}"),
    ];
    if n == 0 {
        bases.push(format!("season-specials-{suffix}"));
    }
    for base in bases {
        for ext in ["png", "jpg", "jpeg", "webp"] {
            let candidate = std::path::Path::new(folder).join(format!("{base}.{ext}"));
            if tokio::fs::try_exists(&candidate).await.unwrap_or(false) {
                return Some(candidate);
            }
        }
    }
    None
}

/// T9-series — the cached TVDB poster/backdrop for a synth Series/Season id,
/// if the series enricher matched this show and the file still exists. `item`
/// is the representative episode the synth id resolved to; its
/// [`SeriesInfo::series_key`] keys the `series_metadata` row. Only Primary
/// (poster) and Backdrop have a series-level asset; every other role returns
/// `None` and keeps the existing resolution. Best-effort — a store error or a
/// recorded-but-missing file falls through (V6 spirit).
///
/// [`SeriesInfo::series_key`]: pharos_core::SeriesInfo::series_key
async fn series_metadata_art_path(
    state: &AppState,
    item: &pharos_core::MediaItem,
    role: ImageRole,
) -> Option<std::path::PathBuf> {
    use pharos_core::SeriesMetadataStore;
    if !matches!(
        role,
        ImageRole::Primary | ImageRole::Backdrop | ImageRole::Logo
    ) {
        return None;
    }
    let series = item.series.as_ref()?;
    let key = series.series_key().to_string();
    // Logo has no DB locator column — it lives at the deterministic
    // series-cache path `upload_series_art` writes. Serve it iff present.
    if role == ImageRole::Logo {
        let cache = state.images.as_ref()?;
        let path =
            pharos_cache::image_cache::series_image_path(cache.root(), ImageRole::Logo, &key);
        return match tokio::fs::try_exists(&path).await {
            Ok(true) => Some(path),
            _ => None,
        };
    }
    let map = state
        .stores
        .series_metadata_by_keys(std::slice::from_ref(&key))
        .await
        .ok()?;
    let meta = map.get(&key)?;
    let locator = match role {
        ImageRole::Primary => meta.poster_locator.clone(),
        ImageRole::Backdrop => meta.backdrop_locator.clone(),
        _ => None,
    }?;
    let path = std::path::PathBuf::from(locator);
    match tokio::fs::try_exists(&path).await {
        Ok(true) => Some(path),
        _ => None,
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

/// LIB-D5 — the recorded on-disk artwork path for `(id, role)`, if a
/// servable source (`local` sidecar from D4 scanning, or a downloaded
/// `tmdb`/`tvdb` cache entry from Task 8) recorded one and the file still
/// exists. A `url` source (not yet downloaded) is deliberately excluded —
/// it has no bytes on disk to serve. Best-effort: a store error or a stale
/// (deleted) file yields `None` so the caller falls through to the upload /
/// frame-extract path rather than 404ing or erroring on a public route (V6
/// spirit).
#[tracing::instrument(name = "image.artwork_lookup", skip_all)]
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
        .find(|(r, source, _)| {
            r.eq_ignore_ascii_case(token) && pharos_core::artwork_source_is_item_servable(source)
        })
        .map(|(_, _, locator)| locator)?;
    let path = std::path::PathBuf::from(locator);
    match tokio::fs::try_exists(&path).await {
        Ok(true) => Some(path),
        _ => {
            tracing::warn!(
                media.id = id,
                role = token,
                path = %path.display(),
                "recorded local artwork missing on disk"
            );
            None
        }
    }
}

/// The still that belongs to episode `item` itself, if one exists on disk: a
/// per-file sidecar beside the media (`<stem>.jpg`, `<stem>-thumb.jpg`, …)
/// first, then a provider's episode image. Show-level art the scanner also
/// records against the episode (`folder.jpg`, `landscape.jpg`) never counts.
async fn episode_still_path(
    state: &AppState,
    item: &pharos_core::MediaItem,
) -> Option<std::path::PathBuf> {
    let rows = match state.stores.artwork_for(item.id).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, media.id = item.id, "artwork lookup failed");
            return None;
        }
    };
    for locator in episode_still_candidates(&item.path, &rows) {
        let path = std::path::PathBuf::from(locator);
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Some(path);
        }
        tracing::warn!(
            media.id = item.id,
            path = %path.display(),
            "recorded episode still missing on disk"
        );
    }
    None
}

/// The locators among an episode's artwork `rows` that are its own image, in
/// serving order: local per-file sidecars (Primary before Thumb), then
/// provider Primary, then provider Thumb.
fn episode_still_candidates<'a>(
    media: &std::path::Path,
    rows: &'a [(String, String, String)],
) -> Vec<&'a str> {
    let stem = media.file_stem().and_then(|s| s.to_str());
    let dir = media.parent();
    let own_sidecar = |locator: &str| {
        let locator = std::path::Path::new(locator);
        let name = locator.file_stem().and_then(|s| s.to_str());
        match (stem, name) {
            (Some(stem), Some(name)) => {
                locator.parent() == dir
                    && name
                        .strip_prefix(stem)
                        .is_some_and(|rest| rest.is_empty() || rest.starts_with('-'))
            }
            _ => false,
        }
    };
    let mut ranked: Vec<(u8, &str)> = rows
        .iter()
        .filter_map(|(role, source, locator)| {
            let primary = role.eq_ignore_ascii_case("primary");
            if !primary && !role.eq_ignore_ascii_case("thumb") {
                return None;
            }
            let rank = if source.eq_ignore_ascii_case("local") {
                if !own_sidecar(locator) {
                    return None;
                }
                0
            } else if pharos_core::artwork_source_is_item_servable(source) {
                2
            } else {
                return None;
            };
            Some((rank + u8::from(!primary), locator.as_str()))
        })
        .collect();
    ranked.sort_by_key(|(rank, _)| *rank);
    ranked.into_iter().map(|(_, locator)| locator).collect()
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

/// LIB-D5 — serve an artwork file (a recorded local sidecar, or the image
/// cache's extracted frame) sized for the client.
///
/// `?format=avif|webp` is answered from the encoded copy once the background
/// encode has made it, and from the sized JPEG until then (marked so the
/// client does not keep it). Encoded and scaled copies live in the cache
/// directory, never in the user's media folder.
#[allow(clippy::too_many_arguments)]
async fn serve_local_artwork(
    path: &std::path::Path,
    role: ImageRole,
    head_only: bool,
    format: ImageFormat,
    state: &AppState,
    req_width: Option<u32>,
    if_none_match: Option<&str>,
) -> Result<HttpResponse, actix_web::Error> {
    // Downscale big poster/fanart sidecars to the client-requested display width
    // (capped per role) and serve the cached copy — a full-res multi-MB sidecar
    // read off NFS was costing 1-4s per tile in a library grid AND made the
    // client decode a giant bitmap (the home-screen scroll OOM). Only opaque
    // photographic roles are scaled or re-encoded; Logo/Banner/Art/Disc are
    // usually small PNGs with alpha that a re-encode would wreck, so they pass
    // through untouched (`effective_width` → None).
    let (Some(width), Some(cache)) = (effective_width(role, req_width), state.images.as_ref())
    else {
        return Ok(deliver_image(
            path,
            content_type_for_ext(path),
            ImageFreshness::Settled,
            head_only,
            if_none_match,
        )
        .await);
    };
    let mut freshness = ImageFreshness::Settled;
    if let Some(encoded) = format.encoded() {
        match cache
            .encoded_artwork(path, width, encoded)
            .instrument(tracing::info_span!("image.encoded_lookup"))
            .await
        {
            pharos_cache::Encoded::Ready(out) => {
                // Compared at the encode's own bucket width, so an off-bucket
                // request does not pit a bigger encode against a smaller JPEG.
                let bucket = pharos_cache::encode_width(width);
                let jpeg = cache
                    .scaled_artwork(path, bucket)
                    .instrument(tracing::info_span!("image.resize", width = bucket))
                    .await;
                let delivery = Delivery::pick(&out, &jpeg).await;
                metrics::counter!(
                    "pharos_image_encoded_served_total",
                    "format" => encoded.ext(),
                    "served" => delivery.label(encoded)
                )
                .increment(1);
                let (file, content_type) = match delivery {
                    Delivery::Encoded => (&out, encoded.content_type()),
                    Delivery::Jpeg => (&jpeg, content_type_for_ext(&jpeg)),
                };
                return Ok(deliver_image(
                    file,
                    content_type,
                    ImageFreshness::Settled,
                    head_only,
                    if_none_match,
                )
                .await);
            }
            pharos_cache::Encoded::Pending => freshness = ImageFreshness::Pending,
            // The JPEG is the final answer for a source that cannot be encoded.
            pharos_cache::Encoded::Unavailable => {}
        }
    }
    let scaled = cache
        .scaled_artwork(path, width)
        .instrument(tracing::info_span!("image.resize", width))
        .await;
    Ok(deliver_image(
        &scaled,
        content_type_for_ext(&scaled),
        freshness,
        head_only,
        if_none_match,
    )
    .await)
}

/// Which of a ready encode and its same-width JPEG is sent: the smaller one.
/// An encode can come out bigger, e.g. from a source that is already a small,
/// heavily compressed JPEG.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delivery {
    Encoded,
    Jpeg,
}

impl Delivery {
    async fn pick(encoded: &std::path::Path, jpeg: &std::path::Path) -> Self {
        let encoded_bytes = tokio::fs::metadata(encoded).await.ok().map(|m| m.len());
        let jpeg_bytes = tokio::fs::metadata(jpeg).await.ok().map(|m| m.len());
        let delivery = match (encoded_bytes, jpeg_bytes) {
            (Some(e), Some(j)) if j < e => Self::Jpeg,
            (Some(_), _) => Self::Encoded,
            // The encode was evicted after the lookup; the JPEG still answers.
            (None, _) => Self::Jpeg,
        };
        tracing::debug!(
            ?encoded_bytes,
            ?jpeg_bytes,
            ?delivery,
            "encoded artwork delivery"
        );
        delivery
    }

    fn label(self, format: pharos_cache::EncodedFormat) -> &'static str {
        match self {
            Self::Encoded => format.ext(),
            Self::Jpeg => "fallback",
        }
    }
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

impl ImageFormat {
    fn encoded(self) -> Option<pharos_cache::EncodedFormat> {
        match self {
            Self::Jpeg => None,
            Self::Webp => Some(pharos_cache::EncodedFormat::Webp),
            Self::Avif => Some(pharos_cache::EncodedFormat::Avif),
        }
    }
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

/// The display width the client asked the image to be delivered at. jellyfin's
/// clients request grid thumbnails with `fillWidth`/`fillHeight` (and older /
/// alternate shapes `maxWidth`/`maxHeight`/`width`/`height`) — e.g. an Android
/// TV home row asks for `fillWidth=300`. pharos previously ignored these and
/// shipped the full-resolution artwork (a 1280-wide backdrop, a 480×720
/// poster), so the client decoded a multi-MB bitmap per tile; scrolling a
/// library / home screen piled up graphics memory and, on a RAM-tight TV, the
/// LMK SIGKILL'd the app. Honour the request so the wire + decoded bitmap match
/// what's shown.
///
/// Width-type params win (the scaler is width-driven, aspect preserved); a
/// height-only request falls back to using the height as the width bound —
/// imprecise but still caps memory, and the client rescales for display. `0`
/// / unparseable values are ignored (Jellyfin treats them as "unset").
fn requested_width(qs: &str) -> Option<u32> {
    let mut width_kind: Option<u32> = None;
    let mut height_kind: Option<u32> = None;
    for kv in qs.split('&') {
        let Some((k, v)) = kv.split_once('=') else {
            continue;
        };
        let Ok(n) = v.parse::<u32>() else { continue };
        if n == 0 {
            continue;
        }
        if k.eq_ignore_ascii_case("fillWidth")
            || k.eq_ignore_ascii_case("maxWidth")
            || k.eq_ignore_ascii_case("width")
        {
            // Smallest requested width wins — several params may be present.
            width_kind = Some(width_kind.map_or(n, |c| c.min(n)));
        } else if k.eq_ignore_ascii_case("fillHeight")
            || k.eq_ignore_ascii_case("maxHeight")
            || k.eq_ignore_ascii_case("height")
        {
            height_kind = Some(height_kind.map_or(n, |c| c.min(n)));
        }
    }
    width_kind.or(height_kind)
}

/// `imageIndex` from the query string.
///
/// The jellyfin-sdk-kotlin clients (Android / Google TV) address an indexed
/// image as `/Images/{type}?imageIndex=N` where jellyfin-web uses
/// `/Images/{type}/{N}`. Both are Jellyfin-legal; pharos only routed the path
/// form, so every Android TV chapter thumbnail 400'd.
fn requested_image_index(qs: &str) -> u32 {
    for kv in qs.split('&') {
        let Some((k, v)) = kv.split_once('=') else {
            continue;
        };
        if k.eq_ignore_ascii_case("imageIndex") {
            if let Ok(n) = v.parse::<u32>() {
                return n;
            }
        }
    }
    0
}

/// Cap a client-requested width by the role's sane maximum so a client asking
/// for a huge size can't force a giant re-encode, while a smaller request (the
/// common grid-thumbnail case) is honoured exactly. `None` role cap (Logo /
/// Banner / Art / Disc — small alpha PNGs a JPEG rescale would wreck) means
/// "don't rescale": return None so the original is served untouched.
fn effective_width(role: ImageRole, requested: Option<u32>) -> Option<u32> {
    let cap = match role {
        ImageRole::Primary => 480,
        ImageRole::Thumb => 640,
        ImageRole::Backdrop => 1280,
        _ => return None,
    };
    Some(requested.map_or(cap, |w| w.min(cap)))
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
    // B124 — the bytes behind an unchanged URL just changed. Rotate the item's
    // image tags or every client keeps its week-cached copy of the old picture.
    bump_art_version(state, id).await;
    state.notify_library_changed();
    Ok(HttpResponse::NoContent().finish())
}

/// B124 — record that an item's artwork bytes were replaced, so every
/// `ImageTag` it advertises rotates and clients request the new picture.
///
/// Best-effort: the art is already on disk and serving it is more useful than
/// failing the request, so a store error is logged (naming the item) rather
/// than surfaced — but it IS logged, because the visible symptom of a silent
/// failure here is "the download button does nothing".
pub(crate) async fn bump_art_version(state: &AppState, id: u64) {
    use pharos_core::MediaStore;
    if let Err(e) = state.stores.bump_art_version(id).await {
        tracing::warn!(
            error = %e,
            media.id = id,
            "artwork replaced but its version could not be bumped; clients will \
             keep serving the previous image from cache until it expires"
        );
    }
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
    bump_art_version(state, id).await;
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
    fn episode_still_candidates_rank_own_sidecars_then_provider_stills() {
        let row = |role: &str, source: &str, locator: &str| {
            (role.to_owned(), source.to_owned(), locator.to_owned())
        };
        let rows = vec![
            row("Primary", "local", "/tv/Show/folder.jpg"),
            row("Thumb", "tvdb", "/cache/thumb/episode/1.jpg"),
            row("Primary", "tvdb", "/cache/primary/episode/1.jpg"),
            row("Thumb", "local", "/tv/Show/S1/Show S01E01-thumb.jpg"),
            row("Thumb", "local", "/tv/Show/S1/landscape.jpg"),
            row("Thumb", "local", "/tv/Show/S1/Show S01E010-thumb.jpg"),
            row("Primary", "local", "/tv/Show/S1/Show S01E01.jpg"),
            row("Logo", "local", "/tv/Show/S1/Show S01E01-logo.png"),
            row("Primary", "upload", "/cache/upload/1.jpg"),
        ];
        assert_eq!(
            episode_still_candidates(std::path::Path::new("/tv/Show/S1/Show S01E01.mkv"), &rows),
            vec![
                "/tv/Show/S1/Show S01E01.jpg",
                "/tv/Show/S1/Show S01E01-thumb.jpg",
                "/cache/primary/episode/1.jpg",
                "/cache/thumb/episode/1.jpg",
            ]
        );
    }

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

    #[::core::prelude::v1::test]
    fn requested_width_parses_jellyfin_size_params() {
        // The Android TV home row asks for fillWidth=300.
        assert_eq!(
            requested_width("fillWidth=300&fillHeight=450&tag=x"),
            Some(300)
        );
        assert_eq!(requested_width("maxWidth=250"), Some(250));
        assert_eq!(requested_width("width=200"), Some(200));
        // Height-only falls back to the height value as the width bound.
        assert_eq!(requested_width("fillHeight=450"), Some(450));
        // Width-type wins over height-type when both present.
        assert_eq!(requested_width("fillHeight=450&fillWidth=300"), Some(300));
        // Smallest width wins when several are given.
        assert_eq!(requested_width("maxWidth=600&fillWidth=300"), Some(300));
        // Zero / unparseable / absent → None (Jellyfin treats as unset).
        assert_eq!(requested_width("fillWidth=0"), None);
        assert_eq!(requested_width("fillWidth=abc"), None);
        assert_eq!(requested_width("tag=x&quality=90"), None);
        assert_eq!(requested_width(""), None);
    }

    #[::core::prelude::v1::test]
    fn effective_width_caps_by_role_and_honours_smaller_requests() {
        // A 300px grid request is honoured exactly (below every cap).
        assert_eq!(effective_width(ImageRole::Primary, Some(300)), Some(300));
        assert_eq!(effective_width(ImageRole::Backdrop, Some(300)), Some(300));
        // An oversized request is capped at the role max.
        assert_eq!(effective_width(ImageRole::Primary, Some(4000)), Some(480));
        assert_eq!(effective_width(ImageRole::Backdrop, Some(4000)), Some(1280));
        // No request → the role's default cap (prior behaviour).
        assert_eq!(effective_width(ImageRole::Thumb, None), Some(640));
        // Alpha roles never rescale (JPEG re-encode would wreck them).
        assert_eq!(effective_width(ImageRole::Logo, Some(300)), None);
        assert_eq!(effective_width(ImageRole::Banner, None), None);
    }

    /// Create an unrestricted user and issue a token for it.
    async fn token_for(stores: &Stores) -> String {
        use pharos_core::{SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore};
        let id = UserId::new();
        stores
            .create(UserRecord {
                id,
                name: "tester".into(),
                password_hash: SecretString::new("x"),
                policy: UserPolicy::default(),
            })
            .await
            .unwrap();
        stores
            .issue(id, "test")
            .await
            .unwrap()
            .0
            .expose()
            .to_string()
    }

    async fn seed_state() -> web::Data<crate::state::AppState> {
        let stores = Stores::connect("sqlite::memory:").await.unwrap();
        web::Data::new(crate::state::AppState::new(stores, "t".into()))
    }

    #[actix_web::test]
    async fn known_type_returns_404_not_500() {
        let state = seed_state().await;
        let token = token_for(&state.stores).await;
        let app = test::init_service(App::new().app_data(state).configure(register)).await;
        for t in ["primary", "backdrop", "thumb", "logo", "banner", "art"] {
            let req = test::TestRequest::get()
                .insert_header(("X-Emby-Token", token.clone()))
                .uri(&format!("/items/abc/images/{t}"))
                .to_request();
            let resp = test::call_service(&app, req).await;
            assert_eq!(resp.status(), 404, "type={t}");
        }
    }

    #[actix_web::test]
    async fn indexed_route_404s() {
        let state = seed_state().await;
        let token = token_for(&state.stores).await;
        let app = test::init_service(App::new().app_data(state).configure(register)).await;
        let req = test::TestRequest::get()
            .insert_header(("X-Emby-Token", token.clone()))
            .uri("/items/abc/images/backdrop/0")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 404);
    }

    #[actix_web::test]
    async fn unknown_type_returns_400() {
        let state = seed_state().await;
        let token = token_for(&state.stores).await;
        let app = test::init_service(App::new().app_data(state).configure(register)).await;
        let req = test::TestRequest::get()
            .insert_header(("X-Emby-Token", token.clone()))
            .uri("/items/abc/images/bogus")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 400);
    }

    #[actix_web::test]
    async fn head_request_returns_no_body_404() {
        let state = seed_state().await;
        let token = token_for(&state.stores).await;
        let app = test::init_service(App::new().app_data(state).configure(register)).await;
        let req = test::TestRequest::default()
            .insert_header(("X-Emby-Token", token.clone()))
            .method(actix_web::http::Method::HEAD)
            .uri("/items/abc/images/primary")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 404);
    }

    #[actix_web::test]
    async fn images_serve_anonymous_and_credentialed_requests() {
        let state = seed_state().await;
        let token = token_for(&state.stores).await;
        let app = test::init_service(App::new().app_data(state).configure(register)).await;
        let uri = "/items/abc/images/primary";

        let anon = test::call_service(&app, test::TestRequest::get().uri(uri).to_request()).await;
        assert_ne!(anon.status(), 401, "anonymous GET must be served (V225)");

        let query = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("{uri}?api_key={token}"))
                .to_request(),
        )
        .await;
        assert_ne!(query.status(), 401, "api_key query token must authenticate");

        let cookie = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(uri)
                .insert_header(("Cookie", format!("JellyfinAuth={token}")))
                .to_request(),
        )
        .await;
        assert_ne!(
            cookie.status(),
            401,
            "JellyfinAuth cookie must authenticate"
        );
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
    async fn album_representative_prefers_the_track_that_has_a_cover() {
        // B154 — the MusicBrainz backfill records the cover against ONE track
        // of an album, and which one is unrelated to track title. Picking the
        // alphabetically-first track as the album's representative therefore
        // landed on a coverless row most of the time, and `synth_primary_tag`
        // (correctly, per B149) refuses to advertise a Primary for an audio
        // row with no art — so the album tile stayed blank while the album
        // demonstrably had a cover. Live count when this was written: 24 of
        // 105 albums with art rendered blank for exactly this reason.
        use pharos_core::{MediaItem, MediaKind, MediaProbe, MediaStore};
        let cache_dir = tempfile::tempdir().unwrap();
        let state = seed_state_with_cache(cache_dir.path()).await;
        let track = |id: u64, title: &str| MediaItem {
            id,
            path: format!("/m/{id}.flac").into(),
            title: title.into(),
            kind: MediaKind::Audio,
            probe: MediaProbe {
                album: Some("Toxicity".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        // "Aerials" sorts first; the cover lives on "Toxicity".
        state.stores.put(track(1, "Aerials")).await.unwrap();
        state.stores.put(track(2, "Toxicity")).await.unwrap();
        state
            .stores
            .set_artwork(2, "Primary", "musicbrainz", "/cache/toxicity.jpg")
            .await
            .unwrap();

        let album_id = crate::api::jellyfin::dto::album_id_for("Toxicity");
        let tags = synth_primary_tag(&state, &album_id)
            .await
            .expect("an album with a cover on any track must advertise Primary");
        assert_eq!(
            tags.get("Primary").map(String::as_str),
            Some("2"),
            "the representative must be the track that actually has the cover"
        );
    }

    #[actix_web::test]
    async fn an_album_artist_with_no_track_of_its_own_resolves_from_the_map() {
        // The artist listing files tracks under their album artist, but the
        // map only knew track artists: each "Various Artists" tile missed it
        // and read the whole store, five full scans per music artist page.
        use pharos_core::{MediaItem, MediaKind, MediaProbe, MediaStore};
        let cache_dir = tempfile::tempdir().unwrap();
        let state = seed_state_with_cache(cache_dir.path()).await;
        state
            .stores
            .put(MediaItem {
                id: 7,
                path: "/m/7.flac".into(),
                title: "Chop Suey!".into(),
                kind: MediaKind::Audio,
                probe: MediaProbe {
                    artist: Some("System of a Down".into()),
                    album_artist: Some("Various Artists".into()),
                    album: Some("Hits".into()),
                    ..Default::default()
                },
                ..Default::default()
            })
            .await
            .unwrap();
        state
            .stores
            .set_artwork(7, "Primary", "musicbrainz", "/cache/hits.jpg")
            .await
            .unwrap();

        let all = state.list_items_cached().await.unwrap();
        build_synth_image_map(&state, &all);
        let various = crate::api::jellyfin::dto::artist_id_for("Various Artists");
        assert_eq!(state.synth_image_cached(&various), Some(Some(7)));
        let tags = synth_primary_tag(&state, &various)
            .await
            .expect("an album artist whose track has a cover must advertise Primary");
        assert_eq!(tags.get("Primary").map(String::as_str), Some("7"));
    }

    #[actix_web::test]
    async fn unknown_synth_ids_confirm_against_the_store_once_per_generation() {
        // Each unknown id used to read the whole store to confirm its
        // negative; a page of them paid one full scan each.
        use pharos_core::{MediaItem, MediaKind, MediaProbe, MediaStore};
        let cache_dir = tempfile::tempdir().unwrap();
        let state = seed_state_with_cache(cache_dir.path()).await;
        let track = |id: u64, album: &str| MediaItem {
            id,
            path: format!("/m/{id}.flac").into(),
            title: "T".into(),
            kind: MediaKind::Audio,
            probe: MediaProbe {
                album: Some(album.into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let album = crate::api::jellyfin::dto::album_id_for;
        state.stores.put(track(1, "Old")).await.unwrap();
        assert!(resolve_synth_image_item(&state, &album("Gone"))
            .await
            .is_none());

        // Stored without a generation bump: only a fresh store read finds it.
        state.stores.put(track(2, "New")).await.unwrap();
        assert!(
            resolve_synth_image_item(&state, &album("New"))
                .await
                .is_none(),
            "a second miss at the same generation must not rescan the store"
        );

        state.notify_library_changed();
        let found = resolve_synth_image_item(&state, &album("New")).await;
        assert_eq!(found.map(|it| it.id), Some(2));
    }

    #[actix_web::test]
    async fn a_library_of_coverless_books_advertises_no_primary() {
        // B251 — a book has no video frame to extract, so like an audio track
        // its only Primary is a cover. The library tile advertised its
        // coverless representative book anyway and every client got a 404.
        use pharos_core::{Library, LibraryKind, MediaItem, MediaKind, MediaStore};
        let cache_dir = tempfile::tempdir().unwrap();
        let state = seed_state_with_cache(cache_dir.path()).await;
        state.set_libraries(vec![Library {
            id: 1,
            name: "Books".into(),
            root_path: "/b".into(),
            kind: LibraryKind::Books,
            wire_id: "b".repeat(32),
        }]);
        state
            .stores
            .put(MediaItem {
                id: 9,
                path: "/b/plain.epub".into(),
                title: "Plain".into(),
                kind: MediaKind::Book,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(synth_primary_tag(&state, &"b".repeat(32)).await.is_none());

        state
            .stores
            .set_artwork(9, "Primary", "local", "/cache/plain.jpg")
            .await
            .unwrap();
        state.note_artwork_changed();
        let tags = synth_primary_tag(&state, &"b".repeat(32))
            .await
            .expect("a library whose book has a cover advertises it");
        assert_eq!(tags.get("Primary").map(String::as_str), Some("9"));
    }

    #[actix_web::test]
    async fn artwork_landing_after_the_memo_warmed_re_picks_the_representative() {
        // B155 — the synth-image memo pins the representative for the life of
        // the pod. The backfill runs for hours after boot, so an album whose
        // tile was first rendered before its cover downloaded kept resolving to
        // the coverless track it was warmed with — permanently blank until a
        // restart, which is why covers "arrived" only after a deploy.
        use pharos_core::{MediaItem, MediaKind, MediaProbe, MediaStore};
        let cache_dir = tempfile::tempdir().unwrap();
        let state = seed_state_with_cache(cache_dir.path()).await;
        let track = |id: u64, title: &str| MediaItem {
            id,
            path: format!("/m/{id}.flac").into(),
            title: title.into(),
            kind: MediaKind::Audio,
            probe: MediaProbe {
                album: Some("Hypnotize".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        state.stores.put(track(1, "Attack")).await.unwrap();
        state.stores.put(track(2, "Lonely Day")).await.unwrap();

        let album_id = crate::api::jellyfin::dto::album_id_for("Hypnotize");
        // Warmed while the album had no art at all: no tag, memo now pinned.
        assert!(synth_primary_tag(&state, &album_id).await.is_none());

        state
            .stores
            .set_artwork(2, "Primary", "musicbrainz", "/cache/hypnotize.jpg")
            .await
            .unwrap();
        state.note_artwork_changed();

        let tags = synth_primary_tag(&state, &album_id)
            .await
            .expect("a cover that landed after the memo warmed must be picked up");
        assert_eq!(tags.get("Primary").map(String::as_str), Some("2"));
    }

    #[actix_web::test]
    async fn a_head_advertises_the_length_the_get_would_send() {
        // B166 — `.finish()` on the HEAD path declared an empty body, and
        // actix derives Content-Length from the body size while DROPPING a
        // manually-inserted header, so every image HEAD said 0. Observed live
        // as `content-length: 0` next to an ETag whose own prefix encoded
        // 157,350 bytes. Same trap as B101 on the video path.
        use pharos_core::MediaStore;
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let poster = dir.path().join("poster.png");
        std::fs::write(&poster, PNG_1X1).unwrap();
        let state = seed_state_with_cache(cache_dir.path()).await;
        put_movie(&state, 77, &dir.path().join("movie.mkv")).await;
        state
            .stores
            .set_artwork(77, "Primary", "local", &poster.to_string_lossy())
            .await
            .unwrap();

        let token = token_for(&state.stores).await;
        let app = test::init_service(App::new().app_data(state.clone()).configure(register)).await;

        let get = test::call_service(
            &app,
            test::TestRequest::get()
                .insert_header(("X-Emby-Token", token.clone()))
                .uri("/items/77/images/primary")
                .to_request(),
        )
        .await;
        let get_len = test::read_body(get).await.len();
        assert_eq!(get_len, PNG_1X1.len(), "GET serves the sidecar");

        let head = test::call_service(
            &app,
            test::TestRequest::with_uri("/items/77/images/primary")
                .insert_header(("X-Emby-Token", token.clone()))
                .method(actix_web::http::Method::HEAD)
                .to_request(),
        )
        .await;
        assert_eq!(head.status(), 200);
        // The Content-Length header is stamped by the h1 encoder at wire time,
        // from the body's declared size — which is precisely the value that
        // was wrong. Assert the mechanism rather than a header the in-process
        // test harness never materialises.
        use actix_web::body::MessageBody;
        assert_eq!(
            head.into_body().size(),
            actix_web::body::BodySize::Sized(get_len as u64),
            "a HEAD must declare the length the GET would send"
        );
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

        let token = token_for(&state.stores).await;
        let app = test::init_service(App::new().app_data(state.clone()).configure(register)).await;
        let req = test::TestRequest::get()
            .insert_header(("X-Emby-Token", token.clone()))
            .uri("/items/42/images/primary")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers().get("content-type").unwrap(), "image/png");
        let body = test::read_body(resp).await;
        assert_eq!(body.as_ref(), PNG_1X1, "served bytes must be the sidecar");
    }

    #[actix_web::test]
    async fn avif_is_served_once_encoded_and_the_stand_in_is_not_kept() {
        use pharos_core::MediaStore;
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        // A real decodable image: the encode runs the real ffmpeg on it.
        let poster = dir.path().join("poster.jpg");
        let made = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i"])
            .args(["testsrc=s=800x1200", "-frames:v", "1", "-y"])
            .arg(&poster)
            .status()
            .unwrap();
        assert!(made.success());
        let state = seed_state_with_cache(cache_dir.path()).await;
        put_movie(&state, 56, &dir.path().join("movie.mkv")).await;
        state
            .stores
            .set_artwork(56, "Primary", "local", &poster.to_string_lossy())
            .await
            .unwrap();
        let app = test::init_service(App::new().app_data(state).configure(register)).await;
        let get = |uri: &'static str| test::TestRequest::get().uri(uri).to_request();
        let header = |resp: &actix_web::dev::ServiceResponse, name: &str| {
            resp.headers()
                .get(name)
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned()
        };

        let cold = test::call_service(&app, get("/items/56/images/primary?format=avif")).await;
        assert_eq!(cold.status(), 200);
        assert_eq!(header(&cold, "content-type"), "image/jpeg");
        assert_eq!(header(&cold, "cache-control"), IMAGE_PENDING_CACHE_CONTROL);

        let mut warm = None;
        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let resp = test::call_service(&app, get("/items/56/images/primary?format=avif")).await;
            if header(&resp, "content-type") == "image/avif" {
                warm = Some(resp);
                break;
            }
        }
        let warm = warm.expect("the queued encode never landed");
        assert_eq!(header(&warm, "cache-control"), image_cache_control());
        let body = test::read_body(warm).await;
        assert_eq!(&body[4..12], b"ftypavif", "the body must be an AVIF file");

        let plain = test::call_service(&app, get("/items/56/images/primary")).await;
        assert_eq!(header(&plain, "content-type"), "image/jpeg");
        assert_eq!(header(&plain, "cache-control"), image_cache_control());

        // An encode bigger than the JPEG loses to it, for good.
        for entry in std::fs::read_dir(cache_dir.path().join("encoded")).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "avif") {
                std::fs::write(&path, vec![0u8; 4 << 20]).unwrap();
            }
        }
        let bigger = test::call_service(&app, get("/items/56/images/primary?format=avif")).await;
        assert_eq!(header(&bigger, "content-type"), "image/jpeg");
        assert_eq!(header(&bigger, "cache-control"), image_cache_control());
    }

    #[actix_web::test]
    async fn served_image_carries_cache_control_and_etag() {
        // B89 — a poster served with no Cache-Control/ETag made the gallery
        // client re-download every tile on every render. A 200 must now carry a
        // max-age Cache-Control and an ETag so the client caches it.
        use pharos_core::MediaStore;
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let media_path = dir.path().join("movie.mkv");
        let poster = dir.path().join("poster.png");
        std::fs::write(&poster, PNG_1X1).unwrap();
        let state = seed_state_with_cache(cache_dir.path()).await;
        put_movie(&state, 55, &media_path).await;
        state
            .stores
            .set_artwork(55, "Primary", "local", &poster.to_string_lossy())
            .await
            .unwrap();
        let token = token_for(&state.stores).await;
        let app = test::init_service(App::new().app_data(state).configure(register)).await;
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .insert_header(("X-Emby-Token", token.clone()))
                .uri("/items/55/images/primary")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let cc = resp
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(cc.contains("max-age="), "must set a max-age: {cc:?}");
        assert!(
            resp.headers().get("etag").is_some(),
            "must set an ETag for revalidation"
        );
    }

    #[actix_web::test]
    async fn matching_if_none_match_yields_304() {
        // B89 — once the client holds the current ETag, a conditional request
        // must revalidate to 304 (no body) rather than re-shipping the bytes.
        use pharos_core::MediaStore;
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let poster = dir.path().join("poster.png");
        std::fs::write(&poster, PNG_1X1).unwrap();
        let state = seed_state_with_cache(cache_dir.path()).await;
        put_movie(&state, 56, &dir.path().join("movie.mkv")).await;
        state
            .stores
            .set_artwork(56, "Primary", "local", &poster.to_string_lossy())
            .await
            .unwrap();
        let token = token_for(&state.stores).await;
        let app = test::init_service(App::new().app_data(state).configure(register)).await;
        let first = test::call_service(
            &app,
            test::TestRequest::get()
                .insert_header(("X-Emby-Token", token.clone()))
                .uri("/items/56/images/primary")
                .to_request(),
        )
        .await;
        assert_eq!(first.status(), 200);
        let etag = first
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .unwrap()
            .to_string();
        let second = test::call_service(
            &app,
            test::TestRequest::get()
                .insert_header(("X-Emby-Token", token.clone()))
                .uri("/items/56/images/primary")
                .insert_header(("If-None-Match", etag.as_str()))
                .to_request(),
        )
        .await;
        assert_eq!(second.status(), 304, "matching ETag must 304");
        let body = test::read_body(second).await;
        assert!(body.is_empty(), "304 must carry no body");
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
        let token = token_for(&state.stores).await;
        let app = test::init_service(App::new().app_data(state).configure(register)).await;
        let req = test::TestRequest::get()
            .insert_header(("X-Emby-Token", token.clone()))
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
        let token = token_for(&state.stores).await;
        let app = test::init_service(App::new().app_data(state).configure(register)).await;
        let req = test::TestRequest::get()
            .insert_header(("X-Emby-Token", token.clone()))
            .uri("/items/9/images/primary")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 404);
    }

    #[actix_web::test]
    async fn synth_series_primary_serves_cached_tvdb_poster() {
        // T9-series — a synth Series id whose show the enricher matched serves
        // the cached TVDB poster (via series_metadata.poster_locator), ahead of
        // the representative episode's own frame.
        use crate::api::jellyfin::dto::series_id_for_key;
        use pharos_core::{
            MediaItem, MediaKind, MediaStore, SeriesInfo, SeriesMetadata, SeriesMetadataStore,
        };
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let poster = dir.path().join("buffy-poster.png");
        std::fs::write(&poster, PNG_1X1).unwrap();

        let state = seed_state_with_cache(cache_dir.path()).await;
        // One episode of a folder-keyed show — the synth Series id resolves to it.
        let ep = MediaItem {
            id: 501,
            path: dir.path().join("Buffy/S01E01.mkv"),
            title: "Buffy S01E01".into(),
            kind: MediaKind::Episode,
            series: Some(SeriesInfo {
                series_name: "Buffy".into(),
                season_number: Some(1),
                episode_number: Some(1),
                series_folder: Some("/tv/Buffy (1997)".into()),
                series_year: Some(1997),
            }),
            ..Default::default()
        };
        state.stores.put(ep).await.unwrap();
        state
            .stores
            .upsert_series_metadata(SeriesMetadata {
                series_key: "/tv/Buffy (1997)".into(),
                series_name: "Buffy".into(),
                match_source: Some("search".into()),
                poster_locator: Some(poster.to_string_lossy().into_owned()),
                ..Default::default()
            })
            .await
            .unwrap();

        let synth = series_id_for_key(Some("/tv/Buffy (1997)"), "Buffy");
        let token = token_for(&state.stores).await;
        let app = test::init_service(App::new().app_data(state).configure(register)).await;
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .insert_header(("X-Emby-Token", token.clone()))
                .uri(&format!("/items/{synth}/images/primary"))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200, "cached TVDB series poster must serve");
        assert_eq!(resp.headers().get("content-type").unwrap(), "image/png");
        let body = test::read_body(resp).await;
        assert_eq!(
            body.as_ref(),
            PNG_1X1,
            "served bytes must be the cached poster"
        );
    }

    #[actix_web::test]
    async fn synth_series_logo_serves_cached_file_without_db_locator() {
        // Series Logo has no DB locator column — it serves straight from the
        // deterministic `series_image_path` slot `upload_series_art` writes,
        // even with no series_metadata poster/backdrop row.
        use crate::api::jellyfin::dto::series_id_for_key;
        use pharos_cache::image_cache::{series_image_path, ImageRole};
        use pharos_core::{MediaItem, MediaKind, MediaStore, SeriesInfo};

        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let state = seed_state_with_cache(cache_dir.path()).await;

        let series_key = "/tv/Dragon Ball";
        // Write a logo into the deterministic series-cache slot.
        let logo_path = series_image_path(cache_dir.path(), ImageRole::Logo, series_key);
        std::fs::create_dir_all(logo_path.parent().unwrap()).unwrap();
        std::fs::write(&logo_path, PNG_1X1).unwrap();

        let ep = MediaItem {
            id: 601,
            path: dir.path().join("Dragon Ball/S01E01.mkv"),
            title: "Dragon Ball S01E01".into(),
            kind: MediaKind::Episode,
            series: Some(SeriesInfo {
                series_name: "Dragon Ball".into(),
                season_number: Some(1),
                episode_number: Some(1),
                series_folder: Some(series_key.into()),
                series_year: Some(1986),
            }),
            ..Default::default()
        };
        state.stores.put(ep).await.unwrap();

        let synth = series_id_for_key(Some(series_key), "Dragon Ball");
        let token = token_for(&state.stores).await;
        let app = test::init_service(App::new().app_data(state).configure(register)).await;
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .insert_header(("X-Emby-Token", token.clone()))
                .uri(&format!("/items/{synth}/images/logo"))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200, "cached series logo must serve");
        let body = test::read_body(resp).await;
        assert_eq!(
            body.as_ref(),
            PNG_1X1,
            "served bytes must be the cached logo"
        );
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
        let token = token_for(&state.stores).await;
        let app = test::init_service(App::new().app_data(state).configure(register)).await;
        let req = test::TestRequest::get()
            .insert_header(("X-Emby-Token", token.clone()))
            .uri("/items/11/images/primary")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 404);
    }
}
