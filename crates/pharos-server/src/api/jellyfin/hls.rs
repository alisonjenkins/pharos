//! Jellyfin HLS endpoints. Phase 1 ships a fixed-bitrate, single-
//! variant playlist; the per-segment URL spawns ffmpeg (via
//! `pharos-transcode`) on demand. No segment caching — phase 2.
//!
//! Auth follows the T7 direct-play pattern: token via `api_key` query
//! param OR any of the Emby/MediaBrowser headers (see auth_extractor).
//! The playlist embeds the api_key so the player can fetch segments
//! without re-auth in `<video src=…>`.
//!
//! V6 stays held by `pharos-transcode`: ffmpeg crashes never crash the
//! server, abandoned segments don't leak processes.

use crate::{
    api::jellyfin::auth_extractor::{extract_token, AuthUser},
    state::AppState,
};
use actix_web::{error, web, HttpRequest, HttpResponse, Responder};
use pharos_core::{MediaStore, Prober};
use pharos_scanner::FfmpegProber;
use pharos_transcode::{
    AudioCodec, Container, FfmpegTranscoder, TranscodeOptions, VideoCodec,
};

/// Segment length in seconds. 6 s matches Apple's HLS authoring spec
/// recommendation and what most clients ask for; Jellyfin's own
/// default is the same.
const SEGMENT_SECONDS: f64 = 6.0;
const TICKS_PER_SECOND: u64 = 10_000_000;

pub fn register(cfg: &mut web::ServiceConfig) {
    // T31: lowercase canonical paths; `LowercasePath` middleware
    // rewrites the PascalCase URIs the streamer emits before routing.
    cfg.route("/videos/{id}/master.m3u8", web::get().to(master_playlist))
        .route("/videos/{id}/main.m3u8", web::get().to(variant_playlist))
        .route("/videos/{id}/hls1/main/{seg}.ts", web::get().to(segment));
}

/// Snapshot of the probe-derived facts the HLS layer needs. Loaded
/// once per request from `MediaStore` instead of re-deriving in each
/// handler.
struct HlsItem {
    duration_seconds: f64,
    width: Option<u32>,
    height: Option<u32>,
    source_bitrate_bps: Option<u64>,
}

async fn load_hls_item(state: &AppState, id_str: &str) -> Result<HlsItem, actix_web::Error> {
    let id: u64 = id_str
        .parse()
        .map_err(|_| error::ErrorBadRequest("invalid id"))?;
    let item = state.stores.get(id).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("not found"),
        other => error::ErrorInternalServerError(other.to_string()),
    })?;
    // Prefer the probe persisted at scan time. Fall back to live
    // ffprobe only when the row predates the probe-metadata migration
    // so the hot path stays subprocess-free.
    let duration_seconds = match item.probe.duration_ms {
        Some(ms) => ms as f64 / 1000.0,
        None => {
            let prober = FfmpegProber::new();
            let info = prober
                .probe(&item.path)
                .await
                .map_err(|e| error::ErrorInternalServerError(format!("probe: {e}")))?;
            info.duration_ms()
                .map(|ms| ms as f64 / 1000.0)
                .unwrap_or(0.0)
        }
    };
    Ok(HlsItem {
        duration_seconds,
        width: item.probe.width,
        height: item.probe.height,
        source_bitrate_bps: item.probe.bitrate_bps,
    })
}

/// Pick the bitrate we cap the encoder at. Clamp source bitrate into a
/// sane window — we never spend > 8 Mbps on a transcode (modest CPU
/// budget) and never less than 500 kbps so low-bitrate sources still
/// look watchable post-transcode.
const HLS_MIN_BITRATE_BPS: u64 = 500_000;
const HLS_MAX_BITRATE_BPS: u64 = 8_000_000;

fn target_video_bitrate(source: Option<u64>) -> u64 {
    source
        .unwrap_or(HLS_MAX_BITRATE_BPS)
        .clamp(HLS_MIN_BITRATE_BPS, HLS_MAX_BITRATE_BPS)
}

async fn master_playlist(
    state: web::Data<AppState>,
    _user: AuthUser,
    req: HttpRequest,
    path: web::Path<String>,
) -> Result<impl Responder, actix_web::Error> {
    let id = path.into_inner();
    let item = load_hls_item(&state, &id).await?;
    // Bandwidth advertised in the master = encoder cap + a small
    // overhead for audio (128 kbps fits AAC LC + segment framing).
    let bandwidth = target_video_bitrate(item.source_bitrate_bps) + 128_000;
    let resolution = match (item.width, item.height) {
        (Some(w), Some(h)) => format!(",RESOLUTION={w}x{h}"),
        _ => String::new(),
    };
    let body = format!(
        "#EXTM3U\n#EXT-X-VERSION:3\n\
         #EXT-X-STREAM-INF:BANDWIDTH={bandwidth},CODECS=\"avc1.640028,mp4a.40.2\"{resolution}\n\
         /Videos/{id}/main.m3u8?{}\n",
        playback_qs(&req)
    );
    Ok(HttpResponse::Ok()
        .content_type("application/vnd.apple.mpegurl")
        .body(body))
}

async fn variant_playlist(
    state: web::Data<AppState>,
    _user: AuthUser,
    req: HttpRequest,
    path: web::Path<String>,
) -> Result<impl Responder, actix_web::Error> {
    let id = path.into_inner();
    let item = load_hls_item(&state, &id).await?;
    let duration = item.duration_seconds;
    let segment_count = (duration / SEGMENT_SECONDS).ceil() as u32;
    let segment_count = segment_count.max(1);
    let qs = playback_qs(&req);
    let mut body = String::with_capacity(256 + segment_count as usize * 80);
    body.push_str("#EXTM3U\n");
    body.push_str("#EXT-X-VERSION:3\n");
    body.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");
    body.push_str(&format!(
        "#EXT-X-TARGETDURATION:{}\n",
        SEGMENT_SECONDS as u32
    ));
    body.push_str("#EXT-X-MEDIA-SEQUENCE:0\n");
    for seg in 0..segment_count {
        let remaining = duration - (seg as f64 * SEGMENT_SECONDS);
        let len = remaining.clamp(0.01, SEGMENT_SECONDS);
        body.push_str(&format!("#EXTINF:{len:.3},\n"));
        // Lowercase: T31 routes are registered lowercase; emit the
        // canonical form so HLS players don't pay a middleware rewrite
        // for every segment.
        body.push_str(&format!("/videos/{id}/hls1/main/{seg}.ts?{qs}\n"));
    }
    body.push_str("#EXT-X-ENDLIST\n");
    Ok(HttpResponse::Ok()
        .content_type("application/vnd.apple.mpegurl")
        .body(body))
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SegmentQuery {
    #[serde(default)]
    play_session_id: Option<String>,
}

async fn segment(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<(String, u32)>,
    q: web::Query<SegmentQuery>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id, seg) = path.into_inner();
    let id_num: u64 = id
        .parse()
        .map_err(|_| error::ErrorBadRequest("invalid id"))?;
    let item = state.stores.get(id_num).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("not found"),
        other => error::ErrorInternalServerError(other.to_string()),
    })?;

    let start_ticks =
        (seg as u64).saturating_mul(SEGMENT_SECONDS as u64) * TICKS_PER_SECOND;
    let duration_ticks = (SEGMENT_SECONDS * TICKS_PER_SECOND as f64) as u64;

    let opts = build_segment_opts(&state, q.play_session_id.as_deref(), &item, start_ticks, duration_ticks)
        .await;

    // T42: when an HLS cache is wired, route through it. Otherwise
    // fall back to live transcoding (every request spawns ffmpeg).
    if let Some(cache) = state.hls.as_ref() {
        let bytes = cache
            .segment_bytes(id_num, seg, &item.path, &opts)
            .await
            .map_err(|e| error::ErrorInternalServerError(format!("segment cache: {e}")))?;
        return Ok(HttpResponse::Ok()
            .content_type(opts.container.content_type())
            .body(bytes));
    }

    let transcoder = FfmpegTranscoder::new();
    let stream = transcoder
        .transcode(&item.path, &opts)
        .await
        .map_err(|e| error::ErrorInternalServerError(format!("transcode: {e}")))?;
    Ok(HttpResponse::Ok()
        .content_type(opts.container.content_type())
        .streaming(stream.into_stream()))
}

/// Resolve the per-segment `TranscodeOptions` for this request.
///
/// When the play session was registered by `playback_info` (the
/// common path — jellyfin-web POSTs PlaybackInfo before requesting
/// segments and embeds `PlaySessionId` in every subsequent URL), we
/// honour the negotiator's `Decision::Transcode` — target container,
/// video codec, audio codec, and the negotiated max-video-bitrate
/// cap. Falls back to H264 + AAC + TS with a probe-driven bitrate
/// cap when the session is missing (jellyfin clients that go
/// straight at /master.m3u8 without a PlaySessionId — rare but
/// possible).
async fn build_segment_opts(
    state: &AppState,
    play_session_id: Option<&str>,
    item: &pharos_core::MediaItem,
    start_ticks: u64,
    duration_ticks: u64,
) -> TranscodeOptions {
    use crate::api::jellyfin::device_profile::Decision;

    let cached = match play_session_id {
        Some(id) => state.transcode_sessions.get(id).await.ok().flatten(),
        None => None,
    };

    if let Some(session) = cached {
        if let Decision::Transcode {
            target_container,
            target_video_codec,
            target_audio_codec,
            max_video_bitrate_bps,
        } = session.decision
        {
            let container = Container::from_name(&target_container).unwrap_or(Container::Mpegts);
            let video = target_video_codec
                .as_deref()
                .and_then(VideoCodec::from_name)
                .or(Some(VideoCodec::H264));
            let audio = target_audio_codec
                .as_deref()
                .and_then(AudioCodec::from_name)
                .or(Some(AudioCodec::Aac));
            return TranscodeOptions {
                container,
                video,
                audio,
                video_bitrate_bps: Some(
                    max_video_bitrate_bps
                        .map(|cap| cap.min(HLS_MAX_BITRATE_BPS))
                        .unwrap_or_else(|| target_video_bitrate(item.probe.bitrate_bps)),
                ),
                audio_bitrate_bps: Some(128_000),
                start_position_ticks: start_ticks,
                duration_ticks: Some(duration_ticks),
            };
        }
    }

    // Fallback path: no session registered → conservative defaults.
    TranscodeOptions {
        container: Container::Mpegts,
        video: Some(VideoCodec::H264),
        audio: Some(AudioCodec::Aac),
        video_bitrate_bps: Some(target_video_bitrate(item.probe.bitrate_bps)),
        audio_bitrate_bps: Some(128_000),
        start_position_ticks: start_ticks,
        duration_ticks: Some(duration_ticks),
    }
}

/// Produce `api_key=…&PlaySessionId=…` query string from the incoming
/// request so the embedded segment URLs carry forward the bearer
/// token *and* the play-session id (segment handler needs both:
/// auth + the cached transcode `Decision`).
fn playback_qs(req: &HttpRequest) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(t) = extract_token(req) {
        parts.push(format!("api_key={t}"));
    }
    for kv in req.query_string().split('&') {
        if let Some((k, v)) = kv.split_once('=') {
            if k.eq_ignore_ascii_case("PlaySessionId") && !v.is_empty() {
                parts.push(format!("PlaySessionId={v}"));
            }
        }
    }
    parts.join("&")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use actix_web::test;
    use actix_web::App;
    use pharos_core::{
        MediaItem, MediaKind, MediaStore, SecretString, TokenStore, UserId, UserPolicy,
        UserRecord, UserStore,
    };
    use crate::auth::BuiltinAuth;
    use pharos_store_sqlx::sqlite::SqliteStore;

    async fn seed() -> (web::Data<AppState>, String) {
        let stores = SqliteStore::connect("sqlite::memory:").await.unwrap();
        let auth = BuiltinAuth::new(stores.clone());
        let hash = auth.hash_password(&SecretString::new("p")).unwrap();
        let uid = UserId::new();
        stores
            .create(UserRecord {
                id: uid,
                name: "u".into(),
                password_hash: hash,
                policy: UserPolicy::default(),
            })
            .await
            .unwrap();
        let token = stores.issue(uid, "t").await.unwrap();
        stores
            .put(MediaItem {
                id: 7,
                path: "/nonexistent.mkv".into(),
                title: "m".into(),
                kind: MediaKind::Movie,
                ..Default::default()
            })
            .await
            .unwrap();
        let state = web::Data::new(AppState::new(stores, "t".into()));
        (state, token.0.expose().to_string())
    }

    #[actix_web::test]
    async fn master_playlist_requires_auth() {
        let (state, _t) = seed().await;
        let app = test::init_service(App::new().app_data(state).configure(register)).await;
        let req = test::TestRequest::get()
            .uri("/videos/7/master.m3u8")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 401);
    }

    async fn seed_with_probe(
        probe: pharos_core::MediaProbe,
    ) -> (web::Data<AppState>, String) {
        let stores = SqliteStore::connect("sqlite::memory:").await.unwrap();
        let auth = BuiltinAuth::new(stores.clone());
        let hash = auth.hash_password(&SecretString::new("p")).unwrap();
        let uid = UserId::new();
        stores
            .create(UserRecord {
                id: uid,
                name: "u".into(),
                password_hash: hash,
                policy: UserPolicy::default(),
            })
            .await
            .unwrap();
        let token = stores.issue(uid, "t").await.unwrap();
        stores
            .put(MediaItem {
                id: 9,
                path: "/nonexistent.mkv".into(),
                title: "m".into(),
                kind: MediaKind::Movie,
                probe,
                series: None,
                created_at: None,
            })
            .await
            .unwrap();
        let state = web::Data::new(AppState::new(stores, "t".into()));
        (state, token.0.expose().to_string())
    }

    #[actix_web::test]
    async fn master_playlist_uses_real_resolution_and_bitrate_from_probe() {
        let probe = pharos_core::MediaProbe {
            duration_ms: Some(10_000),
            width: Some(1280),
            height: Some(720),
            bitrate_bps: Some(1_500_000),
            ..Default::default()
        };
        let (state, token) = seed_with_probe(probe).await;
        let app = test::init_service(App::new().app_data(state).configure(register)).await;
        let req = test::TestRequest::get()
            .uri(&format!("/videos/9/master.m3u8?api_key={token}"))
            .to_request();
        let body = test::call_and_read_body(&app, req).await;
        let s = std::str::from_utf8(&body).unwrap();
        assert!(s.contains("RESOLUTION=1280x720"), "{s}");
        // 1.5 Mbps source + 128 kbps audio overhead = 1_628_000.
        assert!(s.contains("BANDWIDTH=1628000"), "{s}");
    }

    #[::core::prelude::v1::test]
    fn target_video_bitrate_clamps_into_window() {
        assert_eq!(target_video_bitrate(Some(100_000)), HLS_MIN_BITRATE_BPS);
        assert_eq!(target_video_bitrate(Some(20_000_000)), HLS_MAX_BITRATE_BPS);
        assert_eq!(target_video_bitrate(Some(2_500_000)), 2_500_000);
        assert_eq!(target_video_bitrate(None), HLS_MAX_BITRATE_BPS);
    }
}
