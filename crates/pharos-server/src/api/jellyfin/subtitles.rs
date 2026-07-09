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

use crate::{api::jellyfin::auth_extractor::AuthUser, state::AppState};
use actix_web::{error, http::header, web, HttpRequest, HttpResponse};
use pharos_cache::subtitle_cache::{mtime_secs, SubtitleKind};
use pharos_core::MediaStore;
use tokio::process::Command;

/// Sidecar streams get indices starting at this offset so they never
/// collide with ffprobe-reported embedded indices (which top out
/// around 100 even for absurd files).
// SIDECAR_BASE_INDEX moved to `pharos_jellyfin_api::dto` in Phase A.2.
// Re-exported here so `crate::api::jellyfin::subtitles::SIDECAR_BASE_INDEX`
// still resolves for any historical caller.
pub use pharos_jellyfin_api::dto::SIDECAR_BASE_INDEX;

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
    );
}

async fn stream_ass(
    state: web::Data<AppState>,
    path: web::Path<(String, String, u32)>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id, _media_source_id, stream_index) = path.into_inner();
    deliver_ass(&state, &id, stream_index).await
}

async fn stream_ass_short(
    state: web::Data<AppState>,
    path: web::Path<(String, u32)>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id, stream_index) = path.into_inner();
    deliver_ass(&state, &id, stream_index).await
}

/// Serve a subtitle track as RAW ASS/SSA (for SubtitlesOctopus). Sidecars pass
/// through verbatim; embedded streams are extracted with `-c:s ass -f ass` and
/// cached (distinct `EmbeddedAss` key from the VTT form).
async fn deliver_ass(
    state: &AppState,
    id_str: &str,
    stream_index: u32,
) -> Result<HttpResponse, actix_web::Error> {
    let id: u64 = id_str
        .parse()
        .map_err(|_| error::ErrorBadRequest("invalid id"))?;
    let item = state.stores.get(id).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("not found"),
        other => error::ErrorInternalServerError(other.to_string()),
    })?;

    let ass_body = |bytes: Vec<u8>| {
        HttpResponse::Ok()
            .content_type("text/x-ssa; charset=utf-8")
            .insert_header((header::CACHE_CONTROL, "public, max-age=3600"))
            .body(bytes)
    };

    // Sidecar → raw passthrough (it's already an .ass/.ssa file on disk).
    if stream_index >= SIDECAR_BASE_INDEX {
        let offset = (stream_index - SIDECAR_BASE_INDEX) as usize;
        let sidecars = discover_sidecars(&item.path).await;
        let Some((sidecar_path, _)) = sidecars.into_iter().nth(offset) else {
            return Err(error::ErrorNotFound("no sidecar at that index"));
        };
        let bytes = tokio::fs::read(&sidecar_path)
            .await
            .map_err(|e| error::ErrorInternalServerError(format!("read: {e}")))?;
        return Ok(ass_body(bytes));
    }

    let input = item
        .path
        .to_str()
        .ok_or_else(|| error::ErrorInternalServerError("non-utf8 path"))?;
    let mtime = mtime_secs(&item.path).await;
    if let Some(cache) = state.subtitles.as_ref() {
        if let Some(bytes) = cache
            .get(&item.path, mtime, stream_index, SubtitleKind::EmbeddedAss)
            .await
        {
            return Ok(ass_body((*bytes).clone()));
        }
        let lock = cache
            .lock(&item.path, mtime, stream_index, SubtitleKind::EmbeddedAss)
            .await;
        let _guard = lock.lock().await;
        if let Some(bytes) = cache
            .get(&item.path, mtime, stream_index, SubtitleKind::EmbeddedAss)
            .await
        {
            return Ok(ass_body((*bytes).clone()));
        }
        let out = run_ffmpeg_embedded_ass(input, stream_index).await?;
        let stored = cache
            .store(
                &item.path,
                mtime,
                stream_index,
                SubtitleKind::EmbeddedAss,
                out,
            )
            .await;
        return Ok(ass_body((*stored).clone()));
    }
    let out = run_ffmpeg_embedded_ass(input, stream_index).await?;
    Ok(ass_body(out))
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
) {
    let Some(input) = item.path.to_str() else {
        return;
    };
    let mtime = mtime_secs(&item.path).await;
    for t in &item.probe.subtitle_tracks {
        let codec_lc = t.codec.as_deref().unwrap_or("").to_ascii_lowercase();
        if is_image_subtitle_codec(&codec_lc) {
            continue; // burned into the transcode, never a sidecar file
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
        // Map the actix `Error` (not `Send`) to a `String` before the
        // `store().await` below, so this future stays `Send` for `tokio::spawn`.
        let extracted = if is_ass {
            run_ffmpeg_embedded_ass(input, t.stream_index).await
        } else {
            run_ffmpeg_embedded(input, t.stream_index).await
        }
        .map_err(|e| e.to_string());
        match extracted {
            Ok(bytes) => {
                cache
                    .store(&item.path, mtime, t.stream_index, kind, bytes)
                    .await;
            }
            Err(e) => {
                tracing::warn!(error = %e, media.id = item.id, idx = t.stream_index, "subtitle pre-extract failed");
            }
        }
    }
}

/// Extract one embedded subtitle stream verbatim as ASS.
async fn run_ffmpeg_embedded_ass(
    input: &str,
    stream_index: u32,
) -> Result<Vec<u8>, actix_web::Error> {
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
    Ok(out.stdout)
}

async fn stream_srt(
    state: web::Data<AppState>,
    req: HttpRequest,
    path: web::Path<(String, String, u32)>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id, _media_source_id, stream_index) = path.into_inner();
    let forced_only = parse_forced_only(req.query_string());
    deliver_srt(&state, &id, stream_index, forced_only).await
}

async fn stream_srt_short(
    state: web::Data<AppState>,
    req: HttpRequest,
    path: web::Path<(String, u32)>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id, stream_index) = path.into_inner();
    let forced_only = parse_forced_only(req.query_string());
    deliver_srt(&state, &id, stream_index, forced_only).await
}

/// P40 — SRT-form delivery. ffmpeg converts the embedded stream to
/// `-c:s subrip -f srt`. Hits the same image-codec refusal as the
/// VTT path so PGS/DVB return 415 instead of empty bodies.
async fn deliver_srt(
    state: &AppState,
    id_str: &str,
    stream_index: u32,
    forced_only: bool,
) -> Result<HttpResponse, actix_web::Error> {
    let id: u64 = id_str
        .parse()
        .map_err(|_| error::ErrorBadRequest("invalid id"))?;
    let item = state.stores.get(id).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("not found"),
        other => error::ErrorInternalServerError(other.to_string()),
    })?;
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
    Ok(HttpResponse::Ok()
        .content_type("application/x-subrip; charset=utf-8")
        .insert_header((header::CACHE_CONTROL, "public, max-age=3600"))
        .body(out.stdout))
}

async fn stream_vtt(
    state: web::Data<AppState>,
    user: Option<AuthUser>,
    req: HttpRequest,
    path: web::Path<(String, String, u32)>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id, _media_source_id, stream_index) = path.into_inner();
    let forced_only = parse_forced_only(req.query_string());
    let style = user_subtitle_style(&state, user).await;
    deliver_vtt(&state, &id, stream_index, forced_only, style).await
}

async fn stream_vtt_short(
    state: web::Data<AppState>,
    user: Option<AuthUser>,
    req: HttpRequest,
    path: web::Path<(String, u32)>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id, stream_index) = path.into_inner();
    let forced_only = parse_forced_only(req.query_string());
    let style = user_subtitle_style(&state, user).await;
    deliver_vtt(&state, &id, stream_index, forced_only, style).await
}

/// Per-user subtitle style when the request is authenticated; default styling
/// otherwise (subtitle routes are public — jellyfin-web's JS renderer fetches
/// them without a token, like the image routes).
async fn user_subtitle_style(state: &AppState, user: Option<AuthUser>) -> SubtitleStyle {
    match user {
        Some(u) => subtitle_style_for(state, u.0.id).await,
        None => SubtitleStyle::default(),
    }
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

async fn deliver_vtt(
    state: &AppState,
    id_str: &str,
    stream_index: u32,
    forced_only: bool,
    style: SubtitleStyle,
) -> Result<HttpResponse, actix_web::Error> {
    let id: u64 = id_str
        .parse()
        .map_err(|_| error::ErrorBadRequest("invalid id"))?;
    let item = state.stores.get(id).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("not found"),
        other => error::ErrorInternalServerError(other.to_string()),
    })?;

    // Sidecar lookups first — they're free (no ffmpeg spawn).
    if stream_index >= SIDECAR_BASE_INDEX {
        let offset = (stream_index - SIDECAR_BASE_INDEX) as usize;
        let sidecars = discover_sidecars(&item.path).await;
        let Some((sidecar_path, kind)) = sidecars.into_iter().nth(offset) else {
            return Err(error::ErrorNotFound("no sidecar at that index"));
        };
        return serve_sidecar(state, &sidecar_path, kind, stream_index).await;
    }

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
        let out = run_ffmpeg_embedded(input, stream_index).await?;
        let stored = cache
            .store(&item.path, mtime, stream_index, SubtitleKind::Embedded, out)
            .await;
        return Ok(vtt_response((*stored).clone(), style_lossy, &style));
    }

    // No cache configured — fall back to the original spawn-per-fetch
    // path. (Default config keeps the cache on; this branch only
    // fires for tests / minimal deployments.)
    let out = run_ffmpeg_embedded(input, stream_index).await?;
    Ok(vtt_response(out, style_lossy, &style))
}

async fn stream_js(
    state: web::Data<AppState>,
    path: web::Path<(String, String, u32)>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id, _msid, stream_index) = path.into_inner();
    deliver_js(&state, &id, stream_index).await
}

async fn stream_js_short(
    state: web::Data<AppState>,
    path: web::Path<(String, u32)>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id, stream_index) = path.into_inner();
    deliver_js(&state, &id, stream_index).await
}

async fn stream_js_ticks(
    state: web::Data<AppState>,
    path: web::Path<(String, String, u32, i64)>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id, _msid, stream_index, _start_ticks) = path.into_inner();
    deliver_js(&state, &id, stream_index).await
}

/// Serve a text subtitle track as jellyfin-web's JSON cue format
/// (`Stream.js`). The client renders these events itself. Reuses the same
/// WebVTT extraction as `Stream.vtt` (sidecar or embedded, cached) then
/// converts the cues to the `{ "TrackEvents": [...] }` shape jellyfin
/// expects. Image-codec tracks are rejected (they burn, never render here).
async fn deliver_js(
    state: &AppState,
    id_str: &str,
    stream_index: u32,
) -> Result<HttpResponse, actix_web::Error> {
    let id: u64 = id_str
        .parse()
        .map_err(|_| error::ErrorBadRequest("invalid id"))?;
    let item = state.stores.get(id).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("not found"),
        other => error::ErrorInternalServerError(other.to_string()),
    })?;

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
        .insert_header((header::CACHE_CONTROL, "public, max-age=3600"))
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
    if stream_index >= SIDECAR_BASE_INDEX {
        let offset = (stream_index - SIDECAR_BASE_INDEX) as usize;
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

    let input = item
        .path
        .to_str()
        .ok_or_else(|| error::ErrorInternalServerError("non-utf8 path"))?;
    let mtime = mtime_secs(&item.path).await;
    if let Some(cache) = state.subtitles.as_ref() {
        if let Some(bytes) = cache
            .get(&item.path, mtime, stream_index, SubtitleKind::Embedded)
            .await
        {
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
            return Ok((*bytes).clone());
        }
        let out = run_ffmpeg_embedded(input, stream_index).await?;
        let stored = cache
            .store(&item.path, mtime, stream_index, SubtitleKind::Embedded, out)
            .await;
        return Ok((*stored).clone());
    }
    run_ffmpeg_embedded(input, stream_index).await
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
        events.push(serde_json::json!({
            "Id": events.len().to_string(),
            "Text": text,
            "StartPositionTicks": start,
            "EndPositionTicks": end,
        }));
    }
    serde_json::json!({ "TrackEvents": events }).to_string()
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
        .insert_header((header::CACHE_CONTROL, "public, max-age=3600"));
    if style_lossy {
        builder.insert_header(("X-Subtitle-Style-Lossy", "true"));
    }
    builder.body(body)
}

async fn run_ffmpeg_embedded(input: &str, stream_index: u32) -> Result<Vec<u8>, actix_web::Error> {
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

/// P15 — known image-based subtitle codecs. ffmpeg can't convert
/// these to text-WebVTT (they're rasters); attempts produce empty
/// or malformed output. Refused with 415 so the client doesn't
/// mistake an empty track for a working one.
fn is_image_subtitle_codec(codec: &str) -> bool {
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
type SidecarCache = std::collections::HashMap<std::path::PathBuf, (u64, SidecarList)>;
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
    let key = media_path.to_path_buf();
    if let Ok(cache) = SIDECAR_CACHE.lock() {
        if let Some((cached_mtime, subs)) = cache.get(&key) {
            if *cached_mtime == mtime {
                return subs.clone();
            }
        }
    }
    let result = discover_sidecars_uncached(media_path).await;
    if let Ok(mut cache) = SIDECAR_CACHE.lock() {
        cache.insert(key, (mtime, result.clone()));
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
                .insert_header((header::CACHE_CONTROL, "public, max-age=3600"))
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
                        .insert_header((header::CACHE_CONTROL, "public, max-age=3600"))
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
                        .insert_header((header::CACHE_CONTROL, "public, max-age=3600"))
                        .body((*bytes).clone()));
                }
                let out = run_ffmpeg_srt_to_vtt(input).await?;
                let stored = cache
                    .store(path, mtime, stream_index, SubtitleKind::Sidecar, out)
                    .await;
                return Ok(HttpResponse::Ok()
                    .content_type("text/vtt; charset=utf-8")
                    .insert_header((header::CACHE_CONTROL, "public, max-age=3600"))
                    .body((*stored).clone()));
            }
            let out = run_ffmpeg_srt_to_vtt(input).await?;
            Ok(HttpResponse::Ok()
                .content_type("text/vtt; charset=utf-8")
                .insert_header((header::CACHE_CONTROL, "public, max-age=3600"))
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
