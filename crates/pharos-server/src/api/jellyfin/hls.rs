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
use pharos_transcode::{AudioCodec, Container, FfmpegTranscoder, TranscodeOptions, VideoCodec};

/// Segment length in seconds. 6 s matches Apple's HLS authoring spec
/// recommendation and what most clients ask for; Jellyfin's own
/// default is the same.
const SEGMENT_SECONDS: f64 = 6.0;
const TICKS_PER_SECOND: u64 = 10_000_000;

pub fn register(cfg: &mut web::ServiceConfig) {
    // T31: lowercase canonical paths; `LowercasePath` middleware
    // rewrites the PascalCase URIs the streamer emits before routing.
    cfg.route("/videos/{id}/master.m3u8", web::get().to(master_playlist))
        // Backwards-compat single-variant path. Players that hit
        // `/main.m3u8` directly (bypassing the master) still work.
        .route(
            "/videos/{id}/main.m3u8",
            web::get().to(variant_playlist_main),
        )
        .route(
            "/videos/{id}/hls1/main/{seg}.ts",
            web::get().to(segment_main),
        )
        // W3 — per-variant playlist + segments. `{variant}` resolves
        // to one of `Variant::ALL` names; unknown values 404.
        .route(
            "/videos/{id}/variants/{variant}.m3u8",
            web::get().to(variant_playlist_named),
        )
        .route(
            "/videos/{id}/hls1/{variant}/{seg}.ts",
            web::get().to(segment_named),
        );
}

/// W3 — quality ladder. Each variant maps to a `(name, height,
/// video_bitrate, audio_bitrate)` tuple driving the master playlist
/// entries and per-segment encoder caps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Variant {
    P1080,
    P720,
    P480,
    P360,
}

impl Variant {
    const fn name(self) -> &'static str {
        match self {
            Self::P1080 => "1080p",
            Self::P720 => "720p",
            Self::P480 => "480p",
            Self::P360 => "360p",
        }
    }
    const fn height(self) -> u32 {
        match self {
            Self::P1080 => 1080,
            Self::P720 => 720,
            Self::P480 => 480,
            Self::P360 => 360,
        }
    }
    /// Video-bitrate cap in bps. Picked to match commonly-quoted
    /// streaming ladder values (1080p ≈ 5 Mbps, 720p ≈ 3 Mbps).
    const fn video_bitrate_bps(self) -> u64 {
        match self {
            Self::P1080 => 5_000_000,
            Self::P720 => 3_000_000,
            Self::P480 => 1_500_000,
            Self::P360 => 800_000,
        }
    }
    /// Aggregate bandwidth advertised in `EXT-X-STREAM-INF`. Video
    /// cap + a 128 kbps AAC audio overhead matches what jellyfin-web
    /// inspects for its quality picker.
    const fn advertised_bandwidth(self) -> u64 {
        self.video_bitrate_bps() + 128_000
    }
    fn from_name(s: &str) -> Option<Self> {
        match s {
            "1080p" => Some(Self::P1080),
            "720p" => Some(Self::P720),
            "480p" => Some(Self::P480),
            "360p" => Some(Self::P360),
            _ => None,
        }
    }
    /// Returns the variants ≤ the source height. Always includes the
    /// smallest one so a low-resolution source still ladders down to
    /// something a phone can play on a poor link.
    fn ladder_for(source_height: Option<u32>) -> Vec<Self> {
        let max = source_height.unwrap_or(1080);
        let mut v: Vec<Self> = [Self::P1080, Self::P720, Self::P480, Self::P360]
            .iter()
            .copied()
            .filter(|x| x.height() <= max)
            .collect();
        if v.is_empty() {
            v.push(Self::P360);
        }
        v
    }
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
    let qs = playback_qs(&req);
    let ladder = Variant::ladder_for(item.height);

    // Compute the aspect-ratio-preserving render width for each
    // variant's `RESOLUTION=` hint. Falls back to omitting
    // RESOLUTION when the source dimensions weren't probed.
    let aspect_w = item
        .width
        .zip(item.height)
        .map(|(w, h)| w as f64 / h as f64);

    let mut body = String::new();
    body.push_str("#EXTM3U\n#EXT-X-VERSION:3\n");

    // Backwards-compat single-variant entry. Older players that
    // ignore EXT-X-STREAM-INF iteration still pick the first one.
    let baseline_bw = target_video_bitrate(item.source_bitrate_bps) + 128_000;
    let baseline_res = match (item.width, item.height) {
        (Some(w), Some(h)) => format!(",RESOLUTION={w}x{h}"),
        _ => String::new(),
    };
    body.push_str(&format!(
        "#EXT-X-STREAM-INF:BANDWIDTH={baseline_bw},CODECS=\"avc1.640028,mp4a.40.2\"{baseline_res}\n\
         /Videos/{id}/main.m3u8?{qs}\n"
    ));

    // W3 — quality ladder. Emit one EXT-X-STREAM-INF per variant
    // that fits the source resolution.
    for v in ladder {
        let target_h = v.height();
        let resolution = match aspect_w {
            Some(ratio) => {
                let target_w = (ratio * target_h as f64).round() as u32 & !1; // even width
                format!(",RESOLUTION={target_w}x{target_h}")
            }
            None => String::new(),
        };
        body.push_str(&format!(
            "#EXT-X-STREAM-INF:BANDWIDTH={bw},CODECS=\"avc1.640028,mp4a.40.2\"{resolution}\n\
             /Videos/{id}/variants/{name}.m3u8?{qs}\n",
            bw = v.advertised_bandwidth(),
            name = v.name(),
        ));
    }
    Ok(HttpResponse::Ok()
        .content_type("application/vnd.apple.mpegurl")
        .body(body))
}

async fn variant_playlist_main(
    state: web::Data<AppState>,
    user: AuthUser,
    req: HttpRequest,
    path: web::Path<String>,
) -> Result<impl Responder, actix_web::Error> {
    let id = path.into_inner();
    render_variant_playlist(state, user, req, id, "main").await
}

async fn variant_playlist_named(
    state: web::Data<AppState>,
    user: AuthUser,
    req: HttpRequest,
    path: web::Path<(String, String)>,
) -> Result<impl Responder, actix_web::Error> {
    let (id, variant) = path.into_inner();
    if Variant::from_name(&variant).is_none() {
        return Err(error::ErrorNotFound("unknown variant"));
    }
    render_variant_playlist(state, user, req, id, &variant).await
}

async fn render_variant_playlist(
    state: web::Data<AppState>,
    _user: AuthUser,
    req: HttpRequest,
    id: String,
    variant: &str,
) -> Result<impl Responder, actix_web::Error> {
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
        body.push_str(&format!("/videos/{id}/hls1/{variant}/{seg}.ts?{qs}\n"));
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
    /// Source-relative audio-stream index. None = ffmpeg default
    /// (first audio stream). When the client switches tracks via the
    /// player's audio dropdown, the URL gains `AudioStreamIndex=N`.
    /// Server-cached segments are keyed per-index so different tracks
    /// don't clobber each other.
    #[serde(default)]
    audio_stream_index: Option<u32>,
    /// Source-relative subtitle-stream index for burn-in. None = no
    /// subtitle overlay. Jellyfin convention emits `-1` for "off" —
    /// the deserializer treats negative values as None.
    #[serde(default, deserialize_with = "deserialize_subtitle_index")]
    subtitle_stream_index: Option<u32>,
}

/// Treat `-1` (Jellyfin's "off" sentinel) as `None`. Anything <0
/// collapses to None; non-negative integers pass through.
fn deserialize_subtitle_index<'de, D>(d: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    let raw: Option<i64> = Option::deserialize(d)?;
    Ok(raw.and_then(|n| if n >= 0 { Some(n as u32) } else { None }))
}

async fn segment_main(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<(String, u32)>,
    q: web::Query<SegmentQuery>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id, seg) = path.into_inner();
    serve_segment(state, user, id, seg, None, q).await
}

async fn segment_named(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<(String, String, u32)>,
    q: web::Query<SegmentQuery>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id, variant, seg) = path.into_inner();
    let v = Variant::from_name(&variant).ok_or_else(|| error::ErrorNotFound("unknown variant"))?;
    serve_segment(state, user, id, seg, Some(v), q).await
}

async fn serve_segment(
    state: web::Data<AppState>,
    _user: AuthUser,
    id: String,
    seg: u32,
    variant: Option<Variant>,
    q: web::Query<SegmentQuery>,
) -> Result<HttpResponse, actix_web::Error> {
    let id_num: u64 = id
        .parse()
        .map_err(|_| error::ErrorBadRequest("invalid id"))?;
    let item = state.stores.get(id_num).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("not found"),
        other => error::ErrorInternalServerError(other.to_string()),
    })?;

    // W4 — PlaySessionId enforcement. When the client embeds a
    // PlaySessionId in the segment URL (the common path — jellyfin-web
    // and mobile both do) the matching session in the registry must
    // still be alive. A 410 fires when the session has been GC'd or
    // explicitly removed via /Sessions/Playing/Stopped — stops a
    // stale client from draining cached segments minted under an
    // invalidated session. Requests without a PlaySessionId fall
    // through to the conservative-defaults path (legacy clients that
    // hit master.m3u8 cold).
    let session = if let Some(psid) = q.play_session_id.as_deref() {
        match state.transcode_sessions.get(psid).await {
            Ok(Some(s)) => Some(s),
            Ok(None) => return Err(error::ErrorGone("play session expired")),
            Err(e) => {
                return Err(error::ErrorInternalServerError(format!(
                    "transcode session lookup: {e}"
                )));
            }
        }
    } else {
        None
    };

    let start_ticks = (seg as u64).saturating_mul(SEGMENT_SECONDS as u64) * TICKS_PER_SECOND;
    let duration_ticks = (SEGMENT_SECONDS * TICKS_PER_SECOND as f64) as u64;

    let mut opts = build_segment_opts(
        session,
        &item,
        start_ticks,
        duration_ticks,
        q.audio_stream_index,
        q.subtitle_stream_index,
    );
    // W3 — variant overrides the video-bitrate cap negotiated by
    // PlaybackInfo. Keeps the audio cap + codecs untouched so a
    // shared TranscodeSession answer drives every variant ladder.
    if let Some(v) = variant {
        opts.video_bitrate_bps = Some(v.video_bitrate_bps());
    }

    // T42: when an HLS cache is wired, route through it. Otherwise
    // fall back to live transcoding (every request spawns ffmpeg).
    if let Some(cache) = state.hls.as_ref() {
        let bytes = cache
            .segment_bytes_keyed(
                id_num,
                seg,
                opts.audio_source_stream_index,
                opts.burn_subtitle_stream_index,
                &item.path,
                &opts,
            )
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
fn build_segment_opts(
    session: Option<crate::transcode_sessions::TranscodeSession>,
    item: &pharos_core::MediaItem,
    start_ticks: u64,
    duration_ticks: u64,
    audio_stream_index: Option<u32>,
    subtitle_stream_index: Option<u32>,
) -> TranscodeOptions {
    use crate::api::jellyfin::device_profile::Decision;

    if let Some(session) = session {
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
                audio_source_stream_index: audio_stream_index,
                burn_subtitle_stream_index: subtitle_stream_index,
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
        audio_source_stream_index: audio_stream_index,
        burn_subtitle_stream_index: subtitle_stream_index,
    }
}

/// Produce the query string each embedded segment URL needs.
/// Carries forward the bearer token (`api_key`), the play-session id,
/// and any client-supplied per-stream picks (`AudioStreamIndex`,
/// `SubtitleStreamIndex`) so the segment handler resolves the right
/// `TranscodeOptions` without a server-side state lookup.
fn playback_qs(req: &HttpRequest) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(t) = extract_token(req) {
        parts.push(format!("api_key={t}"));
    }
    for kv in req.query_string().split('&') {
        if let Some((k, v)) = kv.split_once('=') {
            if v.is_empty() {
                continue;
            }
            // Forward case-insensitively but emit canonical case.
            if k.eq_ignore_ascii_case("PlaySessionId") {
                parts.push(format!("PlaySessionId={v}"));
            } else if k.eq_ignore_ascii_case("AudioStreamIndex") {
                parts.push(format!("AudioStreamIndex={v}"));
            } else if k.eq_ignore_ascii_case("SubtitleStreamIndex") {
                parts.push(format!("SubtitleStreamIndex={v}"));
            }
        }
    }
    parts.join("&")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::auth::BuiltinAuth;
    use actix_web::test;
    use actix_web::App;
    use pharos_core::{
        MediaItem, MediaKind, MediaStore, SecretString, TokenStore, UserId, UserPolicy, UserRecord,
        UserStore,
    };
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

    async fn seed_with_probe(probe: pharos_core::MediaProbe) -> (web::Data<AppState>, String) {
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

    #[::core::prelude::v1::test]
    fn variant_ladder_filters_to_source_height() {
        // 4K source → every variant up to 1080p (current cap).
        let v = Variant::ladder_for(Some(2160));
        assert_eq!(v.len(), 4);
        assert!(v.contains(&Variant::P1080));

        // 720p source → 720p + 480p + 360p only.
        let v = Variant::ladder_for(Some(720));
        assert_eq!(v.len(), 3);
        assert!(!v.contains(&Variant::P1080));
        assert!(v.contains(&Variant::P720));

        // 240p source — no variant matches; ladder still includes 360p
        // so a phone has *something* to play.
        let v = Variant::ladder_for(Some(240));
        assert_eq!(v, vec![Variant::P360]);

        // Unknown height — assume 1080p.
        let v = Variant::ladder_for(None);
        assert_eq!(v.len(), 4);
    }

    #[::core::prelude::v1::test]
    fn variant_name_roundtrip() {
        for v in [Variant::P1080, Variant::P720, Variant::P480, Variant::P360] {
            assert_eq!(Variant::from_name(v.name()), Some(v));
        }
        assert_eq!(Variant::from_name("nope"), None);
    }

    #[actix_web::test]
    async fn master_playlist_lists_each_variant_below_source_height() {
        let probe = pharos_core::MediaProbe {
            duration_ms: Some(10_000),
            width: Some(1920),
            height: Some(1080),
            bitrate_bps: Some(5_000_000),
            ..Default::default()
        };
        let (state, token) = seed_with_probe(probe).await;
        let app = test::init_service(App::new().app_data(state).configure(register)).await;
        let req = test::TestRequest::get()
            .uri(&format!("/videos/9/master.m3u8?api_key={token}"))
            .to_request();
        let body = test::call_and_read_body(&app, req).await;
        let s = std::str::from_utf8(&body).unwrap();
        // Baseline "main" entry retained.
        assert!(s.contains("/Videos/9/main.m3u8"), "{s}");
        // Each ladder rung renders a STREAM-INF + variant URL.
        for name in ["1080p", "720p", "480p", "360p"] {
            assert!(
                s.contains(&format!("/Videos/9/variants/{name}.m3u8")),
                "{s}"
            );
        }
        // 720p variant advertises its bitrate.
        assert!(s.contains("BANDWIDTH=3128000"), "{s}");
    }

    #[actix_web::test]
    async fn variant_playlist_unknown_name_404s() {
        let (state, token) = seed().await;
        let app = test::init_service(App::new().app_data(state).configure(register)).await;
        let req = test::TestRequest::get()
            .uri(&format!("/videos/7/variants/8k.m3u8?api_key={token}"))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 404);
    }

    #[actix_web::test]
    async fn named_variant_playlist_routes_segments_to_named_path() {
        let probe = pharos_core::MediaProbe {
            duration_ms: Some(6_000),
            width: Some(1280),
            height: Some(720),
            bitrate_bps: Some(3_000_000),
            ..Default::default()
        };
        let (state, token) = seed_with_probe(probe).await;
        let app = test::init_service(App::new().app_data(state).configure(register)).await;
        let req = test::TestRequest::get()
            .uri(&format!("/videos/9/variants/720p.m3u8?api_key={token}"))
            .to_request();
        let body = test::call_and_read_body(&app, req).await;
        let s = std::str::from_utf8(&body).unwrap();
        assert!(s.contains("/videos/9/hls1/720p/0.ts"), "{s}");
    }
}
