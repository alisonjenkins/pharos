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
use actix_web::{error, http::header, web, HttpResponse};
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
    path: web::Path<(String, String, u32)>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id, _media_source_id, stream_index) = path.into_inner();
    deliver_vtt(&state, &id, stream_index).await
}

async fn stream_vtt_short(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<(String, u32)>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id, stream_index) = path.into_inner();
    deliver_vtt(&state, &id, stream_index).await
}

async fn deliver_vtt(
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

    // Sidecar lookups first — they're free (no ffmpeg spawn).
    if stream_index >= SIDECAR_BASE_INDEX {
        let offset = (stream_index - SIDECAR_BASE_INDEX) as usize;
        let sidecars = discover_sidecars(&item.path).await;
        let Some((sidecar_path, kind)) = sidecars.into_iter().nth(offset) else {
            return Err(error::ErrorNotFound("no sidecar at that index"));
        };
        return serve_sidecar(&sidecar_path, kind).await;
    }

    // Embedded stream: ffmpeg -map 0:<idx> -f webvtt.
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
    Ok(HttpResponse::Ok()
        .content_type("text/vtt; charset=utf-8")
        .insert_header((header::CACHE_CONTROL, "public, max-age=3600"))
        .body(out.stdout))
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
    path: &std::path::Path,
    kind: SidecarKind,
) -> Result<HttpResponse, actix_web::Error> {
    match kind {
        SidecarKind::Vtt => {
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
            Ok(HttpResponse::Ok()
                .content_type("text/vtt; charset=utf-8")
                .insert_header((header::CACHE_CONTROL, "public, max-age=3600"))
                .body(out.stdout))
        }
    }
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
}
