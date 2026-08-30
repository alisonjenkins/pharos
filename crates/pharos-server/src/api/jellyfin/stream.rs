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
    body::MessageBody,
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
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};

use pharos_core::time::{Ticks, TICKS_PER_SECOND};

/// Wraps a response body so the shared `playback_activity` clock is restamped
/// as bytes ACTUALLY flow to the client (V35). A single long GET — direct-play
/// `stream.mp4`, resume-from-offset, progressive webm, or an audio remux — thus
/// keeps the `bg_io` regulator parked for the WHOLE stream, not just the 12s
/// window after the request line (all a once-per-request stamp bought). B72: the
/// regulator was blind to every non-webm delivery path, so background sweeps ran
/// at full `BG_IO_MAX` during direct playback and starved live reads.
/// Bounded, stable label naming which streaming surface a body came from —
/// the counterpart to [`DirectPlayDelivery`] for body-COMPLETION
/// observability, since a delivery decision and whether its body actually
/// finished are two different questions. 2026-08-31 (House, skip-intro): the
/// DirectPlay decision logged a fast, correct 206 in ~2ms, and then the
/// client went silent for 82s with no signal ANYWHERE — not an abort (the
/// handler had already returned by then, so `RedMetrics`'s `AbortGuard` never
/// saw it), not an error, nothing — that the body itself never finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StreamBodyRoute {
    DirectPlay,
    Resume,
    ProgressiveWebm,
    AudioRemux,
}

impl StreamBodyRoute {
    fn label(self) -> &'static str {
        match self {
            Self::DirectPlay => "direct_play",
            Self::Resume => "resume",
            Self::ProgressiveWebm => "progressive_webm",
            Self::AudioRemux => "audio_remux",
        }
    }
}

/// Below this, a body that never reached its end is not worth a WARN — a
/// player cancelling one in-flight range because it issued a fresh seek is
/// routine, and every seek supersedes exactly one of these. Mirrors
/// `RedMetrics::ABORT_WARN_AFTER`, which draws the same line for the request
/// future this body-level guard cannot see (the handler has already returned
/// by the time this body starts streaming).
const BODY_INCOMPLETE_WARN_AFTER: std::time::Duration = std::time::Duration::from_secs(5);

struct MeteredBody<B> {
    inner: B,
    clock: Arc<AtomicI64>,
    media_id: u64,
    route: StreamBodyRoute,
    total: Option<u64>,
    sent: u64,
    started: Instant,
    /// Set once `poll_next` sees a natural end (`None`) or an error — both are
    /// a DEFINITE outcome the body itself already reported. Only a body
    /// dropped WITHOUT ever reaching one of those is the invisible case this
    /// type exists to name.
    settled: bool,
}

impl<B> Drop for MeteredBody<B> {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        let elapsed = self.started.elapsed();
        metrics::counter!(
            "pharos_stream_body_incomplete_total",
            "route" => self.route.label(),
        )
        .increment(1);
        if self.sent > 0 || elapsed >= BODY_INCOMPLETE_WARN_AFTER {
            tracing::warn!(
                media.id = self.media_id,
                route = self.route.label(),
                sent_bytes = self.sent,
                total_bytes = ?self.total,
                elapsed_ms = elapsed.as_millis() as u64,
                "a stream body was dropped before it finished — the client either \
                 disconnected or stalled reading it, and the request itself already \
                 returned its response headers so nothing else would have caught this"
            );
        }
    }
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
        match &polled {
            Poll::Ready(Some(Ok(chunk))) => {
                // Bytes just went out — mark playback live NOW. Cheap relaxed store.
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                this.clock.store(now, Ordering::Relaxed);
                this.sent += chunk.len() as u64;
            }
            Poll::Ready(None) => {
                this.settled = true;
                metrics::counter!(
                    "pharos_stream_body_finished_total",
                    "route" => this.route.label(),
                )
                .increment(1);
            }
            Poll::Ready(Some(Err(_))) => {
                this.settled = true;
                tracing::warn!(
                    media.id = this.media_id,
                    route = this.route.label(),
                    sent_bytes = this.sent,
                    total_bytes = ?this.total,
                    "a stream body errored mid-transfer"
                );
                metrics::counter!(
                    "pharos_stream_body_error_total",
                    "route" => this.route.label(),
                )
                .increment(1);
            }
            Poll::Pending => {}
        }
        polled
    }
}

/// Route a delivery response's body through [`MeteredBody`] so the playback
/// clock keeps ticking for the stream's whole lifetime (V35), AND so a body
/// that never reaches a natural end or an error — dropped mid-transfer,
/// invisible to every other signal pharos has — gets one. Every direct-play
/// / resume / progressive / audio delivery return value passes through here.
fn meter_body(
    resp: HttpResponse,
    clock: Arc<AtomicI64>,
    media_id: u64,
    route: StreamBodyRoute,
) -> HttpResponse {
    resp.map_body(|_, body| {
        let total = match body.size() {
            actix_web::body::BodySize::Sized(n) => Some(n),
            _ => None,
        };
        actix_web::body::BoxBody::new(MeteredBody {
            inner: body,
            clock: clock.clone(),
            media_id,
            route,
            total,
            sent: 0,
            started: Instant::now(),
            settled: false,
        })
    })
}

pub fn register(cfg: &mut web::ServiceConfig) {
    // T31: lowercase canonical paths; `LowercasePath` middleware
    // rewrites jellyfin-web's PascalCase before the router matches.
    cfg.route("/videos/{id}/stream", web::get().to(stream_video))
        .route("/videos/{id}/stream.{ext}", web::get().to(stream_video))
        .route("/videos/{id}/stream", web::head().to(head_video))
        // B95 — Firefox HEAD-probes the extensioned DirectPlay URL
        // (`stream.mp4`) to confirm range support before it treats the media
        // as seekable. Without a HEAD handler here the probe 405'd and Firefox
        // collapsed `seekable` to `buffered`.
        .route("/videos/{id}/stream.{ext}", web::head().to(head_video))
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
    head_response(&state, &req, path.id_str()).await
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
    head_response(&state, &req, path.id_str()).await
}

/// P11 — HEAD short-circuit. Returns Content-Length + Content-Type + range
/// support without transmitting the body. Mobile clients use HEAD to validate
/// a stream URL before issuing the playback GET; without this they fall back to
/// GET-then-cancel. P25 — also emits `Last-Modified` so a phone re-opening the
/// player can conditional-GET the range cache instead of re-downloading.
///
/// B101 — serve the HEAD through `NamedFile` rather than a hand-built
/// `.finish()` response. actix's h1 encoder derives a HEAD response's
/// `Content-Length` from the response body's declared `BodySize` (the body
/// bytes are never sent for HEAD) and drops any manually-inserted
/// `Content-Length` header. An empty `()` body is `BodySize::Sized(0)`, so the
/// old code advertised `Content-Length: 0` for every file — Firefox HEAD-probes
/// a progressive `<video>` source to learn its length so it can range-fetch the
/// trailing `moov` seek index of a non-faststart mp4; a zero length reads as
/// "nothing to seek" and collapses `seekable` to `buffered`. `NamedFile`'s body
/// is a `SizedStream` whose `BodySize::Sized(file_len)` makes the encoder emit
/// the real length; its reader is never polled on a HEAD, so no bytes are read.
/// It also sets `Accept-Ranges`, `Content-Type`, `ETag`, and `Last-Modified`,
/// and honours `If-Modified-Since` / `If-None-Match`.
async fn head_response(
    state: &AppState,
    req: &HttpRequest,
    id_str: &str,
) -> Result<HttpResponse, actix_web::Error> {
    let item = load_item(state, id_str).await?;
    let file = NamedFile::open_async(&item.path)
        .await
        .map_err(|e| source_unreadable(&item, &e))?
        .use_etag(true)
        .use_last_modified(true);
    let mut resp = file.into_response(req);
    // Same `DeliveryMime` as the GET, so the Firefox seekability HEAD-probe
    // advertises the exact Content-Type the body will carry — a mkv/VP9 HEAD
    // must not say `video/x-matroska` while the GET serves `video/webm`.
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        super::seek::DeliveryMime::for_source(&item).header(),
    );
    Ok(resp)
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
    // A remuxed/downmixed stream is a live ffmpeg pipe (chunked, no
    // Content-Length) so the browser can't byte-range seek it; jellyfin-web
    // instead re-requests this URL with a new `StartTimeTicks` on every seek
    // (the same contract the progressive-WebM transcode honours). Without an
    // input seek the encode always restarted at 0, so the user could only seek
    // within what had already streamed (B102). Honour it via `-ss`.
    let start_ticks = parse_start_time_ticks(qs);
    audio_remux(
        &item,
        &target,
        bitrate,
        max_channels,
        start_ticks,
        state.playback_activity.clone(),
    )
    .await
}

async fn audio_remux(
    item: &MediaItem,
    target_codec: &str,
    bitrate_bps: Option<u64>,
    max_channels: Option<u32>,
    start_ticks: u64,
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
    cmd.args(audio_remux_args(
        &item.path,
        ffmpeg_codec,
        muxer,
        bitrate,
        max_channels,
        start_ticks,
    ))
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
        item.id,
        StreamBodyRoute::AudioRemux,
    ))
}

/// Build the ffmpeg argv for a live audio remux/downmix. Pure so the
/// seek-offset ordering is unit-testable without spawning ffmpeg: `-ss` MUST
/// precede `-i` to act as an INPUT seek (fast keyframe seek + decode forward);
/// placed after `-i` it would decode from 0 and be tens-of-seconds slow deep in
/// a file. A resume/seek re-request carries a fresh `StartTimeTicks`, so this is
/// the only thing that makes a remuxed/downmixed stream seekable (B102).
fn audio_remux_args(
    input: &std::path::Path,
    ffmpeg_codec: &str,
    muxer: &str,
    bitrate: u64,
    max_channels: Option<u32>,
    start_ticks: u64,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
    ];
    if start_ticks > 0 {
        let secs = start_ticks as f64 / TICKS_PER_SECOND as f64;
        args.push("-ss".into());
        args.push(format!("{secs:.3}"));
    }
    args.push("-i".into());
    args.push(input.to_string_lossy().into_owned());
    args.push("-vn".into());
    args.push("-c:a".into());
    args.push(ffmpeg_codec.into());
    args.push("-b:a".into());
    args.push(bitrate.to_string());
    // P19 — downmix to the requested channel count when the client asked for
    // one. ffmpeg's `-ac N` runs a default mix-down for surround → stereo/mono.
    if let Some(n) = max_channels.filter(|n| *n > 0) {
        args.push("-ac".into());
        args.push(n.to_string());
    }
    args.push("-f".into());
    args.push(muxer.into());
    args.push("pipe:1".into());
    args
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
        source_frame_rate: None,
        container: Container::WebM,
        video: Some(VideoCodec::Vp9),
        source_video_codec: item
            .probe
            .video_codec
            .as_deref()
            .and_then(pharos_transcode::SourceCodec::from_name),
        audio: Some(AudioCodec::Opus),
        video_bitrate_bps: Some(cap),
        audio_bitrate_bps: Some(128_000),
        start_position_ticks: start_ticks,
        duration_ticks: None,
        audio_source_stream_index: audio_rel,
        burn_subtitle_stream_index: sub_rel,

        burn_intent: sub_rel.is_some(),
        burn_subtitle_is_text: false,
        burn_subtitle_ass_path: None,
        burn_fonts_dir: None,
        decode_preroll_seconds: None,
        muxed_audio_source: None,
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
                    item.id,
                    StreamBodyRoute::ProgressiveWebm,
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
        item.id,
        StreamBodyRoute::ProgressiveWebm,
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

/// Cap for a single DirectPlay range response body.
///
/// A `<video>` opens a progressive (non-faststart) mp4 with an open-ended
/// `Range: bytes=X-`; answering the whole `X..EOF` — up to ~1.8 GB for a
/// feature film — in one body makes a reverse proxy temp-file the entire
/// remainder before relaying (an unbuffered seek then takes minutes), and a
/// stalled/reset progressive connection restarts the `<video>` at 0 (the
/// Deadpool report). Capping to a bounded window the client simply re-requests
/// keeps every response small and fast. 8 MiB ≈ 10 s at 6 Mbps.
const DIRECTPLAY_RANGE_CAP_BYTES: u64 = 8 * 1024 * 1024;

/// Which shape of player asked for a DirectPlay body.
///
/// The two consume a byte-range response differently, so the delivery decision
/// has to name which one it is answering. A browser `<video>`/MSE re-requests
/// the next window when a response ends short of the file; a native
/// progressive player (ExoPlayer on Android TV, AVPlayer) trusts the declared
/// `Content-Length` of an open-ended `bytes=X-` and reports end-of-input to its
/// extractor when the body runs out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DirectPlayClient {
    Browser,
    Native,
}

impl DirectPlayClient {
    /// Same `Mozilla` User-Agent test PlaybackInfo uses for `is_web_client`, so
    /// the negotiation and the delivery agree on what the client is.
    fn of(req: &HttpRequest) -> Self {
        let mozilla = req
            .headers()
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ua| ua.contains("Mozilla"));
        if mozilla {
            Self::Browser
        } else {
            Self::Native
        }
    }

    /// Stable metric/log label. Bounded cardinality — two values, forever.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Browser => "browser",
            Self::Native => "native",
        }
    }

    /// Whether an over-large open-ended `Range` may be answered with a capped
    /// window instead of the whole tail.
    ///
    /// B140 — only a browser. A `<video>`/MSE source buffer treats a short
    /// response as "this window is done" and issues the next `bytes=X-`, which
    /// is what makes the Deadpool cap work. A native progressive player does
    /// not: ExoPlayer's `DefaultHttpDataSource` sizes its read from the
    /// response's `Content-Length`, and when that many bytes have been consumed
    /// it returns `RESULT_END_OF_INPUT` to the extractor — the media simply
    /// *ends* at the cap. Capping a native player's open-ended range therefore
    /// truncates the film to the first 8 MiB, which is why the Android TV app
    /// opened Central Intelligence, read one window and gave up. Native clients
    /// get the whole tail; the reverse-proxy buffering the cap exists to avoid
    /// is a browser-path problem.
    fn caps_ranges(self) -> bool {
        matches!(self, Self::Browser)
    }
}

/// What `deliver_stream` actually put on the wire. Recorded for EVERY branch —
/// the whole-file open is as interesting as the capped window, because the bug
/// this exists to expose is "which of these did the player get?".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DirectPlayDelivery {
    /// `NamedFile` served the request whole (no range, or a range small enough
    /// / exotic enough to defer to actix-files' own range handling).
    Whole,
    /// An over-large open-ended range was answered with a bounded window.
    CappedWindow,
    /// A `StartTimeTicks` resume with no `Range`, cut at a byte offset on a
    /// self-framing container.
    TicksResume,
}

impl DirectPlayDelivery {
    /// Stable metric/log label. Bounded cardinality.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Whole => "whole",
            Self::CappedWindow => "capped_window",
            Self::TicksResume => "ticks_resume",
        }
    }
}

/// Record the DirectPlay delivery decision: its inputs (client shape, the
/// `Range` the player asked for, the file size), its verdict, and the window
/// actually served. Both success shapes go through here, so a truncated
/// response can be told from a whole one by query rather than by inference —
/// the Android TV give-up (B140) was invisible precisely because this path
/// logged nothing while PlaybackInfo beside it logged fourteen fields.
fn record_directplay_delivery(
    item: &MediaItem,
    client: DirectPlayClient,
    range_header: Option<&str>,
    total: u64,
    delivery: DirectPlayDelivery,
    served: Option<super::seek::ContentRange>,
) {
    tracing::info!(
        media.id = item.id,
        client = client.label(),
        range = range_header.unwrap_or("-"),
        source.total_bytes = total,
        delivery = delivery.label(),
        served.offset = served.map(|r| r.offset()).unwrap_or(0),
        served.len = served.map(|r| r.content_length()).unwrap_or(total),
        served.truncated = served.is_some_and(|r| r.end() + 1 < r.total()),
        "directplay: delivery decision"
    );
    metrics::counter!(
        "pharos_directplay_delivery_total",
        "delivery" => delivery.label(),
        "client" => client.label(),
    )
    .increment(1);
}

/// Decide whether a DirectPlay `Range` should be answered as a *capped* window
/// rather than deferred whole to `NamedFile`. `Some` only for a single
/// `bytes=START-…` range whose served length exceeds `cap`; `None` for small,
/// suffix (`bytes=-N`), multi-range, or unparseable headers — those stay on
/// `NamedFile`, which handles the full range grammar (and its ETag / conditional
/// / multipart behaviour) unchanged.
fn capped_window(range_header: &str, total: u64, cap: u64) -> Option<super::seek::ContentRange> {
    let range = super::seek::ByteRange::parse(range_header)?;
    let served = range.served_len(total)?;
    if served <= cap {
        return None;
    }
    super::seek::ContentRange::window(range.start, total, cap)
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
    let client = DirectPlayClient::of(req);
    let range_header = req
        .headers()
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    // Stat once up front so every delivery branch can report the file size it
    // was deciding against, including the whole-file one.
    let total = tokio::fs::metadata(&item.path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);

    if !has_range && start_ticks > 0 {
        // A StartTimeTicks resume with no Range can only be honoured by cutting
        // the source at a byte offset and streaming from there. That is
        // decodable ONLY for a self-framing container (MPEG-TS / ADTS-AAC /
        // MP3), which resyncs from any packet. For a header-prefixed
        // mp4/mkv/webm the moov / EBML index / cues live at file start or EOF,
        // so a raw interior slice is HEADERLESS and undecodable — the old
        // high-severity bug shipped a 206 the player could not decode.
        // `ResyncWitness` makes that call unrepresentable: for a header-prefixed
        // source we skip the byte cut and fall through to the whole-file
        // NamedFile response, which is fully seekable — the client jumps to the
        // resume offset itself using its own container index (a browser issues
        // a Range; a native player self-seeks).
        let tolerance = super::seek::CutTolerance::for_source(&item);
        if let Some(witness) = super::seek::ResyncWitness::of(tolerance) {
            if let Some(offset) = byte_offset_from_ticks(&item, start_ticks).await {
                record_directplay_delivery(
                    &item,
                    client,
                    range_header.as_deref(),
                    total,
                    DirectPlayDelivery::TicksResume,
                    super::seek::ContentRange::from_offset(offset, total),
                );
                return serve_from_offset(&item, offset, req, clock, witness).await;
            }
        }
    }

    // Cap an over-large / open-ended DirectPlay range to a bounded window so the
    // client re-requests the next chunk. A single multi-GB `bytes=X-` answer
    // stalls proxy buffering (minutes-long unbuffered seeks) and a reset
    // progressive connection restarts the `<video>` at 0 (Deadpool). Capping a
    // byte-range the client explicitly asked for is valid for ANY container (we
    // return a prefix of the requested bytes), so — unlike the StartTimeTicks
    // time→byte cut above — it needs no `ResyncWitness`. Small / suffix / multi
    // ranges fall through to `NamedFile`.
    if let Some(rh) = range_header.as_deref().filter(|_| client.caps_ranges()) {
        if let Some(window) = capped_window(rh, total, DIRECTPLAY_RANGE_CAP_BYTES) {
            record_directplay_delivery(
                &item,
                client,
                Some(rh),
                total,
                DirectPlayDelivery::CappedWindow,
                Some(window),
            );
            return serve_content_range(&item, window, req, clock).await;
        }
    }

    record_directplay_delivery(
        &item,
        client,
        range_header.as_deref(),
        total,
        DirectPlayDelivery::Whole,
        None,
    );

    let file = NamedFile::open_async(&item.path)
        .await
        .map_err(|e| source_unreadable(&item, &e))?
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
    // Content-Type is computed ONCE, via `seek::DeliveryMime`, so this open,
    // the StartTimeTicks seek (`serve_from_offset`) and the HEAD probe
    // (`head_response`) can never disagree. It relabels a WebM-legal
    // Matroska/WebM source (VP8/VP9/AV1) to `video/webm`, because `mime_guess`
    // maps `.mkv` to `video/x-matroska`, which Firefox rejects ("Content-Type
    // video/matroska is not supported"); for every other source it is the
    // identical `mime_guess` value NamedFile already set.
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        super::seek::DeliveryMime::for_source(&item).header(),
    );
    // P24 — echo the auth as a cookie so a follow-up `<video>`-style
    // fetch can drop the `?api_key=` and still authenticate.
    if let Some(token) = api_key_query_value(req.query_string()) {
        if let Ok(hv) = HeaderValue::from_str(&auth_cookie_header(&token)) {
            resp.headers_mut().insert(header::SET_COOKIE, hv);
        }
    }
    Ok(meter_body(
        resp,
        clock,
        item.id,
        StreamBodyRoute::DirectPlay,
    ))
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

/// Serve `file[offset..EOF]` as a 206. Callable ONLY with a
/// [`ResyncWitness`](super::seek::ResyncWitness) — proof the source is a
/// self-framing container that decodes from an arbitrary interior byte. A
/// header-prefixed mp4/mkv cannot produce one, so the headerless-slice bug is a
/// compile error rather than a runtime 206 the player chokes on.
async fn serve_from_offset(
    item: &MediaItem,
    offset: u64,
    req: &HttpRequest,
    clock: Arc<AtomicI64>,
    _witness: super::seek::ResyncWitness,
) -> Result<HttpResponse, actix_web::Error> {
    let total = tokio::fs::metadata(&item.path)
        .await
        .map_err(|e| error::ErrorInternalServerError(format!("stat: {e}")))?
        .len();
    // A past-EOF offset is unrepresentable as a `ContentRange` → 416. `status()`
    // is hard-wired to 206, so this response can never regress to a 200 the
    // browser reads as "ranges unsupported" (the B94 case).
    let Some(range) = super::seek::ContentRange::from_offset(offset, total) else {
        return Err(error::ErrorRangeNotSatisfiable("StartTimeTicks past EOF"));
    };
    serve_content_range(item, range, req, clock).await
}

/// Stream a bounded `206` body for `range` from `item`'s file: seek to
/// `range.offset()` and emit EXACTLY `range.content_length()` bytes with a
/// `Content-Range` / `Content-Length` that match. Shared by the StartTimeTicks
/// resume (`serve_from_offset`, whole tail) and the capped DirectPlay range
/// (`deliver_stream`, a bounded window), so both answer a seek with a
/// decodable, self-consistent partial body that never runs past its window.
async fn serve_content_range(
    item: &MediaItem,
    range: super::seek::ContentRange,
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
        .map_err(|e| source_unreadable(item, &e))?;
    file.seek(SeekFrom::Start(range.offset()))
        .await
        .map_err(|e| error::ErrorInternalServerError(format!("seek: {e}")))?;

    let remaining = range.content_length();
    // Same `DeliveryMime` as the plain open + HEAD, so a mkv/VP9 seek is not
    // served the `video/x-matroska` Firefox rejects.
    let mime = super::seek::DeliveryMime::for_source(item).header();
    let mut resp_builder = HttpResponse::build(range.status());
    resp_builder
        .insert_header((header::CONTENT_TYPE, mime))
        .insert_header((header::CONTENT_RANGE, range.header_value()))
        .insert_header((header::ACCEPT_RANGES, HeaderValue::from_static("bytes")));
    if let Some(lm) = last_modified_from_meta(meta_for_lm.as_ref()) {
        resp_builder.insert_header((header::LAST_MODIFIED, lm.as_str()));
    }
    // P24 — echo the auth as a cookie so a follow-up `<video>`-style fetch can
    // drop the `?api_key=` and still authenticate (parity with the NamedFile
    // branch of `deliver_stream`).
    if let Some(token) = api_key_query_value(req.query_string()) {
        if let Ok(hv) = HeaderValue::from_str(&auth_cookie_header(&token)) {
            resp_builder.insert_header((header::SET_COOKIE, hv));
        }
    }
    // Read EXACTLY `remaining` bytes — a capped window must NOT run to EOF, and a
    // 206 must carry a declared length (never chunked framing). Small windows
    // buffer to a sized `Vec`; large ones (a whole-tail resume) stream via a
    // length-bounded `SizedStream`.
    let resp = if remaining <= 16 * 1024 * 1024 {
        let mut buf = vec![0u8; remaining as usize];
        file.read_exact(&mut buf)
            .await
            .map_err(|e| error::ErrorInternalServerError(format!("read: {e}")))?;
        resp_builder.body(buf)
    } else {
        let limited = file.take(remaining);
        let stream = tokio_util::io::ReaderStream::with_capacity(limited, 64 * 1024);
        let stream = futures_util::TryStreamExt::map_err(stream, |e| {
            actix_web::error::ErrorInternalServerError(format!("read: {e}"))
        });
        resp_builder.body(actix_web::body::SizedStream::new(remaining, stream))
    };
    Ok(meter_body(resp, clock, item.id, StreamBodyRoute::Resume))
}

/// Turn a failure to OPEN a known item's file into the 404 the client sees,
/// after recording WHAT could not be opened and why.
///
/// The two 404s a video route can produce are indistinguishable to a client and
/// used to be indistinguishable in the log as well: an unknown item id, and a
/// known item whose bytes are not readable on disk. The second is a storage
/// incident, not a catalogue one — when the mergerfs pool stopped, every path
/// resolved to nothing, the whole library "just stopped playing", and all the
/// evidence there was to work from was undifferentiated 404s.
///
/// The path stays in the log; the client is told only that the source is
/// unreadable, since a filesystem layout is not a client's business.
fn source_unreadable(item: &MediaItem, e: &std::io::Error) -> actix_web::Error {
    let reason = match e.kind() {
        std::io::ErrorKind::NotFound => "missing",
        std::io::ErrorKind::PermissionDenied => "permission",
        _ => "io",
    };
    tracing::warn!(
        media.id = item.id,
        path = %item.path.display(),
        reason,
        error = %e,
        "media source is catalogued but unreadable on disk"
    );
    metrics::counter!("pharos_source_unreadable_total", "reason" => reason).increment(1);
    error::ErrorNotFound("media source unreadable")
}

/// 008 — refuse a URL-backed item at the byte routes, or `None` for a file.
///
/// Every route in this module serves bytes straight off the filesystem
/// (`NamedFile` / `tokio::fs`), so a synthetic path cannot be served here at
/// all. `playback_info` already declines to advertise direct play for these, so
/// arriving here means a client followed a stale URL or ignored the verdict —
/// uncommon, and not worth alarming on.
///
/// The metric reason is DISTINCT from the unreadable-file reasons on purpose.
/// `pharos_source_unreadable_total` is what an operator reads as "media has
/// gone from disk", and filing a by-design refusal in that bucket would turn a
/// working feature into a permanent false alarm.
fn refuse_remote(item: &MediaItem) -> Option<actix_web::Error> {
    let r = item.origin().remote()?;
    tracing::debug!(
        media.id = item.id,
        extractor = r.extractor(),
        "refusing a byte route for a URL-backed item; it is served over HLS"
    );
    metrics::counter!("pharos_source_unreadable_total", "reason" => "remote_source").increment(1);
    Some(error::ErrorNotFound("media source is not a local file"))
}

async fn load_item(state: &AppState, id_str: &str) -> Result<MediaItem, actix_web::Error> {
    let id: u64 = pharos_jellyfin_api::dto::parse_item_id(id_str)
        .ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
    let item = state.stores.get(id).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("not found"),
        other => error::ErrorInternalServerError(other.to_string()),
    })?;
    // 008 — every route in this module serves bytes straight off the filesystem
    // (NamedFile / tokio::fs), so a URL-backed item cannot be served here at
    // all. Refused ONCE, at the single point every byte route loads through, so
    // a new route inherits the refusal instead of having to remember it.
    //
    // `playback_info` already declines to advertise direct play for these, so
    // reaching this is a client following a stale URL or ignoring the verdict —
    // uncommon but not an error worth alarming on.
    if let Some(e) = refuse_remote(&item) {
        return Err(e);
    }
    Ok(item)
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

    /// 008 — a URL-backed item is refused by the byte routes, and the refusal
    /// is NOT filed under the reason that means "media has gone from disk".
    ///
    /// Both directions asserted: a helper that refused everything would satisfy
    /// the first half on its own, and refusing local items would take the whole
    /// library offline.
    #[test]
    fn a_remote_item_is_refused_by_the_byte_routes() {
        let _ = crate::obs::init("info", None);
        let remote = MediaItem {
            id: 1,
            path: pharos_core::RemoteRef::new("youtube", "dQw4w9WgXcQ")
                .expect("valid ref")
                .to_synthetic_path(),
            kind: MediaKind::Movie,
            ..Default::default()
        };
        let err = refuse_remote(&remote).expect("a remote item must be refused here");
        assert_eq!(err.as_response_error().status_code(), 404);
        assert!(
            !err.to_string().contains("ytdlp://"),
            "the refusal must not hand the client the server's locator: {err}"
        );

        let local = MediaItem {
            id: 2,
            path: "/media/Movies/Arrival.mkv".into(),
            kind: MediaKind::Movie,
            ..Default::default()
        };
        assert!(
            refuse_remote(&local).is_none(),
            "an ordinary local item must still be served"
        );
    }

    /// A catalogued item whose bytes are gone still answers 404 — the client
    /// contract is unchanged — but it must no longer be MUTE about it, and it
    /// must not hand the client the server's filesystem layout.
    #[test]
    fn an_unreadable_source_is_a_404_that_names_itself_in_the_metric() {
        let _ = crate::obs::init("info", None);
        let item = MediaItem {
            id: 4242,
            path: "/mnt/media/gone.mkv".into(),
            ..Default::default()
        };
        let err = source_unreadable(
            &item,
            &std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"),
        );
        assert_eq!(err.as_response_error().status_code(), StatusCode::NOT_FOUND);
        assert!(
            !err.to_string().contains("/mnt/media"),
            "the on-disk path must not reach the client: {err}"
        );

        let rendered = crate::obs::render();
        assert!(
            rendered
                .lines()
                .any(|l| l.starts_with("pharos_source_unreadable_total")
                    && l.contains("reason=\"missing\"")),
            "a vanished source must be countable apart from an unknown id; \
             rendered:\n{rendered}"
        );
    }

    fn item_with_bitrate(bitrate_bps: Option<u64>, size_bytes: Option<u64>) -> MediaItem {
        MediaItem {
            id: 1,
            path: "/no/such".into(),
            title: "t".into(),
            kind: MediaKind::Movie,
            book: None,
            probe: MediaProbe {
                duration_ms: Some(60_000), // 60s
                bitrate_bps,
                size_bytes,
                ..Default::default()
            },
            series: None,
            created_at: None,
            metadata: Default::default(),
            has_primary_art: false,
            art_version: 0,
            match_provider: None,
            match_external_id: None,
            match_source: None,
            match_confidence: None,
            metadata_refreshed_at: None,
        }
    }

    // B102 — a seek/resume re-request carries a fresh StartTimeTicks; the remux
    // must input-seek to it (`-ss` BEFORE `-i`) or the encode restarts at 0 and
    // the user can only seek within already-streamed audio.
    #[::core::prelude::v1::test]
    fn audio_remux_args_seek_is_input_option() {
        let args = audio_remux_args(
            std::path::Path::new("/m.mkv"),
            "aac",
            "adts",
            192_000,
            Some(2),
            60 * TICKS_PER_SECOND, // 60s
        );
        let joined = args.join(" ");
        let ss = args.iter().position(|a| a == "-ss").expect("-ss present");
        let i = args.iter().position(|a| a == "-i").expect("-i present");
        assert!(ss < i, "-ss must precede -i (input seek): {joined}");
        assert_eq!(args[ss + 1], "60.000", "seek seconds: {joined}");
        assert!(joined.contains("-ac 2"), "downmix preserved: {joined}");
    }

    #[::core::prelude::v1::test]
    fn audio_remux_args_no_seek_at_zero() {
        let args = audio_remux_args(
            std::path::Path::new("/m.mkv"),
            "aac",
            "adts",
            192_000,
            None,
            0,
        );
        assert!(
            !args.iter().any(|a| a == "-ss"),
            "no input seek at ticks 0: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a == "-ac"),
            "no downmix when channels None: {args:?}"
        );
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
            media_id: 1,
            route: StreamBodyRoute::DirectPlay,
            total: Some(7),
            sent: 0,
            started: Instant::now(),
            settled: false,
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

    /// 2026-08-31 (House, skip-intro): a body dropped mid-transfer — the
    /// client disconnected or stalled reading it — produced no signal
    /// anywhere. `RedMetrics`'s abort guard only watches the handler's
    /// future, which had already returned by the time this body starts
    /// streaming, so a stall here was genuinely invisible before this.
    #[tokio::test]
    async fn a_body_dropped_before_it_finishes_is_counted_incomplete() {
        use metrics_util::debugging::DebuggingRecorder;

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let body = MeteredBody {
            inner: actix_web::web::Bytes::from_static(b"payload"),
            clock: Arc::new(AtomicI64::new(0)),
            media_id: 42,
            route: StreamBodyRoute::DirectPlay,
            total: Some(7),
            sent: 0,
            started: Instant::now(),
            settled: false,
        };
        {
            // One successful poll (the whole 7-byte payload) — then dropped
            // WITHOUT the follow-up poll that would have returned `None`.
            // Exactly what a client that reads the first chunk and then
            // disconnects looks like from the server's side.
            let mut body = std::pin::pin!(body);
            let chunk = futures_util::future::poll_fn(|cx| body.as_mut().poll_next(cx)).await;
            assert!(chunk.is_some(), "expected the one chunk to flow first");
        }

        let incomplete = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .any(|(ck, _, _, _)| ck.key().name() == "pharos_stream_body_incomplete_total");
        assert!(
            incomplete,
            "a body dropped before a natural end or an error must be counted incomplete"
        );
        let finished = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .any(|(ck, _, _, _)| ck.key().name() == "pharos_stream_body_finished_total");
        assert!(
            !finished,
            "a body that never reached None must not ALSO count as finished"
        );
    }

    /// The success side of the same signal: a body that runs to its natural
    /// end must be counted finished, not incomplete — otherwise every
    /// ordinary completed stream would falsely look like a stall.
    #[tokio::test]
    async fn a_body_that_finishes_naturally_is_not_counted_incomplete() {
        use metrics_util::debugging::DebuggingRecorder;

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let body = MeteredBody {
            inner: actix_web::web::Bytes::from_static(b"payload"),
            clock: Arc::new(AtomicI64::new(0)),
            media_id: 42,
            route: StreamBodyRoute::DirectPlay,
            total: Some(7),
            sent: 0,
            started: Instant::now(),
            settled: false,
        };
        {
            let mut body = std::pin::pin!(body);
            let first = futures_util::future::poll_fn(|cx| body.as_mut().poll_next(cx)).await;
            assert!(first.is_some(), "expected the one chunk to flow first");
            let second = futures_util::future::poll_fn(|cx| body.as_mut().poll_next(cx)).await;
            assert!(second.is_none(), "Bytes must end after its one chunk");
        }

        let finished = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .any(|(ck, _, _, _)| ck.key().name() == "pharos_stream_body_finished_total");
        assert!(
            finished,
            "a body that reaches None must be counted finished"
        );
        let incomplete = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .any(|(ck, _, _, _)| ck.key().name() == "pharos_stream_body_incomplete_total");
        assert!(
            !incomplete,
            "a body that finished normally must not ALSO count as incomplete"
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
    fn capped_window_caps_open_ended_and_oversized_ranges() {
        let total = 2_000_000_000; // ~2 GB feature film
        let cap = 8 * 1024 * 1024;
        // Open-ended near start → capped to an 8 MiB window at that offset.
        let w = capped_window("bytes=379000000-", total, cap).unwrap();
        assert_eq!(w.offset(), 379_000_000);
        assert_eq!(w.content_length(), cap);
        assert_eq!(w.end(), 379_000_000 + cap - 1);
        // Open-ended from 0 (Firefox's opening probe) is also capped.
        let w0 = capped_window("bytes=0-", total, cap).unwrap();
        assert_eq!(w0.offset(), 0);
        assert_eq!(w0.content_length(), cap);
        // A closed range larger than the cap is capped too.
        let wc = capped_window("bytes=100-900000000", total, cap).unwrap();
        assert_eq!(wc.offset(), 100);
        assert_eq!(wc.content_length(), cap);
    }

    #[::core::prelude::v1::test]
    fn capped_window_defers_small_suffix_and_multi_to_namedfile() {
        let total = 2_000_000_000;
        let cap = 8 * 1024 * 1024;
        // A range already within the cap is served whole by NamedFile.
        assert!(capped_window("bytes=100-199", total, cap).is_none());
        // Open-ended range whose tail is smaller than the cap (near EOF).
        assert!(capped_window("bytes=1999000000-", total, cap).is_none());
        // Suffix and multi-range are NamedFile's job.
        assert!(capped_window("bytes=-500", total, cap).is_none());
        assert!(capped_window("bytes=0-99,200-299", total, cap).is_none());
        // Garbage / start past EOF.
        assert!(capped_window("bytes=abc", total, cap).is_none());
        assert!(capped_window("bytes=2000000000-", total, cap).is_none());
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

    // The two label sets are a dashboard contract: a rename silently breaks
    // any panel or alert built on `pharos_directplay_delivery_total`, and two
    // variants sharing a string would merge distinct outcomes into one series.
    #[::core::prelude::v1::test]
    fn directplay_delivery_labels_are_distinct_and_stable() {
        let all = [
            DirectPlayDelivery::Whole,
            DirectPlayDelivery::CappedWindow,
            DirectPlayDelivery::TicksResume,
        ];
        let labels: std::collections::BTreeSet<_> = all.iter().map(|d| d.label()).collect();
        assert_eq!(labels.len(), all.len(), "delivery labels must be distinct");
        assert_eq!(DirectPlayDelivery::Whole.label(), "whole");
        assert_eq!(DirectPlayDelivery::CappedWindow.label(), "capped_window");
        assert_eq!(DirectPlayDelivery::TicksResume.label(), "ticks_resume");
    }

    #[::core::prelude::v1::test]
    fn directplay_client_labels_are_distinct_and_stable() {
        assert_ne!(
            DirectPlayClient::Browser.label(),
            DirectPlayClient::Native.label()
        );
        assert_eq!(DirectPlayClient::Browser.label(), "browser");
        assert_eq!(DirectPlayClient::Native.label(), "native");
    }

    // The classifier is the input to the capping decision, so it has to agree
    // with what PlaybackInfo called the same client. jellyfin-web is Mozilla;
    // the Android TV SDK and its OkHttp fetches are not.
    #[actix_web::test]
    async fn directplay_client_classifies_browser_and_native_user_agents() {
        let browser = actix_web::test::TestRequest::get()
            .insert_header((
                header::USER_AGENT,
                "Mozilla/5.0 (X11; Linux x86_64) Gecko/20100101 Firefox/152.0",
            ))
            .to_http_request();
        assert_eq!(DirectPlayClient::of(&browser), DirectPlayClient::Browser);

        for ua in [
            "Jellyfin Android TV/0.19.9 via jellyfin-sdk-kotlin (OkHttp/4.12.0)",
            "okhttp/4.12.0",
        ] {
            let native = actix_web::test::TestRequest::get()
                .insert_header((header::USER_AGENT, ua))
                .to_http_request();
            assert_eq!(
                DirectPlayClient::of(&native),
                DirectPlayClient::Native,
                "{ua} must classify as a native player"
            );
        }

        // No User-Agent at all is a native player, not a browser — a browser
        // always sends one.
        let bare = actix_web::test::TestRequest::get().to_http_request();
        assert_eq!(DirectPlayClient::of(&bare), DirectPlayClient::Native);
    }
}
