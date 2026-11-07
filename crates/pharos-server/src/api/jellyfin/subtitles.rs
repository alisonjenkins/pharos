//! Subtitle delivery endpoints.
//!
//! Jellyfin web's video player fetches subtitle tracks via two paths:
//!
//! 1. Embedded extraction —
//!    `GET /Videos/{itemId}/{mediaSourceId}/Subtitles/{streamIndex}/Stream.vtt`
//!    Server pipes `ffmpeg -i source -map 0:s:<idx> -f webvtt pipe:1`
//!    bytes back to the client.
//!
//! 2. External sidecar —
//!    `GET /Videos/{itemId}/{mediaSourceId}/Subtitles/{streamIndex}/Stream.vtt`
//!    Same URL shape; the handler also looks for `<basename>.vtt`,
//!    `<basename>.{lang}.vtt`, `<basename>.srt` next to the media
//!    file. SRT is converted on-the-fly via ffmpeg to WebVTT.
//!
//! The stream index space mixes both source — embedded streams
//! report ffprobe's `index` field unchanged; sidecars are appended
//! starting at `1_000_000` so their numeric IDs never collide with
//! real ffprobe stream indices.

use super::items::query_param;
use crate::{bg_io::BgPermit, state::AppState};
use actix_web::{error, http::header, web, HttpRequest, HttpResponse};
use pharos_cache::subtitle_cache::{
    mtime_nanos, mtime_secs, ImageSubs, SubtitleKind, IMAGE_SUB_GEN_VERSION,
};
use serde::Serialize;
use tokio::process::Command;

/// Sidecar streams get indices starting at this offset so they never
/// collide with ffprobe-reported embedded indices (which top out
/// around 100 even for absurd files).
// B71 — the sidecar base is now PER-ITEM (contiguous, one past the highest real
// ffprobe index) instead of a fixed 1_000_000 sentinel, so sidecar wire indices
// are real positions the strict kotlin players don't crash on.
pub use pharos_jellyfin_api::dto::sidecar_base_index;

pub fn register(cfg: &mut web::ServiceConfig) {
    // T31: lowercase canonical paths.
    cfg.route(
        "/videos/{id}/{media_source_id}/subtitles/{stream_index}/stream.vtt",
        web::get().to(stream_vtt),
    )
    // Some Jellyfin clients drop the mediaSourceId segment.
    .route(
        "/videos/{id}/subtitles/{stream_index}/stream.vtt",
        web::get().to(stream_vtt_short),
    )
    // P40 — legacy `.subtitles.{vtt,srt}` form some older Android
    // and Roku clients still emit. Same body, different URL.
    .route(
        "/videos/{id}/{media_source_id}/subtitles/{stream_index}/subtitles.vtt",
        web::get().to(stream_vtt),
    )
    .route(
        "/videos/{id}/{media_source_id}/subtitles/{stream_index}/subtitles.srt",
        web::get().to(stream_srt),
    )
    .route(
        "/videos/{id}/subtitles/{stream_index}/subtitles.srt",
        web::get().to(stream_srt_short),
    )
    // Raw ASS/SSA delivery for jellyfin-web's SubtitlesOctopus (libass needs
    // the real ASS body, not a VTT conversion).
    .route(
        "/videos/{id}/{media_source_id}/subtitles/{stream_index}/stream.ass",
        web::get().to(stream_ass),
    )
    .route(
        "/videos/{id}/subtitles/{stream_index}/stream.ass",
        web::get().to(stream_ass_short),
    )
    // jellyfin-web's JS subtitle renderer fetches `Stream.js` — a JSON list
    // of cue events it draws itself (rather than a native <track>). Without
    // this route selecting a text subtitle 404s and shows nothing. Some
    // client versions insert a `{startPositionTicks}` path segment.
    .route(
        "/videos/{id}/{media_source_id}/subtitles/{stream_index}/stream.js",
        web::get().to(stream_js),
    )
    .route(
        "/videos/{id}/subtitles/{stream_index}/stream.js",
        web::get().to(stream_js_short),
    )
    .route(
        "/videos/{id}/{media_source_id}/subtitles/{stream_index}/{start_ticks}/stream.js",
        web::get().to(stream_js_ticks),
    )
    // Embedded attachment (font) delivery. SubtitlesOctopus fetches these
    // from the MediaAttachments DeliveryUrls to render ASS/SSA correctly.
    .route(
        "/videos/{id}/{media_source_id}/attachments/{stream_index}",
        web::get().to(stream_attachment),
    )
    .route(
        "/videos/{id}/attachments/{stream_index}",
        web::get().to(stream_attachment_short),
    )
    // Spec 019 — image subtitles the native player draws itself: a timed
    // manifest plus one PNG per event, extracted once per file.
    .route(
        "/videos/{id}/subtitles/{stream_index}/images.json",
        web::get().to(image_subtitle_manifest),
    )
    .route(
        "/videos/{id}/subtitles/{stream_index}/images/{n}.png",
        web::get().to(image_subtitle_png),
    )
    // Spec 026 — an embedded font cut to the glyphs one subtitle track draws,
    // for the native player's ASS renderer.
    .route(
        "/videos/{id}/subtitles/{stream_index}/fonts/{n}",
        web::get().to(subtitle_font),
    );
}

/// The font attachment `n` (its stream index), subset to the glyphs the
/// subtitle at `stream_index` draws (026 FR-010, V488). The whole font when
/// it cannot be subset, counted as `outcome="whole"`.
async fn subtitle_font(
    state: web::Data<AppState>,
    req: HttpRequest,
    path: web::Path<(String, u32, u32)>,
) -> Result<HttpResponse, actix_web::Error> {
    use crate::services::font_subset::{
        ass_codepoints, font_mime, is_font_attachment, subset, FONT_SUBSET_VERSION,
    };
    let (id_str, stream_index, n) = path.into_inner();
    bearer(&state, &req, &id_str, "subtitle_font").await?;
    let id: u64 = pharos_jellyfin_api::dto::parse_item_id(&id_str)
        .ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
    let item =
        super::media_source::load_named(&state, super::media_source::Route::Subtitles, id, None)
            .await?;
    let Some(attachment) = item
        .probe
        .attachments
        .iter()
        .find(|a| a.stream_index == n && is_font_attachment(a))
    else {
        return Err(error::ErrorNotFound("no font attachment at that index"));
    };
    let sidecar_base = sidecar_base_index(&item.probe);
    let script_mtime = if stream_index >= sidecar_base {
        let offset = (stream_index - sidecar_base) as usize;
        let sidecars = discover_sidecars(&item.path).await;
        let Some((sidecar, _)) = sidecars.get(offset) else {
            return Err(error::ErrorNotFound("no sidecar at that index"));
        };
        mtime_secs(sidecar).await
    } else {
        let Some(track) = item
            .probe
            .subtitle_tracks
            .iter()
            .find(|t| t.stream_index == stream_index)
        else {
            return Err(error::ErrorNotFound("no subtitle stream at that index"));
        };
        let codec = track.codec.as_deref().unwrap_or("");
        if is_image_subtitle_codec(codec) {
            return Err(error::ErrorUnsupportedMediaType(format!(
                "{codec} is an image subtitle; it has no fonts"
            )));
        }
        mtime_secs(&item.path).await
    };
    let respond = |body: Vec<u8>, outcome: &'static str| {
        metrics::counter!("pharos_subtitle_font_total", "outcome" => outcome).increment(1);
        tracing::debug!(
            media.id = id,
            subtitle = stream_index,
            font = n,
            outcome,
            bytes = body.len(),
            "subtitle font served"
        );
        HttpResponse::Ok()
            .content_type(font_mime(attachment))
            .insert_header((header::CACHE_CONTROL, "private, max-age=300"))
            .insert_header((header::CONTENT_SECURITY_POLICY, ATTACHMENT_CSP))
            .body(body)
    };

    let media_mtime = mtime_secs(&item.path).await;
    let dir = state.subtitles.as_ref().and_then(|c| {
        c.font_dir(
            &item.path,
            media_mtime,
            stream_index,
            script_mtime,
            FONT_SUBSET_VERSION,
        )
    });
    let cached = dir.as_ref().map(|d| d.join(n.to_string()));
    if let Some(file) = &cached {
        if let Ok(body) = tokio::fs::read(file).await {
            return Ok(respond(body, "cached"));
        }
    }

    let script = ass_script(&state, &item, stream_index).await?;
    let Some(attachments) = state.images.as_ref() else {
        return Err(error::ErrorNotFound("no attachment cache"));
    };
    let indices: Vec<u32> = item
        .probe
        .attachments
        .iter()
        .map(|a| a.stream_index)
        .collect();
    let extracted = attachments
        .ensure_all_attachments(id, &item.path, &indices)
        .await
        .map_err(|e| error::ErrorNotFound(format!("attachment: {e}")))?;
    let font: std::sync::Arc<[u8]> = tokio::fs::read(extracted.join(n.to_string()))
        .await
        .map_err(|e| error::ErrorInternalServerError(format!("read: {e}")))?
        .into();
    let codepoints = ass_codepoints(&String::from_utf8_lossy(&script));
    let cut = {
        let font = std::sync::Arc::clone(&font);
        tokio::task::spawn_blocking(move || subset(&font, &codepoints)).await
    };
    let cut = match cut {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(e)) => {
            tracing::warn!(media.id = id, subtitle = stream_index, font = n, file = ?attachment.filename, error = %e, "subtitle font not subset; serving it whole");
            return Ok(respond(font.to_vec(), "whole"));
        }
        Err(e) => {
            tracing::warn!(media.id = id, subtitle = stream_index, font = n, file = ?attachment.filename, error = %e, "subsetter panicked; serving the font whole");
            return Ok(respond(font.to_vec(), "whole"));
        }
    };
    if let (Some(dir), Some(file)) = (&dir, &cached) {
        // Temp file then rename, so a concurrent reader never sees half a font.
        let tmp = dir.join(format!("{n}.{}.tmp", uuid::Uuid::new_v4().simple()));
        let stored = async {
            tokio::fs::create_dir_all(dir).await?;
            tokio::fs::write(&tmp, &cut).await?;
            tokio::fs::rename(&tmp, file).await
        }
        .await;
        if let Err(e) = stored {
            tracing::warn!(dir = %dir.display(), error = %e, "could not cache a subset font");
            let _ = tokio::fs::remove_file(&tmp).await;
        }
    }
    Ok(respond(cut, "subset"))
}

/// The manifest of an image subtitle track, its `file` names rewritten to
/// fetchable URLs. While the one extraction is still reading the source, the
/// manifest covers it up to `through_ms` and `complete` is false; the player
/// polls until it is true.
async fn image_subtitle_manifest(
    state: web::Data<AppState>,
    req: HttpRequest,
    path: web::Path<(String, u32)>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id, stream_index) = path.into_inner();
    bearer(&state, &req, &id, "subtitle_images").await?;
    let track = image_subtitle_track(&state, &id, stream_index).await?;
    let dir = match track
        .subs
        .image_subtitles(&track.path, track.mtime, track.rel_idx, track.duration_ms)
        .await
    {
        Some(ImageSubs::Dir(dir)) => dir,
        Some(ImageSubs::Failed) => {
            return Err(error::ErrorNotFound("image subtitle extraction failed"))
        }
        None => {
            return Err(error::ErrorNotFound(
                "this server does not extract image subtitles",
            ))
        }
    };
    let raw = match tokio::fs::read(dir.join(pharos_cache::subtitle_cache::IMAGE_SUB_MANIFEST_FILE))
        .await
    {
        Ok(raw) => Some(raw),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(error::ErrorInternalServerError(format!(
                "read {}: {e}",
                dir.display()
            )))
        }
    };
    let base = format!("/videos/{id}/subtitles/{stream_index}/images");
    let version = format!("{}-{IMAGE_SUB_GEN_VERSION}", track.mtime);
    let body = manifest_with_urls(raw.as_deref(), &base, &version).map_err(|e| {
        error::ErrorInternalServerError(format!("manifest in {}: {e}", dir.display()))
    })?;
    Ok(HttpResponse::Ok()
        .content_type("application/json; charset=utf-8")
        .insert_header((header::CACHE_CONTROL, "no-store"))
        .json(body))
}

/// One event's PNG. 404 until the extraction has written it. Cached a day:
/// its URL carries the source mtime and extractor version, so a replaced
/// file or a new extractor is a different URL.
async fn image_subtitle_png(
    state: web::Data<AppState>,
    req: HttpRequest,
    path: web::Path<(String, u32, u32)>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id, stream_index, n) = path.into_inner();
    bearer(&state, &req, &id, "subtitle_images").await?;
    let track = image_subtitle_track(&state, &id, stream_index).await?;
    // Only the manifest route starts or checks an extraction; a PNG it named
    // is already on disk.
    let Some(dir) = track
        .subs
        .image_dir(&track.path, track.mtime, track.rel_idx)
    else {
        return Err(error::ErrorNotFound("no subtitle cache directory"));
    };
    match tokio::fs::read(dir.join(format!("{n}.png"))).await {
        Ok(png) => Ok(HttpResponse::Ok()
            .content_type("image/png")
            .insert_header((header::CACHE_CONTROL, "private, max-age=86400"))
            .body(png)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(error::ErrorNotFound("subtitle image not extracted"))
        }
        Err(e) => Err(error::ErrorInternalServerError(format!(
            "read {n}.png in {}: {e}",
            dir.display()
        ))),
    }
}

/// An image subtitle track, located for the subtitle cache.
struct ImageTrack<'a> {
    subs: &'a pharos_cache::subtitle_cache::SubtitleCache,
    path: std::path::PathBuf,
    /// The source's mtime in nanoseconds ([`mtime_nanos`]): it names the
    /// cache directory and versions the PNG URLs.
    mtime: i64,
    rel_idx: u32,
    duration_ms: u64,
}

/// The image subtitle track at absolute `stream_index`. 415 for a text
/// track, 404 for no such stream or no subtitle cache.
async fn image_subtitle_track<'a>(
    state: &'a AppState,
    id_str: &str,
    stream_index: u32,
) -> Result<ImageTrack<'a>, actix_web::Error> {
    let id: u64 = pharos_jellyfin_api::dto::parse_item_id(id_str)
        .ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
    let item =
        super::media_source::load_named(state, super::media_source::Route::Subtitles, id, None)
            .await?;
    let tracks = &item.probe.subtitle_tracks;
    let Some(rel_idx) = tracks.iter().position(|t| t.stream_index == stream_index) else {
        return Err(error::ErrorNotFound("no subtitle stream at that index"));
    };
    let codec = tracks[rel_idx].codec.as_deref().unwrap_or("");
    if !is_image_subtitle_codec(codec) {
        return Err(error::ErrorUnsupportedMediaType(format!(
            "{codec} is a text subtitle; fetch stream.vtt"
        )));
    }
    let Some(subs) = state.subtitles.as_ref() else {
        return Err(error::ErrorNotFound("no subtitle cache"));
    };
    Ok(ImageTrack {
        subs,
        mtime: mtime_nanos(&item.path).await,
        path: item.path,
        rel_idx: rel_idx as u32,
        duration_ms: item.probe.duration_ms.unwrap_or(0),
    })
}

/// `raw` (the extractor's manifest, `None` before its first write) with each
/// event's `file` replaced by `url` = `{base}/{file}?v={version}`.
fn manifest_with_urls(
    raw: Option<&[u8]>,
    base: &str,
    version: &str,
) -> Result<serde_json::Value, serde_json::Error> {
    let Some(raw) = raw else {
        return Ok(serde_json::json!({
            "width": 0, "height": 0, "complete": false, "through_ms": 0, "events": [],
        }));
    };
    let mut manifest: serde_json::Value = serde_json::from_slice(raw)?;
    if let Some(events) = manifest.get_mut("events").and_then(|e| e.as_array_mut()) {
        for event in events.iter_mut().filter_map(|e| e.as_object_mut()) {
            if let Some(serde_json::Value::String(file)) = event.remove("file") {
                event.insert("url".into(), format!("{base}/{file}?v={version}").into());
            }
        }
    }
    Ok(manifest)
}

/// The bearer of a subtitle or attachment fetch, refused (401) without a
/// credential and 404 for an item outside its libraries (V187, V189). The
/// file routes below take the token from `api_key` or the `JellyfinAuth`
/// cookie, since a `<track>` or font fetch cannot set a header.
async fn bearer(
    state: &AppState,
    req: &HttpRequest,
    id_str: &str,
    route: &'static str,
) -> Result<pharos_core::User, actix_web::Error> {
    let user = crate::api::jellyfin::require_bearer(state, req, id_str, route).await?;
    let id = pharos_jellyfin_api::dto::parse_item_id(id_str)
        .ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
    crate::api::jellyfin::require_item_access(state, &user, id, route).await?;
    Ok(user)
}

async fn stream_attachment(
    state: web::Data<AppState>,
    req: HttpRequest,
    path: web::Path<(String, String, u32)>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id, msid, stream_index) = path.into_inner();
    bearer(&state, &req, &id, "attachment").await?;
    deliver_attachment(&state, &id, Some(&msid), stream_index).await
}

async fn stream_attachment_short(
    state: web::Data<AppState>,
    req: HttpRequest,
    path: web::Path<(String, u32)>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id, stream_index) = path.into_inner();
    bearer(&state, &req, &id, "attachment").await?;
    deliver_attachment(&state, &id, None, stream_index).await
}

/// Extract + serve an embedded attachment (font) by stream index. Cached on
/// disk after the first fetch. Callers authenticate first (V189).
#[tracing::instrument(skip(state), fields(media.id = %id_str, idx = stream_index, kind = "attachment"))]
async fn deliver_attachment(
    state: &AppState,
    id_str: &str,
    media_source_id: Option<&str>,
    stream_index: u32,
) -> Result<HttpResponse, actix_web::Error> {
    let id: u64 = pharos_jellyfin_api::dto::parse_item_id(id_str)
        .ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
    let item = super::media_source::load_named(
        state,
        super::media_source::Route::Subtitles,
        id,
        media_source_id,
    )
    .await?;
    let Some(cache) = state.images.as_ref() else {
        return Err(error::ErrorNotFound("no attachment cache"));
    };
    // Content-Type from the probed mimetype; fall back to a generic type.
    let content_type = item
        .probe
        .attachments
        .iter()
        .find(|a| a.stream_index == stream_index)
        .and_then(|a| a.mime_type.clone())
        .unwrap_or_else(|| "application/octet-stream".to_string());
    // Warm EVERY font in one source open, not just the requested one:
    // SubtitlesOctopus fetches all of them before rendering, so a per-font
    // extraction re-opens the (NFS, multi-GB) source N times and stalls the
    // "Fetching assets" phase. The remaining N-1 requests then hit warm cache.
    let indices: Vec<u32> = item
        .probe
        .attachments
        .iter()
        .map(|a| a.stream_index)
        .collect();
    let dir = cache
        .ensure_all_attachments(id, &item.path, &indices)
        .await
        .map_err(|e| error::ErrorNotFound(format!("attachment: {e}")))?;
    let bytes = tokio::fs::read(dir.join(stream_index.to_string()))
        .await
        .map_err(|e| error::ErrorInternalServerError(format!("read: {e}")))?;
    let inline = is_font(&content_type);
    let mut resp = HttpResponse::Ok();
    resp.content_type(content_type.as_str())
        .insert_header((header::CACHE_CONTROL, "private, max-age=86400"))
        .insert_header((header::CONTENT_SECURITY_POLICY, ATTACHMENT_CSP));
    if !inline {
        resp.insert_header((header::CONTENT_DISPOSITION, "attachment"));
    }
    tracing::debug!(
        mime = %content_type,
        disposition = if inline { "inline" } else { "attachment" },
        "attachment served"
    );
    Ok(resp.body(bytes))
}

/// An attachment's type comes from the media file, so whoever made the file
/// chooses it. A sandboxed, script-less CSP keeps an HTML or SVG attachment
/// from running on pharos's origin even if a browser renders it (B255).
const ATTACHMENT_CSP: &str = "default-src 'none'; sandbox";

/// Font mimetypes Matroska muxers write. Only these are served inline,
/// for SubtitlesOctopus; anything else is a download (V195).
pub(crate) fn is_font(mime: &str) -> bool {
    let mime = mime
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    mime.starts_with("font/")
        || matches!(
            mime.as_str(),
            "application/x-truetype-font"
                | "application/x-font-ttf"
                | "application/x-font-otf"
                | "application/x-font-opentype"
                | "application/vnd.ms-opentype"
                | "application/font-sfnt"
                | "application/font-woff"
                | "application/x-font-woff"
        )
}

async fn stream_ass(
    state: web::Data<AppState>,
    req: HttpRequest,
    path: web::Path<(String, String, u32)>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id, msid, stream_index) = path.into_inner();
    bearer(&state, &req, &id, "subtitle").await?;
    deliver_ass(&state, &id, Some(&msid), stream_index, req.query_string()).await
}

async fn stream_ass_short(
    state: web::Data<AppState>,
    req: HttpRequest,
    path: web::Path<(String, u32)>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id, stream_index) = path.into_inner();
    bearer(&state, &req, &id, "subtitle").await?;
    deliver_ass(&state, &id, None, stream_index, req.query_string()).await
}

/// Serve a subtitle track as RAW ASS/SSA (for SubtitlesOctopus). Sidecars pass
/// through verbatim; embedded streams are extracted with `-c:s ass -f ass` and
/// cached (distinct `EmbeddedAss` key from the VTT form).
#[tracing::instrument(skip(state), fields(media.id = %id_str, idx = stream_index, kind = "ass"))]
async fn deliver_ass(
    state: &AppState,
    id_str: &str,
    media_source_id: Option<&str>,
    stream_index: u32,
    query: &str,
) -> Result<HttpResponse, actix_web::Error> {
    let mut track = DeliveredTrack::default();
    let result = deliver_ass_inner(
        state,
        id_str,
        media_source_id,
        stream_index,
        query,
        &mut track,
    )
    .await;
    record_delivery(DeliveryFormat::Ass, track, stream_index, &result);
    result
}

async fn deliver_ass_inner(
    state: &AppState,
    id_str: &str,
    media_source_id: Option<&str>,
    stream_index: u32,
    query: &str,
    track: &mut DeliveredTrack,
) -> Result<HttpResponse, actix_web::Error> {
    let id: u64 = pharos_jellyfin_api::dto::parse_item_id(id_str)
        .ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
    let item = super::media_source::load_named(
        state,
        super::media_source::Route::Subtitles,
        id,
        media_source_id,
    )
    .await?;

    if crate::styled_track::is_styled_index(&item, stream_index).await {
        *track = DeliveredTrack::generated();
        return deliver_styled_ass(state, &item, stream_index, query).await;
    }

    // A sidecar is found once, so the track counted is the file served.
    let sidecar_base = sidecar_base_index(&item.probe);
    let bytes = if stream_index >= sidecar_base {
        let offset = (stream_index - sidecar_base) as usize;
        let sidecar = discover_sidecars(&item.path).await.into_iter().nth(offset);
        *track = DeliveredTrack::sidecar(sidecar.as_ref().map(|(p, _)| p.as_path()));
        let Some((sidecar_path, _)) = sidecar else {
            return Err(error::ErrorNotFound("no sidecar at that index"));
        };
        tokio::fs::read(&sidecar_path)
            .await
            .map_err(|e| error::ErrorInternalServerError(format!("read: {e}")))?
    } else {
        *track = DeliveredTrack::embedded(&item, stream_index);
        ass_script(state, &item, stream_index).await?
    };
    Ok(HttpResponse::Ok()
        .content_type("text/x-ssa; charset=utf-8")
        .insert_header((header::CACHE_CONTROL, "private, max-age=3600"))
        .body(bytes))
}

/// Serve the generated Pharos styled track (spec 028 FR-017): the item's
/// stored effect plan compiled over its source subtitle for `?tier=` (default
/// `tv`) and `?effects=` (default `subtle`). Cached per tier, level, compiler
/// and director version and the plan's `styled_at` stamp, so a restyle or a
/// version bump never serves an old script.
async fn deliver_styled_ass(
    state: &AppState,
    item: &pharos_core::MediaItem,
    styled_idx: u32,
    query: &str,
) -> Result<HttpResponse, actix_web::Error> {
    use pharos_styling::{EffectsLevel, Tier};

    let tier = match query_param(query, "tier") {
        None => Tier::Tv,
        Some(v) => Tier::parse(v).ok_or_else(|| error::ErrorBadRequest("unknown tier"))?,
    };
    let level = match query_param(query, "effects") {
        None => EffectsLevel::Subtle,
        Some(v) => {
            EffectsLevel::parse(v).ok_or_else(|| error::ErrorBadRequest("unknown effects level"))?
        }
    };
    let Some(row) = crate::styled_track::current(&state.stores, item).await else {
        return Err(error::ErrorNotFound("no styled track for this item"));
    };
    let kind = SubtitleKind::Styled {
        tier: u8::try_from(Tier::ALL.iter().position(|t| *t == tier).unwrap_or(0))
            .unwrap_or(u8::MAX),
        level: u8::try_from(
            EffectsLevel::ALL
                .iter()
                .position(|l| *l == level)
                .unwrap_or(0),
        )
        .unwrap_or(u8::MAX),
        compiler_version: pharos_styling::COMPILER_VERSION,
        director_version: pharos_styling::DIRECTOR_VERSION,
        plan_stamp: row.styled_at,
    };
    let mtime = mtime_secs(&item.path).await;
    let cache = state.subtitles.as_ref();
    let _guard;
    if let Some(cache) = cache {
        if let Some(bytes) = cache.get(&item.path, mtime, styled_idx, kind).await {
            return Ok(styled_response(tier, (*bytes).clone()));
        }
        let lock = cache.lock(&item.path, mtime, styled_idx, kind).await;
        _guard = lock.lock_owned().await;
        if let Some(bytes) = cache.get(&item.path, mtime, styled_idx, kind).await {
            return Ok(styled_response(tier, (*bytes).clone()));
        }
    }
    let script = match styled_script(state, item, &row, tier, level).await {
        Ok(script) => script,
        Err(e) => {
            tracing::error!(
                media.id = item.id,
                source_index = row.source_index,
                tier = tier.label(),
                effects = level.label(),
                error = %e,
                "styled subtitle could not be compiled"
            );
            return Err(error::ErrorInternalServerError("styled subtitle failed"));
        }
    };
    let bytes = match cache {
        Some(cache) => (*cache
            .store(&item.path, mtime, styled_idx, kind, script)
            .await)
            .clone(),
        None => script,
    };
    Ok(styled_response(tier, bytes))
}

fn styled_response(tier: pharos_styling::Tier, body: Vec<u8>) -> HttpResponse {
    HttpResponse::Ok()
        .content_type("text/x-ssa; charset=utf-8")
        // The URL stays the same across a restyle, so the client revalidates.
        .insert_header((header::CACHE_CONTROL, "private, no-cache"))
        .insert_header(("X-Pharos-Subtitle-Tier", tier.label()))
        .body(body)
}

/// Compiles the item's stored plan over the source subtitle. An ASS source
/// (embedded or sidecar) and any sidecar are read as they are, `source::read`
/// sniffing ASS, WebVTT or SRT; an embedded SRT or other text stream is read
/// from its WebVTT extraction.
async fn styled_script(
    state: &AppState,
    item: &pharos_core::MediaItem,
    row: &pharos_core::SubtitleStyling,
    tier: pharos_styling::Tier,
    level: pharos_styling::EffectsLevel,
) -> Result<Vec<u8>, String> {
    let sidecar_base = sidecar_base_index(&item.probe);
    let embedded_codec = (row.source_index < sidecar_base)
        .then(|| {
            item.probe
                .subtitle_tracks
                .iter()
                .find(|t| t.stream_index == row.source_index)
                .and_then(|t| t.codec.as_deref())
        })
        .flatten();
    let raw = match embedded_codec {
        Some(codec) if !is_styled_text_subtitle_codec(codec) => {
            resolve_vtt_bytes(state, item, row.source_index).await
        }
        _ => ass_script(state, item, row.source_index).await,
    }
    .map_err(|e| format!("read source: {e}"))?;
    let plan_json = row.plan_json.clone().ok_or("row has no plan")?;
    tokio::task::spawn_blocking(move || {
        // Lossy, as the styling job reads it: the plan's line indices only hold
        // over the same text.
        let text = String::from_utf8_lossy(&raw);
        let source = pharos_styling::source::read(&text).map_err(|e| format!("source: {e}"))?;
        let plan = serde_json::from_str::<pharos_styling::EffectPlan>(&plan_json)
            .map_err(|e| format!("plan: {e}"))?;
        let theme = pharos_styling::theme::Theme::pharos();
        Ok(pharos_styling::compile(&plan, &source.lines, &theme, tier, level).into_bytes())
    })
    .await
    .map_err(|e| format!("compile task: {e}"))?
}

/// The raw ASS/SSA script of the subtitle at `stream_index`: a sidecar's file
/// as it is, an embedded stream extracted once and cached.
pub(crate) async fn ass_script(
    state: &AppState,
    item: &pharos_core::MediaItem,
    stream_index: u32,
) -> Result<Vec<u8>, actix_web::Error> {
    // Sidecar → raw passthrough (it's already an .ass/.ssa file on disk).
    let sidecar_base = sidecar_base_index(&item.probe);
    if stream_index >= sidecar_base {
        let offset = (stream_index - sidecar_base) as usize;
        let sidecars = discover_sidecars(&item.path).await;
        let Some((sidecar_path, _)) = sidecars.into_iter().nth(offset) else {
            return Err(error::ErrorNotFound("no sidecar at that index"));
        };
        return tokio::fs::read(&sidecar_path)
            .await
            .map_err(|e| error::ErrorInternalServerError(format!("read: {e}")));
    }

    embedded_ass_script(
        state.subtitles.as_ref(),
        item,
        stream_index,
        &BgPermit::playback_priority(),
    )
    .await
}

/// The ASS/SSA script of the embedded stream `stream_index`, extracted once and
/// cached when a cache is given. The caller's `permit` is the I/O budget the
/// extraction runs under.
pub(crate) async fn embedded_ass_script(
    cache: Option<&pharos_cache::SubtitleCache>,
    item: &pharos_core::MediaItem,
    stream_index: u32,
    permit: &BgPermit,
) -> Result<Vec<u8>, actix_web::Error> {
    let input = item
        .path
        .to_str()
        .ok_or_else(|| error::ErrorInternalServerError("non-utf8 path"))?;
    let mtime = mtime_secs(&item.path).await;
    if let Some(cache) = cache {
        if let Some(bytes) = cache
            .get(&item.path, mtime, stream_index, SubtitleKind::EmbeddedAss)
            .await
        {
            return Ok((*bytes).clone());
        }
        let lock = cache
            .lock(&item.path, mtime, stream_index, SubtitleKind::EmbeddedAss)
            .await;
        let _guard = lock.lock().await;
        if let Some(bytes) = cache
            .get(&item.path, mtime, stream_index, SubtitleKind::EmbeddedAss)
            .await
        {
            return Ok((*bytes).clone());
        }
        let out = run_ffmpeg_embedded_ass(input, stream_index, permit).await?;
        let stored = cache
            .store(
                &item.path,
                mtime,
                stream_index,
                SubtitleKind::EmbeddedAss,
                out,
            )
            .await;
        return Ok((*stored).clone());
    }
    run_ffmpeg_embedded_ass(input, stream_index, permit).await
}

/// Pre-extract every text subtitle track of `item` into the subtitle cache so
/// playback serves them instantly instead of stalling on a whole-file demux.
/// Image subs (PGS/VOBSUB) are skipped — they burn into the transcode, not a
/// separate file. Best-effort: a failed track logs + is skipped. Runs off the
/// request path (background pre-generator), so a multi-GB source's slow extract
/// never blocks a viewer.
pub(crate) async fn pre_extract_subtitles(
    cache: &pharos_cache::SubtitleCache,
    item: &pharos_core::MediaItem,
    _permit: &BgPermit,
) {
    let Some(input) = item.path.to_str() else {
        return;
    };
    let mtime = mtime_secs(&item.path).await;

    // Collect the text tracks still needing extraction — skip image subs and
    // anything already cached — so the whole item demuxes in ONE source open
    // (V36) rather than a whole-file NFS read PER track. B72 / Supergirl-S01E03:
    // ~10 text tracks × ~500s cold demux each, serialized, saturating NFS and
    // starving live playback. Batching collapses that to a single open pass.
    let mut pending: Vec<PendingTrack> = Vec::new();
    for t in item.probe.subtitle_tracks.iter() {
        let codec_lc = t.codec.as_deref().unwrap_or("").to_ascii_lowercase();
        if is_image_subtitle_codec(&codec_lc) {
            // B52 — image subs never extract to a sidecar. Their EVENT WINDOWS
            // (burn gating) are scanned ON DEMAND from the segment handler
            // (`gate_image_sub_burn`) for the item actually being played —
            // bg-IO gated + deduped, ONE item. B49 kicked that whole-file scan
            // here in EVERY warm pass, so the library-wide warm-all scanned
            // every image-sub title on each boot (thousands of whole-file NFS
            // demuxes), saturating the libav pool and freezing live playback
            // (Supergirl, 2026-07-14). Never scan from the warm path.
            continue;
        }
        let is_ass = matches!(
            codec_lc.as_str(),
            "ass" | "ssa" | "advanced substation alpha"
        );
        let kind = if is_ass {
            SubtitleKind::EmbeddedAss
        } else {
            SubtitleKind::Embedded
        };
        if cache
            .get(&item.path, mtime, t.stream_index, kind)
            .await
            .is_some()
        {
            continue;
        }
        pending.push(PendingTrack {
            stream_index: t.stream_index,
            kind,
            // ass carries styling verbatim; everything else → webvtt.
            muxer: if is_ass { "ass" } else { "webvtt" },
        });
    }
    if pending.is_empty() {
        return;
    }

    // One temp dir for the item; per-track files inside. `tempfile` names the
    // dir randomly, so two concurrent warm passes of the same item never share
    // scratch paths (V36 "fixed scratch paths are per-caller-unique").
    let tmp = match tempfile::Builder::new().prefix("pharos-subx-").tempdir() {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(error = %e, media.id = item.id, "subtitle batch tempdir failed");
            return;
        }
    };
    let mut batch: Vec<BatchTrack> = Vec::with_capacity(pending.len());
    for p in &pending {
        batch.push(BatchTrack {
            stream_index: p.stream_index,
            muxer: p.muxer,
            out: tmp
                .path()
                .join(format!("track-{}.{}", p.stream_index, p.muxer)),
        });
    }

    let args = build_batch_extract_args(input, &batch);
    let started = std::time::Instant::now();
    let output = match Command::new("ffmpeg").args(&args).output().await {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(error = %e, media.id = item.id, "subtitle batch ffmpeg spawn failed");
            return;
        }
    };
    // ffmpeg's own verdict on the SOURCE, said once. Thirty per-track "no
    // output" lines carried no cause at all: on 2026-09-04 the cause was a
    // stale NFS handle on the open, and the exit status and stderr that named
    // it were discarded (B212).
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail = stderr_tail(&stderr, 3);
        tracing::warn!(
            media.id = item.id,
            path = %item.path.display(),
            exit = %output.status,
            stderr_tail = %tail,
            "subtitle batch ffmpeg exited non-zero"
        );
    }

    // Harvest whatever got written, REGARDLESS of overall exit status: ffmpeg
    // writes each output progressively, so one poison stream must not lose the
    // tracks that did extract — and we never re-open per track (no fallback
    // loop, so a permanently-bad stream can't turn every warm pass back into N
    // whole-file demuxes).
    let mut ok = 0u32;
    let mut empty = 0u32;
    for (p, b) in pending.iter().zip(batch.iter()) {
        match tokio::fs::read(&b.out).await {
            Ok(bytes) if !bytes.is_empty() => {
                cache
                    .store(&item.path, mtime, p.stream_index, p.kind, bytes)
                    .await;
                ok += 1;
            }
            _ => {
                empty += 1;
                if memo_empty_track(output.status.success()) {
                    // ffmpeg read the whole source and wrote nothing for this
                    // track: it has no cues. Remember that, or the track stays
                    // "pending" and every warm pass re-demuxes the whole file
                    // to learn it again — 60 such lines for one item in two
                    // days (B212). A failed run is NOT memoised: it says
                    // nothing about the track.
                    cache
                        .store(&item.path, mtime, p.stream_index, p.kind, Vec::new())
                        .await;
                }
                tracing::debug!(
                    media.id = item.id,
                    idx = p.stream_index,
                    "subtitle track produced no output in batch demux"
                );
            }
        }
    }
    tracing::info!(
        media.id = item.id,
        tracks = pending.len(),
        extracted = ok,
        empty,
        ffmpeg_ok = output.status.success(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "subtitle batch extracted (one source open)"
    );
}

/// Whether a track that produced no output should be remembered as EMPTY.
/// Only when ffmpeg finished cleanly: then the source was read end to end and
/// the track genuinely has nothing. A non-zero exit (a source that would not
/// open, a demux that died halfway) says nothing about the track, and memoising
/// it would hide a subtitle until the file's mtime changed.
fn memo_empty_track(ffmpeg_succeeded: bool) -> bool {
    ffmpeg_succeeded
}

/// The last `n` non-empty lines of an ffmpeg stderr, joined — the part that
/// names the fault.
fn stderr_tail(stderr: &str, n: usize) -> String {
    let lines: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join(" | ")
}

/// A text subtitle track selected for batch extraction.
struct PendingTrack {
    stream_index: u32,
    kind: SubtitleKind,
    muxer: &'static str,
}

/// One track's slot in a batch ffmpeg invocation: its stream index, target
/// muxer, and the temp file its demuxed bytes are written to.
struct BatchTrack {
    stream_index: u32,
    muxer: &'static str,
    out: std::path::PathBuf,
}

/// Build the args for ONE ffmpeg invocation that opens `input` a SINGLE time
/// and demuxes every track in `tracks` to its own output file (V36). The source
/// is opened once no matter how many tracks — the whole point of batching. Pure
/// so the "one `-i`" invariant is unit-testable without running ffmpeg.
fn build_batch_extract_args(input: &str, tracks: &[BatchTrack]) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
        "-i".into(),
        input.into(),
    ];
    for t in tracks {
        args.push("-map".into());
        args.push(format!("0:{}", t.stream_index));
        args.push("-c:s".into());
        args.push(t.muxer.into());
        args.push("-f".into());
        args.push(t.muxer.into());
        args.push(t.out.to_string_lossy().into_owned());
    }
    args
}

/// Resolve a local `.ass` file to burn a TEXT/ASS track from, extracting +
/// disk-caching the embedded stream if cold. `stream_index` is the ABSOLUTE
/// ffprobe index (same convention as `deliver_ass` / `pre_extract_subtitles`).
///
/// The burn path uses this to point ffmpeg's `subtitles=` filter at a small
/// local file instead of the whole source container: the `subtitles` filter
/// opens a SECOND demuxer on its `filename=` at init — once PER HLS segment —
/// and reads the WHOLE container to collect subtitle packets + fonts, so a
/// multi-GB NFS source is re-demuxed end-to-end every 6 s segment. Returns
/// `None` (caller then falls back to the source-file form — correct, slower)
/// when no local file can be produced: an external sidecar that's missing, or a
/// memory-only subtitle cache (no disk root → no stable path to hand ffmpeg).
///
/// - External `.ass`/`.srt` sidecars (index ≥ `sidecar_base`) are already small
///   local files → returned directly (also fixes the source `si=N` form, which
///   is wrong for a sidecar not muxed into the source).
/// - Embedded tracks are extracted verbatim as `.ass` into the disk-backed
///   subtitle cache (shared with `deliver_ass`, so a warmed sidecar is reused),
///   and the on-disk path is returned.
#[tracing::instrument(
    name = "ensure_ass_sidecar_path",
    skip_all,
    fields(media.id = %item.id, stream_index)
)]
pub(crate) async fn ensure_ass_sidecar_path(
    state: &AppState,
    item: &pharos_core::MediaItem,
    stream_index: u32,
) -> Option<std::path::PathBuf> {
    // External sidecar → the file on disk is already the burn source.
    let sidecar_base = sidecar_base_index(&item.probe);
    if stream_index >= sidecar_base {
        let offset = (stream_index - sidecar_base) as usize;
        let sidecars = discover_sidecars(&item.path).await;
        return sidecars.into_iter().nth(offset).map(|(p, _)| p);
    }

    // Embedded → extract-if-cold into the disk-backed cache, hand back its path.
    let cache = state.subtitles.as_ref()?;
    let mtime = mtime_secs(&item.path).await;
    // Memory-only cache → no stable file path to give ffmpeg; caller falls back.
    let disk = cache.disk_path_of(&item.path, mtime, stream_index, SubtitleKind::EmbeddedAss)?;
    if tokio::fs::try_exists(&disk).await.unwrap_or(false) {
        return Some(disk);
    }
    // Cold — dedupe concurrent segment requests on the per-key fetch lock, then
    // extract (bg-gated, like every other ASS extraction) + store (writes disk).
    let lock = cache
        .lock(&item.path, mtime, stream_index, SubtitleKind::EmbeddedAss)
        .await;
    let _guard = lock.lock().await;
    if tokio::fs::try_exists(&disk).await.unwrap_or(false) {
        return Some(disk);
    }
    let input = item.path.to_str()?;
    let out = run_ffmpeg_embedded_ass(input, stream_index, &BgPermit::playback_priority())
        .await
        .ok()?;
    cache
        .store(
            &item.path,
            mtime,
            stream_index,
            SubtitleKind::EmbeddedAss,
            out,
        )
        .await;
    // `store` writes the disk sidecar only when a disk root is configured;
    // confirm the file is really there before handing ffmpeg the path.
    tokio::fs::try_exists(&disk)
        .await
        .unwrap_or(false)
        .then_some(disk)
}

/// Extract one embedded subtitle stream verbatim as ASS.
async fn run_ffmpeg_embedded_ass(
    input: &str,
    stream_index: u32,
    _permit: &BgPermit,
) -> Result<Vec<u8>, actix_web::Error> {
    let started = std::time::Instant::now();
    let out = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-i",
            input,
            "-map",
            &format!("0:{stream_index}"),
            "-c:s",
            "ass",
            "-f",
            "ass",
            "pipe:1",
        ])
        .output()
        .await
        .map_err(|e| error::ErrorInternalServerError(format!("ffmpeg spawn: {e}")))?;
    if !out.status.success() {
        return Err(error::ErrorNotFound(format!(
            "ffmpeg ass extract: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    tracing::info!(
        idx = stream_index,
        bytes = out.stdout.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "subtitle ass extracted (ffmpeg, cold demux)"
    );
    Ok(out.stdout)
}

async fn stream_srt(
    state: web::Data<AppState>,
    req: HttpRequest,
    path: web::Path<(String, String, u32)>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id, msid, stream_index) = path.into_inner();
    bearer(&state, &req, &id, "subtitle").await?;
    let forced_only = parse_forced_only(req.query_string());
    deliver_srt(&state, &id, Some(&msid), stream_index, forced_only).await
}

async fn stream_srt_short(
    state: web::Data<AppState>,
    req: HttpRequest,
    path: web::Path<(String, u32)>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id, stream_index) = path.into_inner();
    bearer(&state, &req, &id, "subtitle").await?;
    let forced_only = parse_forced_only(req.query_string());
    deliver_srt(&state, &id, None, stream_index, forced_only).await
}

/// P40 — SRT-form delivery. ffmpeg converts the embedded stream to
/// `-c:s subrip -f srt`. Hits the same image-codec refusal as the
/// VTT path so PGS/DVB return 415 instead of empty bodies.
#[tracing::instrument(skip(state), fields(media.id = %id_str, idx = stream_index, kind = "srt"))]
async fn deliver_srt(
    state: &AppState,
    id_str: &str,
    media_source_id: Option<&str>,
    stream_index: u32,
    forced_only: bool,
) -> Result<HttpResponse, actix_web::Error> {
    let id: u64 = pharos_jellyfin_api::dto::parse_item_id(id_str)
        .ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
    let item = super::media_source::load_named(
        state,
        super::media_source::Route::Subtitles,
        id,
        media_source_id,
    )
    .await?;
    if let Some(track) = item
        .probe
        .subtitle_tracks
        .iter()
        .find(|t| t.stream_index == stream_index)
    {
        if let Some(codec) = track.codec.as_deref() {
            if is_image_subtitle_codec(codec) {
                return Ok(HttpResponse::UnsupportedMediaType()
                    .content_type("application/json")
                    .body(format!(
                        r#"{{"error":"image-based subtitles cannot convert to SRT","codec":"{codec}"}}"#
                    )));
            }
        }
        if forced_only && !track.is_forced {
            return Ok(HttpResponse::NotFound()
                .content_type("application/json")
                .body(
                    r#"{"error":"track is not a forced-only track; pick the forced disposition track"}"#,
                ));
        }
    }
    let input = item
        .path
        .to_str()
        .ok_or_else(|| error::ErrorInternalServerError("non-utf8 path"))?;
    let mtime = mtime_secs(&item.path).await;
    let srt_response = |bytes: Vec<u8>| {
        HttpResponse::Ok()
            .content_type("application/x-subrip; charset=utf-8")
            .insert_header((header::CACHE_CONTROL, "private, max-age=3600"))
            .body(bytes)
    };

    // T97/V36 — a cold SRT fetch is a whole-file NFS demux, and clients may
    // re-request the same track. Persist it (disk-backed, like the vtt/ass
    // paths) and single-flight the miss via the per-key lock, so it demuxes
    // ONCE instead of once per fetch. On-demand for the watched item → the
    // extract runs at playback priority (V34).
    if let Some(cache) = state.subtitles.as_ref() {
        if let Some(bytes) = cache
            .get(&item.path, mtime, stream_index, SubtitleKind::EmbeddedSrt)
            .await
        {
            return Ok(srt_response((*bytes).clone()));
        }
        let lock = cache
            .lock(&item.path, mtime, stream_index, SubtitleKind::EmbeddedSrt)
            .await;
        let _guard = lock.lock().await;
        if let Some(bytes) = cache
            .get(&item.path, mtime, stream_index, SubtitleKind::EmbeddedSrt)
            .await
        {
            return Ok(srt_response((*bytes).clone()));
        }
        let out =
            run_ffmpeg_embedded_srt(input, stream_index, &BgPermit::playback_priority()).await?;
        let stored = cache
            .store(
                &item.path,
                mtime,
                stream_index,
                SubtitleKind::EmbeddedSrt,
                out,
            )
            .await;
        return Ok(srt_response((*stored).clone()));
    }

    // No cache configured (tests / minimal deploys): spawn per-fetch.
    let out = run_ffmpeg_embedded_srt(input, stream_index, &BgPermit::playback_priority()).await?;
    Ok(srt_response(out))
}

/// Extract one embedded subtitle stream as SubRip (`-c:s subrip -f srt`).
async fn run_ffmpeg_embedded_srt(
    input: &str,
    stream_index: u32,
    _permit: &BgPermit,
) -> Result<Vec<u8>, actix_web::Error> {
    let started = std::time::Instant::now();
    let out = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-i",
            input,
            "-map",
            &format!("0:{stream_index}"),
            "-c:s",
            "subrip",
            "-f",
            "srt",
            "pipe:1",
        ])
        .output()
        .await
        .map_err(|e| error::ErrorInternalServerError(format!("ffmpeg spawn: {e}")))?;
    if !out.status.success() {
        return Err(error::ErrorNotFound(format!(
            "ffmpeg extract: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    tracing::info!(
        idx = stream_index,
        bytes = out.stdout.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "subtitle srt extracted (ffmpeg, cold demux)"
    );
    Ok(out.stdout)
}

async fn stream_vtt(
    state: web::Data<AppState>,
    req: HttpRequest,
    path: web::Path<(String, String, u32)>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id, msid, stream_index) = path.into_inner();
    let user = bearer(&state, &req, &id, "subtitle").await?;
    let forced_only = parse_forced_only(req.query_string());
    let style = subtitle_style_for(&state, user.id).await;
    deliver_vtt(&state, &id, Some(&msid), stream_index, forced_only, style).await
}

async fn stream_vtt_short(
    state: web::Data<AppState>,
    req: HttpRequest,
    path: web::Path<(String, u32)>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id, stream_index) = path.into_inner();
    let user = bearer(&state, &req, &id, "subtitle").await?;
    let forced_only = parse_forced_only(req.query_string());
    let style = subtitle_style_for(&state, user.id).await;
    deliver_vtt(&state, &id, None, stream_index, forced_only, style).await
}

fn parse_forced_only(qs: &str) -> bool {
    for kv in qs.split('&') {
        if let Some((k, v)) = kv.split_once('=') {
            if k.eq_ignore_ascii_case("ForcedOnly")
                && matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes")
            {
                return true;
            }
        }
    }
    false
}

#[tracing::instrument(skip(state, style), fields(media.id = %id_str, idx = stream_index, kind = "vtt"))]
async fn deliver_vtt(
    state: &AppState,
    id_str: &str,
    media_source_id: Option<&str>,
    stream_index: u32,
    forced_only: bool,
    style: SubtitleStyle,
) -> Result<HttpResponse, actix_web::Error> {
    let mut track = DeliveredTrack::default();
    let result = deliver_vtt_inner(
        state,
        id_str,
        media_source_id,
        stream_index,
        forced_only,
        style,
        &mut track,
    )
    .await;
    record_delivery(DeliveryFormat::Vtt, track, stream_index, &result);
    result
}

async fn deliver_vtt_inner(
    state: &AppState,
    id_str: &str,
    media_source_id: Option<&str>,
    stream_index: u32,
    forced_only: bool,
    style: SubtitleStyle,
    track: &mut DeliveredTrack,
) -> Result<HttpResponse, actix_web::Error> {
    let id: u64 = pharos_jellyfin_api::dto::parse_item_id(id_str)
        .ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
    let item = super::media_source::load_named(
        state,
        super::media_source::Route::Subtitles,
        id,
        media_source_id,
    )
    .await?;

    // Sidecar lookups first — they're free (no ffmpeg spawn).
    let sidecar_base = sidecar_base_index(&item.probe);
    if stream_index >= sidecar_base {
        let offset = (stream_index - sidecar_base) as usize;
        let sidecar = discover_sidecars(&item.path).await.into_iter().nth(offset);
        *track = DeliveredTrack::sidecar(sidecar.as_ref().map(|(p, _)| p.as_path()));
        let Some((sidecar_path, kind)) = sidecar else {
            return Err(error::ErrorNotFound("no sidecar at that index"));
        };
        return serve_sidecar(state, &sidecar_path, kind, stream_index).await;
    }
    *track = DeliveredTrack::embedded(&item, stream_index);

    // P15 — image-based subtitle codecs (PGS / DVB / VobSub) cannot
    // convert to WebVTT. Refuse with 415 + JSON error so clients
    // surface a clear "unsupported track" UI hint instead of the
    // generic 500 they'd get when ffmpeg fails the convert.
    // P26 — styled text codecs (ASS / SSA) survive the conversion
    // but lose colors / position / italics. Flag the response so
    // clients can choose burn-in via HLS instead.
    let mut style_lossy = false;
    if let Some(track) = item
        .probe
        .subtitle_tracks
        .iter()
        .find(|t| t.stream_index == stream_index)
    {
        if let Some(codec) = track.codec.as_deref() {
            if is_image_subtitle_codec(codec) {
                return Ok(HttpResponse::UnsupportedMediaType()
                    .content_type("application/json")
                    .body(format!(
                        r#"{{"error":"image-based subtitles cannot convert to WebVTT","codec":"{codec}"}}"#
                    )));
            }
            if is_styled_text_subtitle_codec(codec) {
                style_lossy = true;
            }
        }
        // P30 — `?ForcedOnly=1` requested AND this track does NOT
        // carry the forced disposition. Real Jellyfin clients pass
        // the flag to ask "give me only forced lines"; pharos can't
        // reliably filter cue-by-cue (ffmpeg converts ASS/SSA to
        // WebVTT without forced markers), so refuse and let the
        // client fall back to either the actually-forced track or
        // the burn-in HLS path.
        if forced_only && !track.is_forced {
            return Ok(HttpResponse::NotFound()
                .content_type("application/json")
                .body(
                    r#"{"error":"track is not a forced-only track; pick the forced disposition track"}"#,
                ));
        }
    }

    // Embedded stream: ffmpeg -map 0:<idx> -f webvtt.
    let input = item
        .path
        .to_str()
        .ok_or_else(|| error::ErrorInternalServerError("non-utf8 path"))?;

    // P5 — cache lookup before ffmpeg. Key includes mtime so a
    // mid-flight source edit invalidates the cached bytes.
    let mtime = mtime_secs(&item.path).await;
    if let Some(cache) = state.subtitles.as_ref() {
        if let Some(bytes) = cache
            .get(&item.path, mtime, stream_index, SubtitleKind::Embedded)
            .await
        {
            return Ok(vtt_response((*bytes).clone(), style_lossy, &style));
        }
        // Per-key fetch lock dedupes concurrent first-fetchers so
        // they share one ffmpeg spawn.
        let lock = cache
            .lock(&item.path, mtime, stream_index, SubtitleKind::Embedded)
            .await;
        let _guard = lock.lock().await;
        // Re-check — peer may have stored while we waited.
        if let Some(bytes) = cache
            .get(&item.path, mtime, stream_index, SubtitleKind::Embedded)
            .await
        {
            return Ok(vtt_response((*bytes).clone(), style_lossy, &style));
        }
        let out = run_ffmpeg_embedded(input, stream_index, &BgPermit::playback_priority()).await?;
        let stored = cache
            .store(&item.path, mtime, stream_index, SubtitleKind::Embedded, out)
            .await;
        return Ok(vtt_response((*stored).clone(), style_lossy, &style));
    }

    // No cache configured — fall back to the original spawn-per-fetch
    // path. (Default config keeps the cache on; this branch only
    // fires for tests / minimal deployments.)
    let out = run_ffmpeg_embedded(input, stream_index, &BgPermit::playback_priority()).await?;
    Ok(vtt_response(out, style_lossy, &style))
}

async fn stream_js(
    state: web::Data<AppState>,
    req: HttpRequest,
    path: web::Path<(String, String, u32)>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id, msid, stream_index) = path.into_inner();
    bearer(&state, &req, &id, "subtitle").await?;
    deliver_js(&state, &id, Some(&msid), stream_index).await
}

async fn stream_js_short(
    state: web::Data<AppState>,
    req: HttpRequest,
    path: web::Path<(String, u32)>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id, stream_index) = path.into_inner();
    bearer(&state, &req, &id, "subtitle").await?;
    deliver_js(&state, &id, None, stream_index).await
}

async fn stream_js_ticks(
    state: web::Data<AppState>,
    req: HttpRequest,
    path: web::Path<(String, String, u32, i64)>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id, msid, stream_index, _start_ticks) = path.into_inner();
    bearer(&state, &req, &id, "subtitle").await?;
    deliver_js(&state, &id, Some(&msid), stream_index).await
}

/// Serve a text subtitle track as jellyfin-web's JSON cue format
/// (`Stream.js`). The client renders these events itself. Reuses the same
/// WebVTT extraction as `Stream.vtt` (sidecar or embedded, cached) then
/// converts the cues to the `{ "TrackEvents": [...] }` shape jellyfin
/// expects. Image-codec tracks are rejected (they burn, never render here).
#[tracing::instrument(skip(state), fields(media.id = %id_str, idx = stream_index, kind = "js"))]
async fn deliver_js(
    state: &AppState,
    id_str: &str,
    media_source_id: Option<&str>,
    stream_index: u32,
) -> Result<HttpResponse, actix_web::Error> {
    let id: u64 = pharos_jellyfin_api::dto::parse_item_id(id_str)
        .ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
    let item = super::media_source::load_named(
        state,
        super::media_source::Route::Subtitles,
        id,
        media_source_id,
    )
    .await?;

    // Image subtitles can't become cue text — they burn into the transcode.
    if let Some(codec) = item
        .probe
        .subtitle_tracks
        .iter()
        .find(|t| t.stream_index == stream_index)
        .and_then(|t| t.codec.as_deref())
    {
        if is_image_subtitle_codec(&codec.to_ascii_lowercase()) {
            return Err(error::ErrorUnsupportedMediaType(
                "image subtitles render via burn-in, not Stream.js",
            ));
        }
    }

    let vtt = resolve_vtt_bytes(state, &item, stream_index).await?;
    let body = webvtt_to_track_events_json(&vtt);
    Ok(HttpResponse::Ok()
        .content_type("application/json; charset=utf-8")
        .insert_header((header::CACHE_CONTROL, "private, max-age=3600"))
        .body(body))
}

/// Resolve a text subtitle track to raw WebVTT bytes (the cue source for
/// both `Stream.vtt` and `Stream.js`): sidecar file (read or SRT→VTT
/// converted) for indices at/above `SIDECAR_BASE_INDEX`, else the embedded
/// stream extracted via ffmpeg. Cached like the `.vtt` path.
async fn resolve_vtt_bytes(
    state: &AppState,
    item: &pharos_core::MediaItem,
    stream_index: u32,
) -> Result<Vec<u8>, actix_web::Error> {
    let sidecar_base = sidecar_base_index(&item.probe);
    if stream_index >= sidecar_base {
        let offset = (stream_index - sidecar_base) as usize;
        let sidecars = discover_sidecars(&item.path).await;
        let Some((path, kind)) = sidecars.into_iter().nth(offset) else {
            return Err(error::ErrorNotFound("no sidecar at that index"));
        };
        return match kind {
            SidecarKind::Vtt => tokio::fs::read(&path)
                .await
                .map_err(|e| error::ErrorInternalServerError(format!("read: {e}"))),
            SidecarKind::Convert => {
                let input = path
                    .to_str()
                    .ok_or_else(|| error::ErrorInternalServerError("non-utf8 path"))?;
                let out = run_ffmpeg_srt_to_vtt(input).await?;
                Ok(out)
            }
        };
    }

    embedded_vtt(
        state.subtitles.as_ref(),
        item,
        stream_index,
        &BgPermit::playback_priority(),
    )
    .await
}

/// The WebVTT form of the embedded text stream `stream_index`, extracted once
/// and cached when a cache is given, under the caller's `permit`.
pub(crate) async fn embedded_vtt(
    cache: Option<&pharos_cache::SubtitleCache>,
    item: &pharos_core::MediaItem,
    stream_index: u32,
    permit: &BgPermit,
) -> Result<Vec<u8>, actix_web::Error> {
    let input = item
        .path
        .to_str()
        .ok_or_else(|| error::ErrorInternalServerError("non-utf8 path"))?;
    let mtime = mtime_secs(&item.path).await;
    if let Some(cache) = cache {
        if let Some(bytes) = cache
            .get(&item.path, mtime, stream_index, SubtitleKind::Embedded)
            .await
        {
            tracing::debug!(idx = stream_index, "subtitle cache hit (warm)");
            return Ok((*bytes).clone());
        }
        let lock = cache
            .lock(&item.path, mtime, stream_index, SubtitleKind::Embedded)
            .await;
        let _guard = lock.lock().await;
        if let Some(bytes) = cache
            .get(&item.path, mtime, stream_index, SubtitleKind::Embedded)
            .await
        {
            tracing::debug!(idx = stream_index, "subtitle cache hit (after lock)");
            return Ok((*bytes).clone());
        }
        // Cold: not pre-warmed by the backfill yet → extract now (blocks this
        // fetch). A slow one here is the "subtitles take too long" symptom.
        tracing::info!(
            idx = stream_index,
            "subtitle cache MISS — extracting on the request path"
        );
        let out = run_ffmpeg_embedded(input, stream_index, permit).await?;
        let stored = cache
            .store(&item.path, mtime, stream_index, SubtitleKind::Embedded, out)
            .await;
        return Ok((*stored).clone());
    }
    run_ffmpeg_embedded(input, stream_index, permit).await
}

/// Parse a WebVTT document into jellyfin's `{ "TrackEvents": [...] }` JSON.
/// Each cue becomes `{ Id, Text, StartPositionTicks, EndPositionTicks }`
/// (ticks = 100 ns units). Tolerant of an optional leading cue-id line and
/// `HH:MM:SS.mmm` or `MM:SS.mmm` timestamps.
fn webvtt_to_track_events_json(vtt: &[u8]) -> String {
    let text = String::from_utf8_lossy(vtt);
    let mut events = Vec::new();
    for block in text.split("\n\n").flat_map(|b| b.split("\r\n\r\n")) {
        let block = block.trim_matches(['\r', '\n', ' ']);
        if block.is_empty() || block.starts_with("WEBVTT") || block.starts_with("NOTE") {
            continue;
        }
        // Find the `-->` timing line; everything after it is cue text.
        let mut lines = block.lines();
        let mut timing: Option<(i64, i64)> = None;
        let mut cue_lines: Vec<&str> = Vec::new();
        for line in lines.by_ref() {
            if let Some((a, b)) = line.split_once("-->") {
                timing = parse_vtt_ts(a.trim())
                    .zip(parse_vtt_ts(b.trim().split(' ').next().unwrap_or("")));
            } else if timing.is_some() {
                cue_lines.push(line);
            }
            // Lines before the timing (a numeric/id line) are ignored.
        }
        let Some((start, end)) = timing else { continue };
        let text = cue_lines.join("\n");
        if text.trim().is_empty() {
            continue;
        }
        events.push(SubtitleTrackEventDto {
            id: events.len().to_string(),
            text,
            start_position_ticks: start,
            end_position_ticks: end,
        });
    }
    // B78/V38 — typed `SubtitleTrackInfo`/`SubtitleTrackEvent` rather than a
    // raw json! literal. Serialization of these owned structs cannot fail; a
    // valid empty envelope is the belt-and-braces fallback.
    serde_json::to_string(&SubtitleTrackInfoDto {
        track_events: events,
    })
    .unwrap_or_else(|_| r#"{"TrackEvents":[]}"#.to_string())
}

/// A single subtitle cue (Jellyfin `SubtitleTrackEvent`) in the Stream.js JSON
/// the web player's SubtitleEngine reads. All four fields are non-nullable in
/// the SDK model; typed per B78/V38.
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct SubtitleTrackEventDto {
    id: String,
    text: String,
    start_position_ticks: i64,
    end_position_ticks: i64,
}

/// The Stream.js envelope (Jellyfin `SubtitleTrackInfo`) wrapping the cue list.
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct SubtitleTrackInfoDto {
    track_events: Vec<SubtitleTrackEventDto>,
}

/// Parse a WebVTT timestamp (`HH:MM:SS.mmm` or `MM:SS.mmm`) to ticks
/// (100 ns units). Returns `None` on a malformed stamp.
fn parse_vtt_ts(s: &str) -> Option<i64> {
    let (hms, millis) = s.split_once('.').or_else(|| s.split_once(','))?;
    let ms: i64 = millis.get(..3).unwrap_or(millis).parse().ok()?;
    let parts: Vec<&str> = hms.split(':').collect();
    let (h, m, sec) = match parts.as_slice() {
        [h, m, s] => (
            h.parse::<i64>().ok()?,
            m.parse::<i64>().ok()?,
            s.parse::<i64>().ok()?,
        ),
        [m, s] => (0, m.parse::<i64>().ok()?, s.parse::<i64>().ok()?),
        _ => return None,
    };
    let total_ms = ((h * 3600 + m * 60 + sec) * 1000) + ms;
    Some(total_ms * 10_000) // 1 ms = 10,000 ticks
}

/// P33 — per-user subtitle styling. Resolved from the user's
/// `UserConfiguration.SubtitleSettings` JSON blob (jellyfin-web's
/// shape: `{ Color: "#ffff00", Background: "...", Position: "Bottom" }`).
#[derive(Debug, Default, Clone)]
pub struct SubtitleStyle {
    pub color: Option<String>,
    pub background: Option<String>,
    pub font_size: Option<String>,
    pub position: Option<String>,
}

impl SubtitleStyle {
    pub fn is_empty(&self) -> bool {
        self.color.is_none()
            && self.background.is_none()
            && self.font_size.is_none()
            && self.position.is_none()
    }

    /// Emit a WebVTT STYLE block applying the captured prefs to all
    /// cues. Returns an empty Vec when no prefs are set.
    pub fn render_style_block(&self) -> Vec<u8> {
        if self.is_empty() {
            return Vec::new();
        }
        let mut s = String::from("STYLE\n::cue {\n");
        if let Some(c) = sanitise_css_value(self.color.as_deref()) {
            s.push_str(&format!("  color: {c};\n"));
        }
        if let Some(c) = sanitise_css_value(self.background.as_deref()) {
            s.push_str(&format!("  background-color: {c};\n"));
        }
        if let Some(c) = sanitise_css_value(self.font_size.as_deref()) {
            s.push_str(&format!("  font-size: {c};\n"));
        }
        s.push_str("}\n\n");
        s.into_bytes()
    }
}

/// P33 — guard rail for CSS values. Reject `;` / `{` / `}` / quote /
/// newline so a malicious config blob can't break out of the
/// `::cue` rule and inject arbitrary CSS into the served WebVTT.
fn sanitise_css_value(v: Option<&str>) -> Option<String> {
    let v = v?.trim();
    if v.is_empty()
        || v.bytes()
            .any(|b| matches!(b, b';' | b'{' | b'}' | b'\n' | b'\r' | b'"' | b'\''))
    {
        return None;
    }
    Some(v.to_string())
}

/// P33 — read the bound user's `UserConfiguration.SubtitleSettings`
/// JSON object. Tolerates missing fields + a missing configuration
/// row (defaults to no styling).
pub(crate) async fn subtitle_style_for(
    state: &AppState,
    user_id: pharos_core::UserId,
) -> SubtitleStyle {
    use pharos_core::PreferenceStore;
    let json = match state.stores.get_user_configuration(user_id).await {
        Ok(Some(j)) => j,
        _ => return SubtitleStyle::default(),
    };
    let v: serde_json::Value = match serde_json::from_str(&json) {
        Ok(v) => v,
        Err(_) => return SubtitleStyle::default(),
    };
    // jellyfin-web stores the block under `SubtitleSettings`, but
    // older clients used flat `Subtitle*` keys. Accept both.
    let settings = v.get("SubtitleSettings").cloned().unwrap_or(v);
    let pick = |key: &str| -> Option<String> {
        settings
            .get(key)
            .and_then(|x| x.as_str().map(|s| s.to_string()))
    };
    SubtitleStyle {
        color: pick("Color").or_else(|| pick("SubtitleColor")),
        background: pick("Background").or_else(|| pick("SubtitleBackground")),
        font_size: pick("FontSize").or_else(|| pick("SubtitleFontSize")),
        position: pick("Position").or_else(|| pick("SubtitlePosition")),
    }
}

/// P26 — shared WebVTT response builder. Adds the
/// `X-Subtitle-Style-Lossy` header + an in-body WEBVTT NOTE comment
/// when the source codec carried styling that didn't survive the
/// conversion (ASS/SSA). P33 — prepends a STYLE block carrying the
/// per-user color/background/font-size prefs when set.
fn vtt_response(mut body: Vec<u8>, style_lossy: bool, style: &SubtitleStyle) -> HttpResponse {
    let user_style = style.render_style_block();
    if style_lossy || !user_style.is_empty() {
        let mut prefixed = Vec::with_capacity(body.len() + 256);
        prefixed.extend_from_slice(b"WEBVTT\n");
        if style_lossy {
            prefixed.extend_from_slice(
                b"NOTE Source format was ASS/SSA; styling lost in WebVTT conversion.\n\n",
            );
        } else {
            prefixed.extend_from_slice(b"\n");
        }
        if !user_style.is_empty() {
            prefixed.extend_from_slice(&user_style);
        }
        if body.starts_with(b"WEBVTT") {
            if let Some(nl) = body.iter().position(|c| *c == b'\n') {
                body = body[nl + 1..].to_vec();
            }
        }
        if body.starts_with(b"\n") {
            body = body[1..].to_vec();
        }
        prefixed.extend_from_slice(&body);
        body = prefixed;
    }
    let mut builder = HttpResponse::Ok();
    builder
        .content_type("text/vtt; charset=utf-8")
        .insert_header((header::CACHE_CONTROL, "private, max-age=3600"));
    if style_lossy {
        builder.insert_header(("X-Subtitle-Style-Lossy", "true"));
    }
    builder.body(body)
}

async fn run_ffmpeg_embedded(
    input: &str,
    stream_index: u32,
    _permit: &BgPermit,
) -> Result<Vec<u8>, actix_web::Error> {
    // Cold subtitle extraction opens the whole (often multi-GB, NFS-backed)
    // source to demux one text stream — the dominant cost of a first Stream.vtt
    // / Stream.js fetch. Timed so a slow first-fetch is visible in traces.
    let started = std::time::Instant::now();
    let out = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-i",
            input,
            "-map",
            &format!("0:{stream_index}"),
            "-c:s",
            "webvtt",
            "-f",
            "webvtt",
            "pipe:1",
        ])
        .output()
        .await
        .map_err(|e| error::ErrorInternalServerError(format!("ffmpeg spawn: {e}")))?;
    if !out.status.success() {
        return Err(error::ErrorNotFound(format!(
            "ffmpeg extract: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    tracing::info!(
        idx = stream_index,
        bytes = out.stdout.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "subtitle webvtt extracted (ffmpeg, cold demux)"
    );
    Ok(out.stdout)
}

/// P26 — text codecs that carry styling ffmpeg's `-c:s webvtt`
/// strips on conversion (colors / position / italics). Clients see
/// the response header + DTO flag and can fall back to HLS burn-in
/// when they care about styling fidelity (anime / fan-sub libraries).
fn is_styled_text_subtitle_codec(codec: &str) -> bool {
    matches!(
        codec.to_ascii_lowercase().as_str(),
        "ass" | "ssa" | "advanced substation alpha"
    )
}

/// Which form a text subtitle went out in, the label of
/// `pharos_subtitle_delivery_total{format}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeliveryFormat {
    /// The original ASS/SSA script, for a renderer that draws its styling.
    Ass,
    /// WebVTT, plain text.
    Vtt,
}

impl DeliveryFormat {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 2] = [Self::Ass, Self::Vtt];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Ass => "ass",
            Self::Vtt => "vtt",
        }
    }
}

/// Where the delivered track lives, the label `{source}`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum DeliverySource {
    Embedded,
    Sidecar,
    /// The Pharos styled track compiled from a stored plan (spec 028).
    Generated,
    /// The request failed before the item was read (bad id, no item).
    #[default]
    Unknown,
}

impl DeliverySource {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 4] = [
        Self::Embedded,
        Self::Sidecar,
        Self::Generated,
        Self::Unknown,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Embedded => "embedded",
            Self::Sidecar => "sidecar",
            Self::Generated => "generated",
            Self::Unknown => "unknown",
        }
    }
}

/// What the track's codec can carry, the label `{codec_class}`. `styled`
/// with `format="vtt"` is an ASS/SSA track going out as plain text: a client
/// with no ASS renderer, or the native player's hidden `<track>` fallback,
/// which it fetches beside `stream.ass` (B600).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum CodecClass {
    Styled,
    Plain,
    Image,
    #[default]
    Unknown,
}

impl CodecClass {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 4] = [Self::Styled, Self::Plain, Self::Image, Self::Unknown];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Styled => "styled",
            Self::Plain => "plain",
            Self::Image => "image",
            Self::Unknown => "unknown",
        }
    }

    fn of_codec(codec: &str) -> Self {
        if is_styled_text_subtitle_codec(codec) {
            Self::Styled
        } else if is_image_subtitle_codec(codec) {
            Self::Image
        } else {
            Self::Plain
        }
    }
}

/// The track a subtitle request named, as far as the handler got.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct DeliveredTrack {
    pub source: DeliverySource,
    pub class: CodecClass,
    /// The codec (embedded) or file extension (sidecar), for the log line.
    pub codec: String,
}

impl DeliveredTrack {
    /// The sidecar at the requested index, `None` when there is none.
    fn sidecar(path: Option<&std::path::Path>) -> Self {
        let ext = path
            .and_then(|p| p.extension())
            .map(|e| e.to_string_lossy().to_lowercase());
        Self {
            source: DeliverySource::Sidecar,
            class: ext
                .as_deref()
                .map_or(CodecClass::Unknown, CodecClass::of_codec),
            codec: ext.unwrap_or_default(),
        }
    }

    /// The styled track: generated ASS, which no source codec describes.
    fn generated() -> Self {
        Self {
            source: DeliverySource::Generated,
            class: CodecClass::Styled,
            codec: "ass".to_owned(),
        }
    }

    fn embedded(item: &pharos_core::MediaItem, stream_index: u32) -> Self {
        let codec = item
            .probe
            .subtitle_tracks
            .iter()
            .find(|t| t.stream_index == stream_index)
            .and_then(|t| t.codec.clone());
        Self {
            source: DeliverySource::Embedded,
            class: codec
                .as_deref()
                .map_or(CodecClass::Unknown, CodecClass::of_codec),
            codec: codec.unwrap_or_default(),
        }
    }
}

/// How a subtitle request ended, the label `{outcome}`.
pub(crate) fn delivery_outcome(result: &Result<HttpResponse, actix_web::Error>) -> &'static str {
    let status = match result {
        Ok(resp) => resp.status(),
        Err(e) => e.as_response_error().status_code(),
    };
    if status.is_success() {
        "served"
    } else if status.is_client_error() {
        "refused"
    } else {
        "failed"
    }
}

/// B600: count and log every text subtitle delivery with the form it took.
/// A styled track fetched as WebVTT with no `stream.ass` beside it is one
/// drawn as plain text; the player's own fallback reports itself as
/// `rendererMode mode=plain`.
/// The failure path records the same fields as the success path.
fn record_delivery(
    format: DeliveryFormat,
    track: DeliveredTrack,
    stream_index: u32,
    result: &Result<HttpResponse, actix_web::Error>,
) {
    let outcome = delivery_outcome(result);
    metrics::counter!(
        "pharos_subtitle_delivery_total",
        "format" => format.label(),
        "source" => track.source.label(),
        "codec_class" => track.class.label(),
        "outcome" => outcome,
    )
    .increment(1);
    let reason = match (format, track.class) {
        (_, CodecClass::Unknown) => "track not identified",
        (DeliveryFormat::Ass, CodecClass::Styled) => "script asked for, styling kept",
        (DeliveryFormat::Vtt, CodecClass::Styled) => {
            "styled track asked for as WebVTT, as a fallback or by a client without ASS"
        }
        (DeliveryFormat::Ass, _) => "non-ASS track asked for as ASS",
        (DeliveryFormat::Vtt, _) => "text track as WebVTT",
    };
    let error = result.as_ref().err().map(ToString::to_string);
    tracing::info!(
        format = format.label(),
        source = track.source.label(),
        codec_class = track.class.label(),
        codec = %track.codec,
        idx = stream_index,
        outcome,
        error = error.as_deref().unwrap_or(""),
        reason,
        "subtitle delivery"
    );
}

/// P15 — known image-based subtitle codecs. ffmpeg can't convert
/// these to text-WebVTT (they're rasters); attempts produce empty
/// or malformed output. Refused with 415 so the client doesn't
/// mistake an empty track for a working one.
pub(crate) fn is_image_subtitle_codec(codec: &str) -> bool {
    matches!(
        codec.to_ascii_lowercase().as_str(),
        "hdmv_pgs_subtitle"
            | "pgs"
            | "pgssub"
            | "dvb_subtitle"
            | "dvbsub"
            | "dvd_subtitle"
            | "dvdsub"
            | "vobsub"
    )
}

/// Sidecar kind — `.vtt` ships as-is; every other text format
/// (`.srt`/`.ass`/`.ssa`) runs through ffmpeg to WebVTT (browsers consume
/// `<track>` as WebVTT only). ffmpeg's `-f webvtt` auto-detects the input
/// format, so one convert path covers them all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidecarKind {
    Vtt,
    Convert,
}

/// Classify a lowercased filename by its subtitle extension. `None` for
/// non-subtitle / image formats (`.sup`, VobSub `.idx`/`.sub`) that can't
/// become WebVTT.
fn sidecar_kind_for_name(lower_name: &str) -> Option<SidecarKind> {
    if lower_name.ends_with(".vtt") {
        Some(SidecarKind::Vtt)
    } else if lower_name.ends_with(".srt")
        || lower_name.ends_with(".ass")
        || lower_name.ends_with(".ssa")
    {
        Some(SidecarKind::Convert)
    } else {
        None
    }
}

/// Find a direct sub-directory of `dir` whose name case-insensitively equals
/// `name_lower`. Returns its path. Used to locate `Subs/`/`Subtitles/` and
/// per-episode folders regardless of casing.
async fn find_subdir_ci(dir: &std::path::Path, name_lower: &str) -> Option<std::path::PathBuf> {
    let mut entries = tokio::fs::read_dir(dir).await.ok()?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
            if let Some(n) = entry.file_name().to_str() {
                if n.to_ascii_lowercase() == name_lower {
                    return Some(entry.path());
                }
            }
        }
    }
    None
}

/// Push every subtitle file in `dir` whose name begins `<stem_lower>.` (the
/// classic `Episode 01.eng.srt` layout). Silent no-op when `dir` is absent.
async fn scan_stem_matched(
    dir: &std::path::Path,
    stem_lower: &str,
    found: &mut Vec<(std::path::PathBuf, SidecarKind)>,
) {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let p = entry.path();
        let Some(name) = p.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let lower = name.to_ascii_lowercase();
        // Require `<stem>.` so `<stem>extra.srt` doesn't match, but any
        // trailing `.lang`/`.forced` segments are allowed.
        if !lower.starts_with(stem_lower) || !lower[stem_lower.len()..].starts_with('.') {
            continue;
        }
        if let Some(kind) = sidecar_kind_for_name(&lower) {
            found.push((p, kind));
        }
    }
}

/// Parse the subtitle language from a sidecar filename, e.g.
/// `Movie.eng.forced.srt` → `eng`, `English.srt` → `English`, `Movie.srt` →
/// None. Scans the dot-separated tokens before the extension right-to-left,
/// skipping disposition tags (forced/sdh/…) to reach the language; a token
/// that's neither a disposition nor a recognisable language ends the search
/// (it's the title/stem).
pub(crate) fn sidecar_language_from_name(name: &str) -> Option<String> {
    const DISPOSITIONS: &[&str] = &[
        "forced", "sdh", "hi", "cc", "default", "foreign", "full", "normal",
    ];
    let base = name.rsplit_once('.').map(|(b, _)| b)?; // drop extension
    for tok in base.split('.').rev() {
        let t = tok.trim();
        if t.is_empty() {
            continue;
        }
        if DISPOSITIONS.contains(&t.to_ascii_lowercase().as_str()) {
            continue;
        }
        if is_language_token(t) {
            return Some(t.to_string());
        }
        break; // title/stem token — no language present
    }
    None
}

/// Heuristic: is `t` a language identifier? A 2-3 letter ISO-639 code, a
/// locale like `pt-BR`, or a spelled-out English language name.
fn is_language_token(t: &str) -> bool {
    const NAMES: &[&str] = &[
        "english",
        "japanese",
        "spanish",
        "french",
        "german",
        "italian",
        "chinese",
        "korean",
        "russian",
        "portuguese",
        "arabic",
        "dutch",
        "polish",
        "swedish",
        "danish",
        "finnish",
        "norwegian",
        "hindi",
        "turkish",
        "greek",
        "hebrew",
        "thai",
        "czech",
        "hungarian",
        "romanian",
    ];
    let lower = t.to_ascii_lowercase();
    if NAMES.contains(&lower.as_str()) {
        return true;
    }
    // Locale form: pt-BR, zh-CN.
    if let Some((a, b)) = t.split_once('-') {
        return (2..=3).contains(&a.len())
            && a.chars().all(|c| c.is_ascii_alphabetic())
            && !b.is_empty()
            && b.chars().all(|c| c.is_ascii_alphanumeric());
    }
    // Bare ISO code: 2 or 3 letters.
    (2..=3).contains(&t.len()) && t.chars().all(|c| c.is_ascii_alphabetic())
}

/// Push EVERY subtitle file in `dir` regardless of name — for a per-episode
/// subtitle folder whose files are named by language (`English.srt`,
/// `eng.ass`) rather than after the video.
async fn scan_all_subs(dir: &std::path::Path, found: &mut Vec<(std::path::PathBuf, SidecarKind)>) {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let p = entry.path();
        let Some(name) = p.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if let Some(kind) = sidecar_kind_for_name(&name.to_ascii_lowercase()) {
            found.push((p, kind));
        }
    }
}

/// Cache of discovered sidecars keyed by media path, validated by the parent
/// directory's mtime. Sidecar discovery walks the filesystem (multiple
/// `read_dir`s across `Subs/`/`Subtitles/`/per-episode folders) and runs live
/// on every PlaybackInfo + subtitle fetch — expensive over NFS (this cluster
/// mounts with `lookupcache=none`). The parent mtime changes whenever a sub is
/// added/removed directly beside the media (the common case), so a match means
/// "nothing to re-scan". Caveat: a file added *inside* an existing
/// `Subs/<stem>/` subfolder doesn't bump the parent mtime, so that rare case is
/// picked up on the next server restart / library refresh.
type SidecarList = Vec<(std::path::PathBuf, SidecarKind)>;
/// `(parent dir mtime secs, OpenSubtitles store dir mtime nanos, list)`.
type SidecarCache = std::collections::HashMap<std::path::PathBuf, (u64, u64, SidecarList)>;
static SIDECAR_CACHE: std::sync::LazyLock<std::sync::Mutex<SidecarCache>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Discover sidecar subtitle files for `media_path`, "no matter where they
/// live" (see [`discover_sidecars_uncached`]). Memoised per media path +
/// parent-dir mtime so repeated playback requests don't re-walk the folder.
pub async fn discover_sidecars(
    media_path: &std::path::Path,
) -> Vec<(std::path::PathBuf, SidecarKind)> {
    let Some(parent) = media_path.parent() else {
        return Vec::new();
    };
    let mtime = tokio::fs::metadata(parent)
        .await
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Downloaded subtitles live on the cache volume, not beside the media, so a
    // download bumps this directory's mtime rather than the parent's (nanos:
    // a download in the same second as the last scan must still invalidate).
    let store_mtime = match crate::opensubtitles::store::installed_root() {
        Some(root) => tokio::fs::metadata(crate::opensubtitles::store::item_dir(root, media_path))
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|d| u64::try_from(d.as_nanos()).ok())
            .unwrap_or(0),
        None => 0,
    };
    let key = media_path.to_path_buf();
    if let Ok(cache) = SIDECAR_CACHE.lock() {
        if let Some((cached_mtime, cached_store, subs)) = cache.get(&key) {
            if *cached_mtime == mtime && *cached_store == store_mtime {
                return subs.clone();
            }
        }
    }
    let result = discover_sidecars_uncached(media_path).await;
    if let Ok(mut cache) = SIDECAR_CACHE.lock() {
        cache.insert(key, (mtime, store_mtime, result.clone()));
    }
    result
}

/// The actual filesystem walk (see [`discover_sidecars`] for the memoised
/// entry point). Covers, in a stable order:
/// - the same directory, `<stem>[.<lang>…].{srt,vtt,ass,ssa}`;
/// - `Subs/` and `Subtitles/` folders next to the media (case-insensitive),
///   both stem-matched files and a per-episode `Subs/<stem>/` subfolder;
/// - a per-episode `<stem>/` folder next to the media.
///
/// Returns `(sidecar_path, kind)` in ascending-path order so the numeric
/// `stream_index` offsets stay consistent between PlaybackInfo and Stream.vtt
/// fetches.
async fn discover_sidecars_uncached(
    media_path: &std::path::Path,
) -> Vec<(std::path::PathBuf, SidecarKind)> {
    let Some(parent) = media_path.parent() else {
        return Vec::new();
    };
    let Some(stem) = media_path.file_stem().and_then(|s| s.to_str()) else {
        return Vec::new();
    };
    let stem_lower = stem.to_ascii_lowercase();
    let mut found: Vec<(std::path::PathBuf, SidecarKind)> = Vec::new();

    // (a) same directory, stem-matched.
    scan_stem_matched(parent, &stem_lower, &mut found).await;

    // (b) dedicated subtitle folders next to the media.
    for sub in ["subs", "subtitles"] {
        if let Some(dir) = find_subdir_ci(parent, sub).await {
            scan_stem_matched(&dir, &stem_lower, &mut found).await;
            // Per-episode folder inside, e.g. `Subs/<stem>/English.srt`.
            if let Some(epdir) = find_subdir_ci(&dir, &stem_lower).await {
                scan_all_subs(&epdir, &mut found).await;
            }
        }
    }

    // (c) per-episode folder directly beside the media, `<stem>/English.srt`.
    if let Some(epdir) = find_subdir_ci(parent, &stem_lower).await {
        scan_all_subs(&epdir, &mut found).await;
    }

    // (d) subtitles downloaded from OpenSubtitles, kept on the cache volume
    // (spec 030).
    if let Some(root) = crate::opensubtitles::store::installed_root() {
        scan_all_subs(
            &crate::opensubtitles::store::item_dir(root, media_path),
            &mut found,
        )
        .await;
    }

    found.sort_by(|a, b| a.0.cmp(&b.0));
    found.dedup_by(|a, b| a.0 == b.0);
    found
}

async fn serve_sidecar(
    state: &AppState,
    path: &std::path::Path,
    kind: SidecarKind,
    stream_index: u32,
) -> Result<HttpResponse, actix_web::Error> {
    match kind {
        SidecarKind::Vtt => {
            // VTT is already in the target format — disk-read is the
            // hot path. No cache (would just shadow the page cache).
            let bytes = tokio::fs::read(path)
                .await
                .map_err(|e| error::ErrorInternalServerError(format!("read: {e}")))?;
            Ok(HttpResponse::Ok()
                .content_type("text/vtt; charset=utf-8")
                .insert_header((header::CACHE_CONTROL, "private, max-age=3600"))
                .body(bytes))
        }
        SidecarKind::Convert => {
            let input = path
                .to_str()
                .ok_or_else(|| error::ErrorInternalServerError("non-utf8 path"))?;
            // P5 — SRT → WebVTT cache.
            let mtime = mtime_secs(path).await;
            if let Some(cache) = state.subtitles.as_ref() {
                if let Some(bytes) = cache
                    .get(path, mtime, stream_index, SubtitleKind::Sidecar)
                    .await
                {
                    return Ok(HttpResponse::Ok()
                        .content_type("text/vtt; charset=utf-8")
                        .insert_header((header::CACHE_CONTROL, "private, max-age=3600"))
                        .body((*bytes).clone()));
                }
                let lock = cache
                    .lock(path, mtime, stream_index, SubtitleKind::Sidecar)
                    .await;
                let _guard = lock.lock().await;
                if let Some(bytes) = cache
                    .get(path, mtime, stream_index, SubtitleKind::Sidecar)
                    .await
                {
                    return Ok(HttpResponse::Ok()
                        .content_type("text/vtt; charset=utf-8")
                        .insert_header((header::CACHE_CONTROL, "private, max-age=3600"))
                        .body((*bytes).clone()));
                }
                let out = run_ffmpeg_srt_to_vtt(input).await?;
                let stored = cache
                    .store(path, mtime, stream_index, SubtitleKind::Sidecar, out)
                    .await;
                return Ok(HttpResponse::Ok()
                    .content_type("text/vtt; charset=utf-8")
                    .insert_header((header::CACHE_CONTROL, "private, max-age=3600"))
                    .body((*stored).clone()));
            }
            let out = run_ffmpeg_srt_to_vtt(input).await?;
            Ok(HttpResponse::Ok()
                .content_type("text/vtt; charset=utf-8")
                .insert_header((header::CACHE_CONTROL, "private, max-age=3600"))
                .body(out))
        }
    }
}

async fn run_ffmpeg_srt_to_vtt(input: &str) -> Result<Vec<u8>, actix_web::Error> {
    let out = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-i",
            input,
            "-c:s",
            "webvtt",
            "-f",
            "webvtt",
            "pipe:1",
        ])
        .output()
        .await
        .map_err(|e| error::ErrorInternalServerError(format!("ffmpeg spawn: {e}")))?;
    if !out.status.success() {
        return Err(error::ErrorInternalServerError(format!(
            "ffmpeg srt→vtt: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(out.stdout)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// B600: `pharos_subtitle_delivery_total` labels are a dashboard
    /// contract, distinct within each label.
    #[test]
    fn delivery_labels_are_distinct() {
        use std::collections::HashSet;
        let f: HashSet<_> = DeliveryFormat::ALL.iter().map(|x| x.label()).collect();
        let s: HashSet<_> = DeliverySource::ALL.iter().map(|x| x.label()).collect();
        let c: HashSet<_> = CodecClass::ALL.iter().map(|x| x.label()).collect();
        assert_eq!(f.len(), DeliveryFormat::ALL.len());
        assert_eq!(s.len(), DeliverySource::ALL.len());
        assert_eq!(c.len(), CodecClass::ALL.len());
    }

    #[test]
    fn a_codec_or_sidecar_extension_has_a_class() {
        for (codec, class) in [
            ("ass", CodecClass::Styled),
            ("SSA", CodecClass::Styled),
            ("subrip", CodecClass::Plain),
            ("srt", CodecClass::Plain),
            ("vtt", CodecClass::Plain),
            ("hdmv_pgs_subtitle", CodecClass::Image),
        ] {
            assert_eq!(CodecClass::of_codec(codec), class, "{codec}");
        }
    }

    #[test]
    fn a_delivery_outcome_follows_the_status() {
        assert_eq!(delivery_outcome(&Ok(HttpResponse::Ok().finish())), "served");
        assert_eq!(
            delivery_outcome(&Ok(HttpResponse::UnsupportedMediaType().finish())),
            "refused"
        );
        assert_eq!(delivery_outcome(&Err(error::ErrorNotFound("x"))), "refused");
        assert_eq!(
            delivery_outcome(&Err(error::ErrorInternalServerError("x"))),
            "failed"
        );
    }

    #[test]
    fn batch_extract_opens_source_once_for_all_tracks() {
        // V36 / B72: N text tracks must demux in ONE source open, never a loop
        // of whole-file NFS reads. The arg builder is the structural guarantee —
        // exactly one `-i input`, then one -map/-c:s/-f/output group per track.
        let tracks = vec![
            BatchTrack {
                stream_index: 2,
                muxer: "webvtt",
                out: "/tmp/a.webvtt".into(),
            },
            BatchTrack {
                stream_index: 3,
                muxer: "ass",
                out: "/tmp/b.ass".into(),
            },
            BatchTrack {
                stream_index: 5,
                muxer: "webvtt",
                out: "/tmp/c.webvtt".into(),
            },
        ];
        let args = build_batch_extract_args("/media/show.mkv", &tracks);
        // Exactly one input open regardless of track count — the whole point.
        assert_eq!(
            args.iter().filter(|a| a.as_str() == "-i").count(),
            1,
            "batch must open the source exactly once, got {args:?}"
        );
        let i = args.iter().position(|a| a == "-i").unwrap();
        assert_eq!(args[i + 1], "/media/show.mkv");
        // One -map per track, each followed by its codec/muxer/output.
        assert_eq!(args.iter().filter(|a| a.as_str() == "-map").count(), 3);
        for t in &tracks {
            let m = args
                .iter()
                .position(|a| *a == format!("0:{}", t.stream_index))
                .unwrap_or_else(|| panic!("stream {} not mapped in {args:?}", t.stream_index));
            assert_eq!(args[m - 1], "-map");
            assert_eq!(args[m + 1], "-c:s");
            assert_eq!(args[m + 2], t.muxer);
            assert_eq!(args[m + 3], "-f");
            assert_eq!(args[m + 4], t.muxer);
            assert_eq!(args[m + 5], t.out.to_string_lossy());
        }
    }

    #[test]
    fn sidecar_language_parsed_from_filename() {
        // The token before the extension is the language, unless it's a
        // disposition tag (forced/sdh/…). Missing → None.
        assert_eq!(
            sidecar_language_from_name("Movie.eng.srt").as_deref(),
            Some("eng")
        );
        assert_eq!(
            sidecar_language_from_name("Movie.en.vtt").as_deref(),
            Some("en")
        );
        assert_eq!(
            sidecar_language_from_name("Show.S01E01.pt-BR.ass").as_deref(),
            Some("pt-BR")
        );
        // Disposition tags are skipped to reach the language.
        assert_eq!(
            sidecar_language_from_name("Movie.eng.forced.srt").as_deref(),
            Some("eng")
        );
        assert_eq!(
            sidecar_language_from_name("Movie.en.sdh.srt").as_deref(),
            Some("en")
        );
        // Language-named files in a per-episode subs folder.
        assert_eq!(
            sidecar_language_from_name("English.srt").as_deref(),
            Some("English")
        );
        // No language token.
        assert_eq!(sidecar_language_from_name("Movie.srt"), None);
        assert_eq!(sidecar_language_from_name("Movie.forced.srt"), None);
    }

    #[test]
    fn vtt_ts_parses_to_ticks() {
        // 1s = 10,000,000 ticks.
        assert_eq!(parse_vtt_ts("00:00:01.000"), Some(10_000_000));
        assert_eq!(parse_vtt_ts("01:02:03.500"), Some(37_235_000_000)); // 3723.5s
        assert_eq!(parse_vtt_ts("00:05.250"), Some(52_500_000)); // MM:SS form
        assert_eq!(parse_vtt_ts("garbage"), None);
    }

    #[test]
    fn webvtt_converts_to_trackevents_json() {
        let vtt = b"WEBVTT\n\n1\n00:00:01.000 --> 00:00:04.000\nHello world\n\n\
                    00:00:05.000 --> 00:00:06.500 line:90%\nSecond\ncue\n";
        let json = webvtt_to_track_events_json(vtt);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let events = v["TrackEvents"].as_array().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["Text"], "Hello world");
        assert_eq!(events[0]["StartPositionTicks"], 10_000_000);
        assert_eq!(events[0]["EndPositionTicks"], 40_000_000);
        // Multi-line cue joins with \n; trailing cue settings on the timing
        // line (`line:90%`) are ignored.
        assert_eq!(events[1]["Text"], "Second\ncue");
        assert_eq!(events[1]["StartPositionTicks"], 50_000_000);
    }

    #[test]
    fn empty_style_renders_no_block() {
        // P33 — when the user never set any subtitle prefs the VTT
        // body must be byte-identical to the un-styled output so the
        // subtitle cache stays a single shared entry across users.
        let s = SubtitleStyle::default();
        assert!(s.is_empty());
        assert!(s.render_style_block().is_empty());
    }

    #[test]
    fn image_manifest_events_carry_fetchable_urls() {
        let raw = br#"{"width":720,"height":480,"complete":false,"through_ms":6000,
            "events":[{"start_ms":500,"end_ms":2500,"x":240,"y":400,"width":240,
            "height":48,"forced":false,"file":"0.png"}]}"#;
        let got = manifest_with_urls(Some(raw), "/videos/9/subtitles/2/images", "7-1").unwrap();
        let event = &got["events"][0];
        assert_eq!(event["url"], "/videos/9/subtitles/2/images/0.png?v=7-1");
        assert!(event.get("file").is_none(), "the disk name stays private");
        assert_eq!(event["start_ms"], 500);
        assert_eq!(got["width"], 720);
        assert_eq!(got["complete"], false);
    }

    #[test]
    fn an_unwritten_image_manifest_reads_as_incomplete_and_empty() {
        let got = manifest_with_urls(None, "/x", "7-1").unwrap();
        assert_eq!(got["complete"], false);
        assert_eq!(got["events"], serde_json::json!([]));
    }

    #[test]
    fn styled_block_includes_color_and_background_in_cue_rule() {
        let s = SubtitleStyle {
            color: Some("#ffff00".into()),
            background: Some("rgba(0,0,0,0.5)".into()),
            font_size: Some("120%".into()),
            position: None,
        };
        let block = s.render_style_block();
        let text = std::str::from_utf8(&block).unwrap();
        assert!(text.starts_with("STYLE\n::cue {\n"));
        assert!(text.contains("color: #ffff00;"));
        assert!(text.contains("background-color: rgba(0,0,0,0.5);"));
        assert!(text.contains("font-size: 120%;"));
        assert!(text.ends_with("}\n\n"));
    }

    #[test]
    fn css_injection_via_style_value_gets_rejected() {
        // P33 — guard rail. A malicious user configuration that
        // tried to break out of `::cue` and inject `body { ... }`
        // must not make it into the served WebVTT body.
        let s = SubtitleStyle {
            color: Some("#fff; } body { display:none; ::cue {".into()),
            background: Some("red\nINJECT".into()),
            font_size: Some("12px}".into()),
            position: None,
        };
        let block = s.render_style_block();
        let text = std::str::from_utf8(&block).unwrap();
        assert!(!text.contains("body"), "got: {text}");
        assert!(!text.contains("INJECT"), "got: {text}");
        // The struct was not empty so the block opens, but with all
        // values rejected the body is just the empty `::cue { }`.
        assert!(text.starts_with("STYLE\n::cue {\n"));
        assert!(!text.contains("color:"));
        assert!(!text.contains("background-color:"));
        assert!(!text.contains("font-size:"));
    }

    /// V565: a download kept on the cache volume joins the item's external
    /// subtitles with its language, and a download after a first discovery is
    /// seen (the memo is invalidated by the store directory's mtime).
    #[tokio::test]
    async fn discover_sidecars_includes_opensubtitles_downloads() {
        let td = TempDir::new().unwrap();
        let media = td.path().join("dl-show.mkv");
        tokio::fs::write(&media, b"x").await.unwrap();
        let root = crate::opensubtitles::store::shared_test_root();
        assert!(discover_sidecars(&media).await.is_empty());
        crate::opensubtitles::store::save(
            &root,
            &media,
            "en",
            31,
            b"1\n00:00:01,000 --> 00:00:02,000\nhi\n",
        )
        .await
        .unwrap();
        let found = discover_sidecars(&media).await;
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].0.starts_with(&root));
        assert_eq!(found[0].1, SidecarKind::Convert);
        assert_eq!(
            sidecar_language_from_name(found[0].0.file_name().unwrap().to_str().unwrap())
                .as_deref(),
            Some("en")
        );
        let mut beside = tokio::fs::read_dir(td.path()).await.unwrap();
        let mut n = 0;
        while beside.next_entry().await.unwrap().is_some() {
            n += 1;
        }
        assert_eq!(n, 1, "nothing was written beside the media");
    }

    #[tokio::test]
    async fn discover_sidecars_returns_vtt_and_srt_in_order() {
        let td = TempDir::new().unwrap();
        let media = td.path().join("show.mkv");
        tokio::fs::write(&media, b"x").await.unwrap();
        tokio::fs::write(td.path().join("show.eng.vtt"), b"WEBVTT\n")
            .await
            .unwrap();
        tokio::fs::write(td.path().join("show.fra.vtt"), b"WEBVTT\n")
            .await
            .unwrap();
        tokio::fs::write(td.path().join("show.srt"), b"1\n00:00\n")
            .await
            .unwrap();
        // Decoy file that shouldn't match.
        tokio::fs::write(td.path().join("other.vtt"), b"")
            .await
            .unwrap();
        let found = discover_sidecars(&media).await;
        assert_eq!(found.len(), 3);
        // Sorted lexicographically: show.eng.vtt, show.fra.vtt, show.srt.
        assert!(found[0].0.to_string_lossy().ends_with("show.eng.vtt"));
        assert_eq!(found[0].1, SidecarKind::Vtt);
        assert!(found[2].0.to_string_lossy().ends_with("show.srt"));
        assert_eq!(found[2].1, SidecarKind::Convert);
    }

    #[tokio::test]
    async fn discover_sidecars_finds_ass_and_subfolders() {
        // "Find them no matter what": .ass next to the file, a Subs/ folder,
        // and a per-episode folder — all common anime layouts.
        let td = TempDir::new().unwrap();
        let media = td.path().join("Episode 01.mkv");
        tokio::fs::write(&media, b"x").await.unwrap();
        // (a) same-dir .ass (converts to VTT).
        tokio::fs::write(td.path().join("Episode 01.eng.ass"), b"[Script Info]")
            .await
            .unwrap();
        // (b) Subs/ folder with a per-episode subfolder named by language.
        let subs_ep = td.path().join("Subs").join("Episode 01");
        tokio::fs::create_dir_all(&subs_ep).await.unwrap();
        tokio::fs::write(subs_ep.join("English.srt"), b"1\n")
            .await
            .unwrap();
        // (c) per-episode folder directly beside the media.
        let epdir = td.path().join("Episode 01");
        tokio::fs::create_dir_all(&epdir).await.unwrap();
        tokio::fs::write(epdir.join("eng.ssa"), b"[Script Info]")
            .await
            .unwrap();
        // A .sup image sub must be ignored (can't become WebVTT).
        tokio::fs::write(td.path().join("Episode 01.sup"), b"")
            .await
            .unwrap();

        let found = discover_sidecars(&media).await;
        let names: Vec<String> = found
            .iter()
            .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.contains(&"Episode 01.eng.ass".to_string()),
            "{names:?}"
        );
        assert!(names.contains(&"English.srt".to_string()), "{names:?}");
        assert!(names.contains(&"eng.ssa".to_string()), "{names:?}");
        assert!(
            !names.iter().any(|n| n.ends_with(".sup")),
            "image sub leaked: {names:?}"
        );
        // All three text sidecars, none is passthrough-VTT.
        assert!(found.iter().all(|(_, k)| *k == SidecarKind::Convert));
    }

    #[tokio::test]
    async fn discover_sidecars_requires_dot_after_stem() {
        let td = TempDir::new().unwrap();
        let media = td.path().join("show.mkv");
        tokio::fs::write(&media, b"x").await.unwrap();
        // Not a sidecar — name doesn't separate stem from suffix with `.`.
        tokio::fs::write(td.path().join("shownotsidecar.vtt"), b"")
            .await
            .unwrap();
        assert!(discover_sidecars(&media).await.is_empty());
    }

    #[test]
    fn image_codec_detection_covers_common_names() {
        for c in [
            "hdmv_pgs_subtitle",
            "PGS",
            "pgssub",
            "dvb_subtitle",
            "DVDSub",
            "VobSub",
        ] {
            assert!(is_image_subtitle_codec(c), "{c} should be flagged");
        }
        for c in ["subrip", "srt", "webvtt", "ass", "ssa", "mov_text"] {
            assert!(!is_image_subtitle_codec(c), "{c} should NOT be flagged");
        }
    }

    #[test]
    fn styled_codec_detection_covers_ass_ssa() {
        for c in ["ass", "ASS", "ssa", "advanced substation alpha"] {
            assert!(is_styled_text_subtitle_codec(c), "{c} should be flagged");
        }
        for c in ["subrip", "webvtt", "mov_text", "pgssub"] {
            assert!(
                !is_styled_text_subtitle_codec(c),
                "{c} should NOT be flagged"
            );
        }
    }
}
