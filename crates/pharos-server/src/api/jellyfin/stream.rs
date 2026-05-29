//! Direct-play streaming endpoints. Hands off to `actix_files::NamedFile`,
//! which provides byte ranges, content-type sniffing, ETags, and 206
//! Partial Content for free. Transcoded streaming (HLS) lands in T9.
//!
//! V9: the stored `MediaItem.path` is treated as authoritative — its
//! provenance is the scanner-walked media roots (T3). Anything reaching
//! the `MediaStore` from elsewhere must validate root-prefix at the
//! call site; tracked in §B if violated.

use crate::{
    api::jellyfin::auth_extractor::{auth_cookie_header, AuthUser},
    state::AppState,
};
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
        .route("/videos/{id}/stream", web::head().to(head_video))
        .route("/audio/{id}/stream", web::get().to(stream_audio))
        .route("/audio/{id}/stream", web::head().to(head_audio))
        // P11 — universal honours AudioCodec + MaxStreamingBitrate.
        .route("/audio/{id}/universal", web::get().to(audio_universal))
        .route("/audio/{id}/universal", web::head().to(head_audio));
}

async fn head_video(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<StreamPath>,
) -> Result<HttpResponse, actix_web::Error> {
    head_response(&state, path.id_str()).await
}

async fn head_audio(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<StreamPath>,
) -> Result<HttpResponse, actix_web::Error> {
    head_response(&state, path.id_str()).await
}

/// P11 — HEAD short-circuit. Returns Content-Length + Content-Type
/// from the probe / stat without opening the body. Mobile clients
/// use HEAD to validate a stream URL before issuing the playback
/// GET; without this they fall back to GET-then-cancel. P25 — also
/// emits `Last-Modified` so a phone re-opening the player can
/// conditional-GET the range cache instead of re-downloading.
async fn head_response(state: &AppState, id_str: &str) -> Result<HttpResponse, actix_web::Error> {
    let item = load_item(state, id_str).await?;
    let meta = tokio::fs::metadata(&item.path).await.ok();
    let size = item
        .probe
        .size_bytes
        .or_else(|| meta.as_ref().map(|m| m.len()))
        .unwrap_or(0);
    let mime = mime_guess::from_path(&item.path)
        .first_or_octet_stream()
        .to_string();
    let mut builder = HttpResponse::Ok();
    builder
        .insert_header((header::CONTENT_TYPE, mime))
        .insert_header((header::ACCEPT_RANGES, HeaderValue::from_static("bytes")))
        .insert_header((
            header::CONTENT_LENGTH,
            HeaderValue::from_str(&size.to_string()).map_err(error::ErrorInternalServerError)?,
        ));
    if let Some(lm) = last_modified_from_meta(meta.as_ref()) {
        builder.insert_header((header::LAST_MODIFIED, lm.as_str()));
    }
    Ok(builder.finish())
}

/// P25 — `Last-Modified` header formatting from a `Metadata`.
fn last_modified_from_meta(meta: Option<&std::fs::Metadata>) -> Option<String> {
    let m = meta?.modified().ok()?;
    Some(httpdate::fmt_http_date(m))
}

/// P25 — parse the `If-Modified-Since` header and decide if the
/// caller's snapshot is still current.
fn not_modified(req: &HttpRequest, file_modified: std::time::SystemTime) -> bool {
    let Some(ims) = req
        .headers()
        .get(header::IF_MODIFIED_SINCE)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    httpdate::parse_http_date(ims)
        .map(|since| {
            // HTTP-date has 1-second resolution; treat anything earlier
            // than or equal to the cache snapshot as "still current".
            file_modified <= since
        })
        .unwrap_or(false)
}

/// P11 — `/Audio/{id}/universal`. Parses `AudioCodec` (CSV of
/// acceptable codecs) + `MaxStreamingBitrate` and either streams the
/// source directly (when its codec is acceptable) or remuxes via
/// ffmpeg to the first acceptable target (typically AAC).
async fn audio_universal(
    state: web::Data<AppState>,
    _user: AuthUser,
    req: HttpRequest,
    path: web::Path<StreamPath>,
) -> Result<HttpResponse, actix_web::Error> {
    let item = load_item(&state, path.id_str()).await?;
    let qs = req.query_string();
    let acceptable = parse_audio_codec_list(qs);
    let bitrate = parse_max_streaming_bitrate(qs);
    let max_channels = parse_max_audio_channels(qs);
    let source_codec = item.probe.audio_codec.as_deref().unwrap_or("");
    let source_channels = item.probe.audio_channels.unwrap_or(0);

    // P19 — when source channels exceed the cap, force a remux even
    // when the codec matches (Direct path can't downmix). Downmix
    // target is AAC at the supplied codec list's first acceptable
    // hit, or AAC by default.
    let needs_downmix =
        max_channels.is_some_and(|cap| source_channels > 0 && source_channels > cap);

    if !needs_downmix
        && (acceptable.is_empty()
            || acceptable
                .iter()
                .any(|c| c.eq_ignore_ascii_case(source_codec)))
    {
        // Direct path — defer to the existing delivery (StartTimeTicks
        // + Range honoured by `deliver_stream`).
        return deliver_stream(&state, &req, path.id_str()).await;
    }

    // Remux. Pick the first acceptable target the server knows how to
    // emit. AAC is the lowest-common-denominator and always present
    // in modern ffmpeg.
    let target = acceptable
        .iter()
        .find(|c| matches!(c.to_ascii_lowercase().as_str(), "aac"))
        .cloned()
        .unwrap_or_else(|| "aac".to_string());
    audio_remux(&item, &target, bitrate, max_channels).await
}

async fn audio_remux(
    item: &MediaItem,
    target_codec: &str,
    bitrate_bps: Option<u64>,
    max_channels: Option<u32>,
) -> Result<HttpResponse, actix_web::Error> {
    use std::process::Stdio;
    use tokio::process::Command;

    let codec = target_codec.to_ascii_lowercase();
    let (ffmpeg_codec, muxer, content_type) = match codec.as_str() {
        "aac" => ("aac", "adts", "audio/aac"),
        "mp3" => ("libmp3lame", "mp3", "audio/mpeg"),
        "opus" => ("libopus", "ogg", "audio/ogg"),
        other => {
            return Err(error::ErrorBadRequest(format!(
                "unsupported audio remux target: {other}"
            )));
        }
    };
    let bitrate = bitrate_bps.unwrap_or(192_000);

    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-nostdin")
        .arg("-i")
        .arg(&item.path)
        .arg("-vn")
        .arg("-c:a")
        .arg(ffmpeg_codec)
        .arg("-b:a")
        .arg(bitrate.to_string());
    // P19 — downmix to the requested channel count when the client
    // asked for one. ffmpeg's `-ac N` runs a default mix-down for
    // surround → stereo / mono.
    if let Some(n) = max_channels.filter(|n| *n > 0) {
        cmd.arg("-ac").arg(n.to_string());
    }
    cmd.arg("-f")
        .arg(muxer)
        .arg("pipe:1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let mut child = cmd
        .spawn()
        .map_err(|e| error::ErrorInternalServerError(format!("ffmpeg spawn: {e}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| error::ErrorInternalServerError("ffmpeg stdout missing"))?;
    let reader = tokio_util::io::ReaderStream::with_capacity(stdout, 64 * 1024);
    let stream = futures_util::TryStreamExt::map_err(reader, |e| {
        actix_web::error::ErrorInternalServerError(format!("read: {e}"))
    });
    // Spawn a watcher so the child gets reaped even when the client
    // disconnects mid-stream. V6 invariant: child Drop kills it; but
    // explicit await keeps zombies off PIDs.
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(HttpResponse::Ok()
        .content_type(content_type)
        .body(actix_web::body::BodyStream::new(stream)))
}

fn parse_audio_codec_list(qs: &str) -> Vec<String> {
    for kv in qs.split('&') {
        if let Some((k, v)) = kv.split_once('=') {
            if k.eq_ignore_ascii_case("AudioCodec") && !v.is_empty() {
                return v
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
        }
    }
    Vec::new()
}

fn parse_max_streaming_bitrate(qs: &str) -> Option<u64> {
    for kv in qs.split('&') {
        if let Some((k, v)) = kv.split_once('=') {
            if k.eq_ignore_ascii_case("MaxStreamingBitrate") {
                return v.parse::<u64>().ok();
            }
        }
    }
    None
}

/// P24 — extract the `api_key` (or `ApiKey`) query value so the
/// stream / audio handlers can echo it back as a JellyfinAuth cookie
/// on the response. Returns None when the auth source was a header
/// instead — no need to set a cookie when the client could already
/// inject one.
fn api_key_query_value(qs: &str) -> Option<String> {
    for kv in qs.split('&') {
        if let Some((k, v)) = kv.split_once('=') {
            if (k.eq_ignore_ascii_case("api_key") || k.eq_ignore_ascii_case("ApiKey"))
                && !v.is_empty()
            {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn parse_max_audio_channels(qs: &str) -> Option<u32> {
    for kv in qs.split('&') {
        if let Some((k, v)) = kv.split_once('=') {
            if k.eq_ignore_ascii_case("MaxAudioChannels") {
                return v.parse::<u32>().ok();
            }
        }
    }
    None
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
            return serve_from_offset(&item, offset, req).await;
        }
    }

    let file = NamedFile::open_async(&item.path)
        .await
        .map_err(|e| error::ErrorNotFound(e.to_string()))?
        .use_etag(true)
        .use_last_modified(true);
    let mut resp = file.into_response(req);
    // P24 — echo the auth as a cookie so a follow-up `<video>`-style
    // fetch can drop the `?api_key=` and still authenticate.
    if let Some(token) = api_key_query_value(req.query_string()) {
        if let Ok(hv) = HeaderValue::from_str(&auth_cookie_header(&token)) {
            resp.headers_mut().insert(header::SET_COOKIE, hv);
        }
    }
    Ok(resp)
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
    req: &HttpRequest,
) -> Result<HttpResponse, actix_web::Error> {
    // P25 — conditional GET. When the client's cached snapshot is
    // still current per `If-Modified-Since`, short-circuit with 304.
    let meta_for_lm = tokio::fs::metadata(&item.path).await.ok();
    if let Some(modified) = meta_for_lm.as_ref().and_then(|m| m.modified().ok()) {
        if not_modified(req, modified) {
            let mut resp = HttpResponse::NotModified();
            if let Some(lm) = last_modified_from_meta(meta_for_lm.as_ref()) {
                resp.insert_header((header::LAST_MODIFIED, lm.as_str()));
            }
            return Ok(resp.finish());
        }
    }

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
    let mut resp_builder = HttpResponse::build(StatusCode::PARTIAL_CONTENT);
    resp_builder
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
        ));
    if let Some(lm) = last_modified_from_meta(meta_for_lm.as_ref()) {
        resp_builder.insert_header((header::LAST_MODIFIED, lm.as_str()));
    }
    let mut resp = resp_builder.body(body);
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

    #[::core::prelude::v1::test]
    fn parse_audio_codec_list_csv() {
        assert_eq!(
            parse_audio_codec_list("AudioCodec=aac,mp3,opus"),
            vec!["aac", "mp3", "opus"]
        );
        assert_eq!(parse_audio_codec_list("audiocodec=aac"), vec!["aac"]);
        assert!(parse_audio_codec_list("").is_empty());
        assert!(parse_audio_codec_list("foo=bar").is_empty());
        // Whitespace-trim + drop empty entries.
        assert_eq!(
            parse_audio_codec_list("AudioCodec= aac , , mp3 "),
            vec!["aac", "mp3"]
        );
    }

    #[::core::prelude::v1::test]
    fn parse_max_audio_channels_extracts_numeric_value() {
        assert_eq!(parse_max_audio_channels("MaxAudioChannels=2"), Some(2));
        assert_eq!(parse_max_audio_channels("maxaudiochannels=6"), Some(6));
        assert_eq!(parse_max_audio_channels(""), None);
        assert_eq!(parse_max_audio_channels("MaxAudioChannels=abc"), None);
    }

    #[::core::prelude::v1::test]
    fn parse_max_streaming_bitrate_extracts_numeric_value() {
        assert_eq!(
            parse_max_streaming_bitrate("MaxStreamingBitrate=128000"),
            Some(128_000)
        );
        assert_eq!(
            parse_max_streaming_bitrate("maxstreamingbitrate=1500000"),
            Some(1_500_000)
        );
        assert_eq!(parse_max_streaming_bitrate(""), None);
        assert_eq!(parse_max_streaming_bitrate("MaxStreamingBitrate=abc"), None);
    }
}
