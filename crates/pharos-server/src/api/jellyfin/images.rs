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

/// A content ETag from the served file's length + mtime — stable while the
/// cached bytes are, and rotates when the cache re-extracts / re-encodes.
/// `None` if the file vanished (the caller then 404s on the body read).
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
    head_only: bool,
    if_none_match: Option<&str>,
) -> HttpResponse {
    use actix_web::http::header::{CACHE_CONTROL, ETAG};
    let etag = file_etag(final_path).await;
    if if_none_match_hit(if_none_match, etag.as_deref()) {
        let mut resp = HttpResponse::NotModified();
        resp.insert_header((CACHE_CONTROL, image_cache_control()));
        if let Some(t) = &etag {
            resp.insert_header((ETAG, t.clone()));
        }
        return resp.finish();
    }
    let mut resp = HttpResponse::Ok();
    resp.content_type(content_type);
    resp.insert_header((CACHE_CONTROL, image_cache_control()));
    if let Some(t) = &etag {
        resp.insert_header((ETAG, t.clone()));
    }
    if head_only {
        return resp.finish();
    }
    match tokio::fs::read(final_path).await {
        Ok(bytes) => resp.body(bytes),
        Err(_) => HttpResponse::NotFound().body(""),
    }
}

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
    req: HttpRequest,
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
    // B89 — chapter thumbs are the heaviest per-item image after B88 turned
    // them on; cache them client-side like every other artwork.
    Ok(deliver_image(&path, "image/jpeg", false, if_none_match_of(&req)).await)
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
    serve_image(
        &state,
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
    serve_image(
        &state,
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
    serve_image(
        &state,
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
    let mut synth_id = false;
    let item = match pharos_jellyfin_api::dto::parse_item_id(id_str).ok_or(()) {
        Ok(id) => match state.stores.get(id).await {
            Ok(it) => it,
            Err(_) => return Ok(HttpResponse::NotFound().body("")),
        },
        Err(_) => {
            synth_id = true;
            match resolve_synth_image_item(state, id_str).await {
                Some(it) => it,
                None => return Ok(HttpResponse::NotFound().body("")),
            }
        }
    };
    let id = item.id;
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
    // Downscale to the client-requested display width (capped per role) before
    // serving. The extracted/stored cache jpeg is full source resolution (a
    // 1080p video frame, a full-size embedded cover); shipping that for a
    // `fillWidth=300` grid tile made the client decode a multi-MB bitmap per
    // item → a RAM-tight TV's LMK SIGKILL'd the app on scroll. `scaled_artwork`
    // caches per-width (keyed on path+mtime+width) and no-ops small inputs, so
    // this is a one-time encode per (image, width). Logo/Banner/Art/Disc return
    // None (alpha PNGs a JPEG rescale would wreck) → served untouched.
    let jpeg_path = match effective_width(role, req_width) {
        Some(width) => cache.scaled_artwork(&jpeg_path, width).await,
        None => jpeg_path,
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
    Ok(deliver_image(&final_path, content_type, head_only, if_none_match).await)
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
    // photographic roles are scaled; Logo/Banner/Art/Disc are usually small
    // PNGs with alpha that a JPEG re-encode would wreck, so they pass through
    // untouched (`effective_width` → None).
    let target_width: Option<u32> = effective_width(role, req_width);
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
    Ok(deliver_image(&final_path, content_type, head_only, if_none_match).await)
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
        let app = test::init_service(App::new().app_data(state).configure(register)).await;
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
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
        let app = test::init_service(App::new().app_data(state).configure(register)).await;
        let first = test::call_service(
            &app,
            test::TestRequest::get()
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
