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

use crate::{
    api::jellyfin::auth_extractor::AuthUser,
    state::AppState,
    subtitle_cache::{mtime_secs, SubtitleKind},
};
use actix_web::{error, http::header, web, HttpRequest, HttpResponse};
use pharos_core::MediaStore;
use tokio::process::Command;

/// Sidecar streams get indices starting at this offset so they never
/// collide with ffprobe-reported embedded indices (which top out
/// around 100 even for absurd files).
pub const SIDECAR_BASE_INDEX: u32 = 1_000_000;

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
    );
}

async fn stream_vtt(
    state: web::Data<AppState>,
    _user: AuthUser,
    req: HttpRequest,
    path: web::Path<(String, String, u32)>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id, _media_source_id, stream_index) = path.into_inner();
    let forced_only = parse_forced_only(req.query_string());
    deliver_vtt(&state, &id, stream_index, forced_only).await
}

async fn stream_vtt_short(
    state: web::Data<AppState>,
    _user: AuthUser,
    req: HttpRequest,
    path: web::Path<(String, u32)>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id, stream_index) = path.into_inner();
    let forced_only = parse_forced_only(req.query_string());
    deliver_vtt(&state, &id, stream_index, forced_only).await
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
            return Ok(vtt_response((*bytes).clone(), style_lossy));
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
            return Ok(vtt_response((*bytes).clone(), style_lossy));
        }
        let out = run_ffmpeg_embedded(input, stream_index).await?;
        let stored = cache
            .store(&item.path, mtime, stream_index, SubtitleKind::Embedded, out)
            .await;
        return Ok(vtt_response((*stored).clone(), style_lossy));
    }

    // No cache configured — fall back to the original spawn-per-fetch
    // path. (Default config keeps the cache on; this branch only
    // fires for tests / minimal deployments.)
    let out = run_ffmpeg_embedded(input, stream_index).await?;
    Ok(vtt_response(out, style_lossy))
}

/// P26 — shared WebVTT response builder. Adds the
/// `X-Subtitle-Style-Lossy` header + an in-body WEBVTT NOTE comment
/// when the source codec carried styling that didn't survive the
/// conversion (ASS/SSA).
fn vtt_response(mut body: Vec<u8>, style_lossy: bool) -> HttpResponse {
    if style_lossy {
        // Prepend the NOTE before the existing WEBVTT magic so the
        // WebVTT parser keeps treating the file as valid.
        let mut prefixed = Vec::with_capacity(body.len() + 128);
        prefixed.extend_from_slice(
            b"WEBVTT\nNOTE Source format was ASS/SSA; styling lost in WebVTT conversion.\n\n",
        );
        // Strip an existing WEBVTT magic line so we don't end up with
        // two of them.
        if body.starts_with(b"WEBVTT") {
            if let Some(nl) = body.iter().position(|c| *c == b'\n') {
                body = body[nl + 1..].to_vec();
            }
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

/// Sidecar kind — `.vtt` ships as-is, `.srt` runs through ffmpeg to
/// WebVTT (browsers consume `<track>` as WebVTT only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidecarKind {
    Vtt,
    Srt,
}

/// Discover sidecar subtitle files alongside `media_path`.
///
/// Returns `(sidecar_path, kind)` pairs in a stable order so the
/// numeric `stream_index` offsets stay consistent between PlaybackInfo
/// and Stream.vtt fetches. Order: ascending file name.
pub async fn discover_sidecars(
    media_path: &std::path::Path,
) -> Vec<(std::path::PathBuf, SidecarKind)> {
    let Some(parent) = media_path.parent() else {
        return Vec::new();
    };
    let Some(stem) = media_path.file_stem().and_then(|s| s.to_str()) else {
        return Vec::new();
    };
    let mut found: Vec<(std::path::PathBuf, SidecarKind)> = Vec::new();
    let Ok(mut entries) = tokio::fs::read_dir(parent).await else {
        return Vec::new();
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let p = entry.path();
        let Some(name) = p.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let lower = name.to_ascii_lowercase();
        let stem_lower = stem.to_ascii_lowercase();
        // Accept `<stem>.vtt`, `<stem>.<lang>.vtt`, plus same for srt.
        if !lower.starts_with(&stem_lower) {
            continue;
        }
        // Ensure the next char after stem is a separator (`.`) so we
        // don't match `<stem>extra.vtt`.
        let rest = &lower[stem_lower.len()..];
        if !rest.starts_with('.') {
            continue;
        }
        let kind = if lower.ends_with(".vtt") {
            SidecarKind::Vtt
        } else if lower.ends_with(".srt") {
            SidecarKind::Srt
        } else {
            continue;
        };
        found.push((p, kind));
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
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
        SidecarKind::Srt => {
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
        assert_eq!(found[2].1, SidecarKind::Srt);
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
