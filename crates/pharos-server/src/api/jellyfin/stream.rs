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
use pharos_core::{MediaItem, MediaStore, TokenStore};
use pharos_transcode::{AudioCodec, Container, FfmpegTranscoder, TranscodeOptions, VideoCodec};
use std::pin::Pin;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};

use pharos_core::time::{Ticks, TICKS_PER_SECOND};

/// Wraps a response body so the shared `playback_activity` clock is restamped
/// as bytes ACTUALLY flow to the client (V35). A single long GET — direct-play
/// `stream.mp4`, resume-from-offset, progressive webm, or an audio remux — thus
/// keeps the `bg_io` regulator parked for the WHOLE stream, not just the 12s
/// window after the request line (all a once-per-request stamp bought). B72: the
/// regulator was blind to every non-webm delivery path, so background sweeps ran
/// at full `BG_IO_MAX` during direct playback and starved live reads.
struct MeteredBody<B> {
    inner: B,
    clock: Arc<AtomicI64>,
}

impl<B: actix_web::body::MessageBody + Unpin> actix_web::body::MessageBody for MeteredBody<B> {
    type Error = B::Error;

    fn size(&self) -> actix_web::body::BodySize {
        self.inner.size()
    }

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<actix_web::web::Bytes, Self::Error>>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.inner).poll_next(cx);
        if let Poll::Ready(Some(Ok(_))) = &polled {
            // Bytes just went out — mark playback live NOW. Cheap relaxed store.
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            this.clock.store(now, Ordering::Relaxed);
        }
        polled
    }
}

/// Route a delivery response's body through [`MeteredBody`] so the playback
/// clock keeps ticking for the stream's whole lifetime (V35). Every direct-play
/// / resume / progressive / audio delivery return value passes through here.
fn meter_body(resp: HttpResponse, clock: Arc<AtomicI64>) -> HttpResponse {
    resp.map_body(|_, body| {
        actix_web::body::BoxBody::new(MeteredBody {
            inner: body,
            clock: clock.clone(),
        })
    })
}

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
    req: HttpRequest,
    path: web::Path<StreamPath>,
) -> Result<HttpResponse, actix_web::Error> {
    let media_id = pharos_jellyfin_api::dto::parse_item_id(path.id_str())
        .ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
    authorize_media(&state, &req, media_id).await?;
    head_response(&state, path.id_str()).await
}

async fn head_audio(
    state: web::Data<AppState>,
    req: HttpRequest,
    path: web::Path<StreamPath>,
) -> Result<HttpResponse, actix_web::Error> {
    // B86 — native direct-play (Android TV / ExoPlayer) fetches the audio URL
    // raw with the MediaSource ETag forwarded as `?tag=`, NOT a bearer header.
    // Authorize via that capability (like stream_video/B75) instead of the
    // strict AuthUser extractor, which 401'd every music DirectPlay so nothing
    // played.
    let media_id = pharos_jellyfin_api::dto::parse_item_id(path.id_str())
        .ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
    authorize_media(&state, &req, media_id).await?;
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
    // Audio playback is live too — stamp on entry, meter the body below (V35).
    state.note_playback_activity();
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
    audio_remux(
        &item,
        &target,
        bitrate,
        max_channels,
        state.playback_activity.clone(),
    )
    .await
}

async fn audio_remux(
    item: &MediaItem,
    target_codec: &str,
    bitrate_bps: Option<u64>,
    max_channels: Option<u32>,
    clock: Arc<AtomicI64>,
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
    Ok(meter_body(
        HttpResponse::Ok()
            .content_type(content_type)
            .body(actix_web::body::BodyStream::new(stream)),
        clock,
    ))
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

/// First value of query key `key` (case-insensitive), empty values skipped.
fn query_value_ci(qs: &str, key: &str) -> Option<String> {
    for kv in qs.split('&') {
        if let Some((k, v)) = kv.split_once('=') {
            if k.eq_ignore_ascii_case(key) && !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// B75 — authorize a direct-play `/videos/{id}/stream` request. Two accepted
/// credentials:
///
/// 1. A normal token (Emby/`X-Emby-Token` header, `api_key` query, or the
///    JellyfinAuth cookie) — the browser path (jellyfin-web) and any client
///    with an auth interceptor.
/// 2. A **capability token** the native Jellyfin apps forward. jellyfin-android-tv
///    (and the mobile SDK) build the direct-play URL themselves and send NO
///    credential at all — no header, no cookie, no `api_key` (their ExoPlayer
///    OkHttp data-source has no auth interceptor; confirmed by B72 + reading
///    the SDK). Real Jellyfin only survives this because its stream route is
///    anonymous (item ids are random GUIDs). pharos ids are low-entropy, so an
///    anonymous stream route would be enumerable. Instead we bind auth to the
///    ONE server-controlled value the app always echoes back: the MediaSource
///    `ETag`, which the SDK passes verbatim as `?tag=` (`getVideoStreamUrl(tag =
///    mediaSource.eTag)`). `playback_info` stamps `ETag = PlaySessionId` — a
///    random uuid registered against this media id in the session registry, and
///    ONLY handed out in an authenticated PlaybackInfo response. A `tag` (or
///    `PlaySessionId`) whose registered session is bound to THIS media id
///    authorizes the stream; the token is unguessable, single-item-scoped, and
///    time-limited — strictly tighter than upstream's anonymous-by-GUID.
async fn authorize_media(
    state: &AppState,
    req: &HttpRequest,
    media_id: pharos_core::MediaId,
) -> Result<(), actix_web::Error> {
    // 1. Normal credential.
    if let Some(token) = crate::api::jellyfin::auth_extractor::extract_token(req) {
        if state.stores.resolve(&token).await.is_ok() {
            return Ok(());
        }
    }
    // 2. Capability token forwarded by a native app (tag == our ETag).
    let qs = req.query_string();
    for key in ["tag", "PlaySessionId"] {
        if let Some(cap) = query_value_ci(qs, key) {
            if let Ok(Some(session)) = state.transcode_sessions.get(&cap).await {
                if session.media_id == media_id {
                    return Ok(());
                }
            }
        }
    }
    Err(error::ErrorUnauthorized("missing token"))
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
    req: HttpRequest,
    path: web::Path<StreamPath>,
) -> Result<HttpResponse, actix_web::Error> {
    let media_id = pharos_jellyfin_api::dto::parse_item_id(path.id_str())
        .ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
    authorize_media(&state, &req, media_id).await?;
    // A `.webm` extension WITHOUT `Static=true` is a progressive transcode
    // request. jellyfin-web routes browsers whose MSE can't decode H.264
    // (e.g. some Firefox/Zen builds) here, since pharos's HLS surface only
    // emits H.264/mpegts. `Static=true` is direct-play → serve the file as-is.
    let ext = path.ext.as_deref().unwrap_or("");
    if ext.eq_ignore_ascii_case("webm") && !qs_flag(req.query_string(), "Static") {
        return stream_transcoded_webm(&state, &req, path.id_str()).await;
    }
    deliver_stream(&state, &req, path.id_str()).await
}

/// Live progressive VP9/WebM transcode. VP9 + Opus in a WebM container is
/// decodable by every modern browser (Firefox included) without any system
/// H.264 codec. Streamed straight from ffmpeg's stdout — no segmenting.
async fn stream_transcoded_webm(
    state: &AppState,
    req: &HttpRequest,
    id_str: &str,
) -> Result<HttpResponse, actix_web::Error> {
    // Progressive playback is live too — keep the background backfill parked
    // (the segment handlers do this; this path was missing it).
    state.note_playback_activity();
    let item = load_item(state, id_str).await?;
    let qs = req.query_string();
    let start_ticks = parse_start_time_ticks(qs);
    // Cap the encode bitrate: VP9 realtime software encoding is CPU-heavy, so
    // keep it modest. Honour the client's MaxStreamingBitrate when lower.
    let cap = parse_max_streaming_bitrate(qs)
        .unwrap_or(3_000_000)
        .clamp(500_000, 6_000_000);
    // `AudioStreamIndex` / `SubtitleStreamIndex` are ABSOLUTE ffprobe stream
    // indices (as jellyfin-web sends them), but the encoder args select by
    // per-CODEC index (`-map 0:a:N`, subtitle-filter `si=N`). Convert by the
    // track's position among its own codec's streams.
    let audio_abs: Vec<u32> = item
        .probe
        .audio_tracks
        .iter()
        .map(|t| t.stream_index)
        .collect();
    let sub_abs: Vec<u32> = item
        .probe
        .subtitle_tracks
        .iter()
        .map(|t| t.stream_index)
        .collect();
    let audio_rel = parse_query_u32(qs, "AudioStreamIndex")
        .and_then(|abs| codec_relative_index(&audio_abs, abs));
    // A progressive `<video src>` has no soft-subtitle selection, so the picked
    // subtitle is BURNED IN (only possible because VP9 re-encodes the frames).
    let sub_rel = parse_query_u32(qs, "SubtitleStreamIndex")
        .and_then(|abs| codec_relative_index(&sub_abs, abs));
    let opts = TranscodeOptions {
        container: Container::WebM,
        video: Some(VideoCodec::Vp9),
        audio: Some(AudioCodec::Opus),
        video_bitrate_bps: Some(cap),
        audio_bitrate_bps: Some(128_000),
        start_position_ticks: start_ticks,
        duration_ticks: None,
        audio_source_stream_index: audio_rel,
        burn_subtitle_stream_index: sub_rel,
    };
    tracing::info!(
        media.id = item.id,
        start_ticks,
        bitrate_cap = cap,
        audio_rel,
        sub_rel,
        burn = sub_rel.is_some(),
        "progressive webm transcode starting"
    );
    // Route through the load-balancing scheduler (crash-isolated worker,
    // spread across every GPU + CPU). Inline ffmpeg is only a last-resort
    // fallback when the scheduler genuinely declines (pool saturated).
    let clock = state.playback_activity.clone();
    if let Some(sched) = state.transcode_scheduler.as_ref() {
        match sched.submit_live(item.path.clone(), opts.clone()).await {
            Ok(stream) => {
                return Ok(meter_body(
                    HttpResponse::Ok()
                        .content_type("video/webm")
                        .streaming(stream),
                    clock,
                ));
            }
            Err(e) => {
                tracing::warn!(error = %e, "scheduler webm live transcode declined; inline fallback");
            }
        }
    }
    let transcoder = FfmpegTranscoder::new();
    let stream = transcoder
        .transcode(&item.path, &opts)
        .await
        .map_err(|e| error::ErrorInternalServerError(format!("webm transcode: {e}")))?;
    Ok(meter_body(
        HttpResponse::Ok()
            .content_type("video/webm")
            .streaming(stream.into_stream()),
        clock,
    ))
}

/// Parse an unsigned integer query param (case-insensitive key).
fn parse_query_u32(qs: &str, name: &str) -> Option<u32> {
    qs.split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .and_then(|(_, v)| v.parse().ok())
}

/// Map an absolute ffprobe stream index to its position among the streams of
/// one codec kind (what ffmpeg's `0:a:N` / `subtitles=si=N` expect).
fn codec_relative_index(abs_indices: &[u32], abs: u32) -> Option<u32> {
    abs_indices.iter().position(|&i| i == abs).map(|p| p as u32)
}

/// True when `name=true` (case-insensitive) appears in the query string.
fn qs_flag(qs: &str, name: &str) -> bool {
    qs.split('&')
        .filter_map(|kv| kv.split_once('='))
        .any(|(k, v)| k.eq_ignore_ascii_case(name) && v.eq_ignore_ascii_case("true"))
}

async fn stream_audio(
    state: web::Data<AppState>,
    req: HttpRequest,
    path: web::Path<StreamPath>,
) -> Result<HttpResponse, actix_web::Error> {
    // B86 — see head_audio: authorize via the ETag capability (`?tag=`), not a
    // bearer, so a tokenless native direct-play GET works (matches
    // stream_video/B75). Without this every music track 401'd and would not play.
    let media_id = pharos_jellyfin_api::dto::parse_item_id(path.id_str())
        .ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
    authorize_media(&state, &req, media_id).await?;
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
    // Direct-play is live playback too. Stamp on entry so the bg_io regulator
    // parks immediately, and route the body through `meter_body` so it STAYS
    // parked for the whole stream (V35) — B72's regulator-blind root.
    state.note_playback_activity();
    let clock = state.playback_activity.clone();
    let item = load_item(state, id_str).await?;
    let has_range = req.headers().contains_key(header::RANGE);
    let start_ticks = parse_start_time_ticks(req.query_string());

    if !has_range && start_ticks > 0 {
        if let Some(offset) = byte_offset_from_ticks(&item, start_ticks).await {
            return serve_from_offset(&item, offset, req, clock).await;
        }
    }

    let file = NamedFile::open_async(&item.path)
        .await
        .map_err(|e| error::ErrorNotFound(e.to_string()))?
        .use_etag(true)
        .use_last_modified(true);
    let mut resp = file.into_response(req);
    // B94 — Firefox's `<video>` opens playback with `Range: bytes=0-`, a range
    // that spans the whole file. actix-files gates its 206 on `offset != 0 ||
    // length != total` (named.rs:605), so it answers 200 while still stamping a
    // Content-Range header. Firefox reads the 200 as "server ignores ranges"
    // and marks the media non-seekable (seek bar inert / restarts at 0). Any
    // response to a Range request that carries a Content-Range is partial by
    // definition — promote it to 206 so the opening probe confirms seekability.
    if has_range
        && resp.status() == StatusCode::OK
        && resp.headers().contains_key(header::CONTENT_RANGE)
    {
        *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
    }
    // A Matroska file carrying only WebM-legal codecs (VP8/VP9/AV1 + Opus/
    // Vorbis) is playable by browsers as `video/webm`, but `mime_guess` maps
    // the `.mkv` extension to `video/x-matroska`, which Firefox rejects
    // outright ("Content-Type video/matroska is not supported"). Re-label such
    // a direct-play stream as `video/webm` so the browser accepts it.
    let webm_video = matches!(
        item.probe
            .video_codec
            .as_deref()
            .map(|c| c.to_ascii_lowercase())
            .as_deref(),
        Some("vp9" | "vp09" | "vp8" | "vp08" | "av1" | "av01")
    );
    let matroska = matches!(
        item.probe
            .container
            .as_deref()
            .map(|c| c.to_ascii_lowercase())
            .as_deref(),
        Some("webm" | "matroska" | "mkv")
    ) || item
        .path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("mkv") || e.eq_ignore_ascii_case("webm"));
    if webm_video && matroska {
        resp.headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static("video/webm"));
    }
    // P24 — echo the auth as a cookie so a follow-up `<video>`-style
    // fetch can drop the `?api_key=` and still authenticate.
    if let Some(token) = api_key_query_value(req.query_string()) {
        if let Ok(hv) = HeaderValue::from_str(&auth_cookie_header(&token)) {
            resp.headers_mut().insert(header::SET_COOKIE, hv);
        }
    }
    Ok(meter_body(resp, clock))
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
    let duration_ticks = probe.duration_ms.map(|ms| Ticks::from_millis(ms).0);

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
    clock: Arc<AtomicI64>,
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
    Ok(meter_body(resp, clock))
}

async fn load_item(state: &AppState, id_str: &str) -> Result<MediaItem, actix_web::Error> {
    let id: u64 = pharos_jellyfin_api::dto::parse_item_id(id_str)
        .ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
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
            metadata: Default::default(),
        }
    }

    #[tokio::test]
    async fn metered_body_stamps_clock_as_bytes_flow() {
        // V35 / B72: the body wrapper must restamp the playback clock every time
        // a chunk actually flows, so a long single GET keeps the bg_io regulator
        // parked for the whole stream — not just the request line.
        use actix_web::body::MessageBody;
        let clock = Arc::new(AtomicI64::new(0));
        let body = MeteredBody {
            inner: actix_web::web::Bytes::from_static(b"payload"),
            clock: clock.clone(),
        };
        assert_eq!(
            clock.load(Ordering::Relaxed),
            0,
            "no bytes have flowed yet → clock must be unstamped"
        );
        let mut body = std::pin::pin!(body);
        let chunk = futures_util::future::poll_fn(|cx| body.as_mut().poll_next(cx)).await;
        assert!(chunk.is_some(), "expected a data chunk to flow");
        assert!(
            clock.load(Ordering::Relaxed) > 0,
            "clock must stamp once bytes flow (V35)"
        );
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
