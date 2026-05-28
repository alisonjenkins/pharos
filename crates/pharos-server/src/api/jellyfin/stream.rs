//! Direct-play streaming endpoints. Hands off to `actix_files::NamedFile`,
//! which provides byte ranges, content-type sniffing, ETags, and 206
//! Partial Content for free. Transcoded streaming (HLS) lands in T9.
//!
//! V9: the stored `MediaItem.path` is treated as authoritative — its
//! provenance is the scanner-walked media roots (T3). Anything reaching
//! the `MediaStore` from elsewhere must validate root-prefix at the
//! call site; tracked in §B if violated.

use crate::{api::jellyfin::auth_extractor::AuthUser, state::AppState};
use actix_files::NamedFile;
use actix_web::{
    error,
    http::{
        header::{self, HeaderValue},
        StatusCode,
    },
    web, HttpRequest, HttpResponse,
};
use pharos_core::{MediaItem, MediaStore};
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};

/// Jellyfin 100-ns ticks per second.
const TICKS_PER_SECOND: u64 = 10_000_000;

pub fn register(cfg: &mut web::ServiceConfig) {
    // T31: lowercase canonical paths; `LowercasePath` middleware
    // rewrites jellyfin-web's PascalCase before the router matches.
    cfg.route("/videos/{id}/stream", web::get().to(stream_video))
        .route("/videos/{id}/stream.{ext}", web::get().to(stream_video))
        .route("/audio/{id}/stream", web::get().to(stream_audio))
        .route("/audio/{id}/universal", web::get().to(stream_audio));
}

async fn stream_video(
    state: web::Data<AppState>,
    _user: AuthUser,
    req: HttpRequest,
    path: web::Path<StreamPath>,
) -> Result<HttpResponse, actix_web::Error> {
    deliver_stream(&state, &req, path.id_str()).await
}

async fn stream_audio(
    state: web::Data<AppState>,
    _user: AuthUser,
    req: HttpRequest,
    path: web::Path<StreamPath>,
) -> Result<HttpResponse, actix_web::Error> {
    deliver_stream(&state, &req, path.id_str()).await
}

/// P7 — when `StartTimeTicks` query is present AND no Range header
/// supplied, translate ticks → byte offset and respond 206 starting
/// at that byte. Range header wins when both are sent (matches
/// Jellyfin behaviour). All other paths delegate to `NamedFile` so
/// Content-Type / ETag / Last-Modified / regular Range processing
/// keeps working.
async fn deliver_stream(
    state: &AppState,
    req: &HttpRequest,
    id_str: &str,
) -> Result<HttpResponse, actix_web::Error> {
    let item = load_item(state, id_str).await?;
    let has_range = req.headers().contains_key(header::RANGE);
    let start_ticks = parse_start_time_ticks(req.query_string());

    if !has_range && start_ticks > 0 {
        if let Some(offset) = byte_offset_from_ticks(&item, start_ticks).await {
            return serve_from_offset(&item, offset).await;
        }
    }

    let file = NamedFile::open_async(&item.path)
        .await
        .map_err(|e| error::ErrorNotFound(e.to_string()))?
        .use_etag(true)
        .use_last_modified(true);
    Ok(file.into_response(req))
}

fn parse_start_time_ticks(qs: &str) -> u64 {
    for kv in qs.split('&') {
        if let Some((k, v)) = kv.split_once('=') {
            if k.eq_ignore_ascii_case("StartTimeTicks") {
                return v.parse::<u64>().unwrap_or(0);
            }
        }
    }
    0
}

/// Resolve byte offset for the requested tick offset. Prefers
/// bitrate × duration math; falls back to `size × ticks / duration`
/// when only size + duration are available.
async fn byte_offset_from_ticks(item: &MediaItem, start_ticks: u64) -> Option<u64> {
    if start_ticks == 0 {
        return Some(0);
    }
    let probe = &item.probe;
    let duration_ticks = probe.duration_ms.map(|ms| ms.saturating_mul(10_000));

    if let Some(bps) = probe.bitrate_bps {
        // bytes = ticks × bps / (8 × ticks_per_second)
        let bytes = (start_ticks as u128)
            .saturating_mul(bps as u128)
            .saturating_div(8u128 * TICKS_PER_SECOND as u128);
        return Some(bytes.min(u64::MAX as u128) as u64);
    }

    if let (Some(dur), Some(size)) = (duration_ticks.filter(|d| *d > 0), probe.size_bytes) {
        let bytes = (start_ticks as u128)
            .saturating_mul(size as u128)
            .saturating_div(dur as u128);
        return Some(bytes.min(u64::MAX as u128) as u64);
    }

    // Last resort: stat the file ourselves so we can still satisfy a
    // resume request even when the probe lacks size info.
    let dur = duration_ticks.filter(|d| *d > 0)?;
    let meta = tokio::fs::metadata(&item.path).await.ok()?;
    let size = meta.len();
    let bytes = (start_ticks as u128)
        .saturating_mul(size as u128)
        .saturating_div(dur as u128);
    Some(bytes.min(u64::MAX as u128) as u64)
}

async fn serve_from_offset(
    item: &MediaItem,
    offset: u64,
) -> Result<HttpResponse, actix_web::Error> {
    let mut file = tokio::fs::File::open(&item.path)
        .await
        .map_err(|e| error::ErrorNotFound(e.to_string()))?;
    let meta = file
        .metadata()
        .await
        .map_err(|e| error::ErrorInternalServerError(format!("stat: {e}")))?;
    let total = meta.len();
    if offset >= total {
        return Err(error::ErrorRangeNotSatisfiable("StartTimeTicks past EOF"));
    }
    file.seek(SeekFrom::Start(offset))
        .await
        .map_err(|e| error::ErrorInternalServerError(format!("seek: {e}")))?;

    let remaining = total - offset;
    // Pre-buffer in memory for small files; stream chunks otherwise.
    // 16 MiB threshold keeps RSS bounded while letting tests verify
    // first-byte content cheaply.
    let body = if remaining <= 16 * 1024 * 1024 {
        let mut buf = Vec::with_capacity(remaining as usize);
        file.read_to_end(&mut buf)
            .await
            .map_err(|e| error::ErrorInternalServerError(format!("read: {e}")))?;
        actix_web::body::BoxBody::new(buf)
    } else {
        let stream = tokio_util::io::ReaderStream::with_capacity(file, 64 * 1024);
        let stream = futures_util::TryStreamExt::map_err(stream, |e| {
            actix_web::error::ErrorInternalServerError(format!("read: {e}"))
        });
        actix_web::body::BoxBody::new(actix_web::body::BodyStream::new(stream))
    };

    let end = total - 1;
    let mime = mime_guess::from_path(&item.path)
        .first_or_octet_stream()
        .to_string();
    let mut resp = HttpResponse::build(StatusCode::PARTIAL_CONTENT)
        .insert_header((header::CONTENT_TYPE, mime))
        .insert_header((
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {offset}-{end}/{total}"))
                .map_err(error::ErrorInternalServerError)?,
        ))
        .insert_header((header::ACCEPT_RANGES, HeaderValue::from_static("bytes")))
        .insert_header((
            header::CONTENT_LENGTH,
            HeaderValue::from_str(&remaining.to_string())
                .map_err(error::ErrorInternalServerError)?,
        ))
        .body(body);
    // Strip Content-Length on streaming bodies — actix sets
    // transfer-encoding: chunked for those automatically.
    if remaining > 16 * 1024 * 1024 {
        resp.headers_mut().remove(header::CONTENT_LENGTH);
    }
    Ok(resp)
}

async fn load_item(state: &AppState, id_str: &str) -> Result<MediaItem, actix_web::Error> {
    let id: u64 = id_str
        .parse()
        .map_err(|_| error::ErrorBadRequest("invalid id"))?;
    state.stores.get(id).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("not found"),
        other => error::ErrorInternalServerError(other.to_string()),
    })
}

#[derive(serde::Deserialize)]
struct StreamPath {
    id: String,
    #[serde(default)]
    #[allow(dead_code)]
    ext: Option<String>,
}

impl StreamPath {
    fn id_str(&self) -> &str {
        &self.id
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use pharos_core::{MediaItem, MediaKind, MediaProbe};

    fn item_with_bitrate(bitrate_bps: Option<u64>, size_bytes: Option<u64>) -> MediaItem {
        MediaItem {
            id: 1,
            path: "/no/such".into(),
            title: "t".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(60_000), // 60s
                bitrate_bps,
                size_bytes,
                ..Default::default()
            },
            series: None,
            created_at: None,
        }
    }

    #[tokio::test]
    async fn byte_offset_from_ticks_uses_bitrate_when_available() {
        // 1 Mbps source = 125_000 bytes/s.
        // StartTimeTicks = 10_000_000 (1 second).
        let item = item_with_bitrate(Some(1_000_000), None);
        let offset = byte_offset_from_ticks(&item, 10_000_000).await.unwrap();
        assert_eq!(offset, 125_000);
    }

    #[tokio::test]
    async fn byte_offset_from_ticks_falls_back_to_size_over_duration() {
        // duration_ms = 60_000 → 600_000_000 ticks.
        // size = 60_000_000 bytes → 1 MB/s effective.
        // ticks=10_000_000 (1s) → 1_000_000 bytes.
        let item = item_with_bitrate(None, Some(60_000_000));
        let offset = byte_offset_from_ticks(&item, 10_000_000).await.unwrap();
        assert_eq!(offset, 1_000_000);
    }

    #[tokio::test]
    async fn byte_offset_zero_returns_zero() {
        let item = item_with_bitrate(Some(1_000_000), None);
        let offset = byte_offset_from_ticks(&item, 0).await.unwrap();
        assert_eq!(offset, 0);
    }

    #[::core::prelude::v1::test]
    fn parse_start_time_ticks_handles_case_insensitive() {
        assert_eq!(parse_start_time_ticks("StartTimeTicks=12345"), 12345);
        assert_eq!(parse_start_time_ticks("starttimeticks=42"), 42);
        assert_eq!(parse_start_time_ticks("api_key=abc&StartTimeTicks=99"), 99);
        assert_eq!(parse_start_time_ticks(""), 0);
        assert_eq!(parse_start_time_ticks("foo=bar"), 0);
        assert_eq!(parse_start_time_ticks("StartTimeTicks=notanumber"), 0);
    }
}
