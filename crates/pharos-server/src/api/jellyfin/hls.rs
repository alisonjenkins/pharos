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
        )
        // P8 — per-subtitle-track HLS playlist (referenced by the
        // master playlist's `EXT-X-MEDIA` URI). Returns a single-
        // segment VOD m3u8 pointing at the existing VTT extractor.
        .route(
            "/videos/{id}/subtitles/{idx}.m3u8",
            web::get().to(subtitle_playlist),
        );
}

async fn subtitle_playlist(
    state: web::Data<AppState>,
    _user: AuthUser,
    req: HttpRequest,
    path: web::Path<(String, u32)>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id, idx) = path.into_inner();
    let item = load_hls_item(&state, &id).await?;
    let qs = playback_qs(&req);
    let duration_secs = item.duration_seconds.max(0.0);
    // VTT served by the existing subtitle handler. `0` stands in for
    // the mediaSourceId (subtitle endpoint accepts the short form
    // too, but the canonical wire shape keeps the segment count = 1).
    let mut body = String::with_capacity(256);
    body.push_str("#EXTM3U\n");
    body.push_str("#EXT-X-VERSION:3\n");
    body.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");
    body.push_str(&format!(
        "#EXT-X-TARGETDURATION:{}\n",
        duration_secs.ceil() as u64
    ));
    body.push_str("#EXT-X-MEDIA-SEQUENCE:0\n");
    body.push_str(&format!("#EXTINF:{duration_secs:.3},\n"));
    body.push_str(&format!("/videos/{id}/0/subtitles/{idx}/stream.vtt?{qs}\n"));
    body.push_str("#EXT-X-ENDLIST\n");
    Ok(HttpResponse::Ok()
        .content_type("application/vnd.apple.mpegurl")
        .body(body))
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
    /// Returns the variants ≤ the source height AND ≤ the session's
    /// negotiated bitrate cap (P2). Always includes the smallest rung
    /// so a low-resolution / heavily-throttled source still ladders
    /// down to something a phone can play on a poor link.
    fn ladder_for(source_height: Option<u32>, bitrate_cap_bps: Option<u64>) -> Vec<Self> {
        let max_h = source_height.unwrap_or(1080);
        let max_bps = bitrate_cap_bps.unwrap_or(u64::MAX);
        let mut v: Vec<Self> = [Self::P1080, Self::P720, Self::P480, Self::P360]
            .iter()
            .copied()
            .filter(|x| x.height() <= max_h && x.video_bitrate_bps() <= max_bps)
            .collect();
        if v.is_empty() {
            v.push(Self::P360);
        }
        v
    }
}

/// W3 — audio-only ladder for `MediaKind::Audio`. Bitrates in AAC kbps;
/// each rung becomes one EXT-X-STREAM-INF entry without a RESOLUTION
/// token (audio-only HLS spec).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioVariant {
    A256,
    A192,
    A128,
    A96,
    A64,
}

impl AudioVariant {
    const fn name(self) -> &'static str {
        match self {
            Self::A256 => "256k",
            Self::A192 => "192k",
            Self::A128 => "128k",
            Self::A96 => "96k",
            Self::A64 => "64k",
        }
    }
    const fn audio_bitrate_bps(self) -> u64 {
        match self {
            Self::A256 => 256_000,
            Self::A192 => 192_000,
            Self::A128 => 128_000,
            Self::A96 => 96_000,
            Self::A64 => 64_000,
        }
    }
    fn from_name(s: &str) -> Option<Self> {
        match s {
            "256k" => Some(Self::A256),
            "192k" => Some(Self::A192),
            "128k" => Some(Self::A128),
            "96k" => Some(Self::A96),
            "64k" => Some(Self::A64),
            _ => None,
        }
    }
    /// Audio ladder ≤ source bitrate AND ≤ session cap. Always
    /// includes 64k so a tethered phone gets something.
    fn ladder_for(source_bitrate_bps: Option<u64>, cap_bps: Option<u64>) -> Vec<Self> {
        let max_src = source_bitrate_bps.unwrap_or(u64::MAX);
        let max_cap = cap_bps.unwrap_or(u64::MAX);
        let mut v: Vec<Self> = [Self::A256, Self::A192, Self::A128, Self::A96, Self::A64]
            .iter()
            .copied()
            .filter(|x| x.audio_bitrate_bps() <= max_src && x.audio_bitrate_bps() <= max_cap)
            .collect();
        if v.is_empty() {
            v.push(Self::A64);
        }
        v
    }
}

/// Either a video Variant or an AudioVariant — drives the named
/// variant routes (`/variants/{name}.m3u8`, `/hls1/{name}/{seg}.ts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnyVariant {
    Video(Variant),
    Audio(AudioVariant),
}

impl AnyVariant {
    fn from_name(s: &str) -> Option<Self> {
        Variant::from_name(s)
            .map(Self::Video)
            .or_else(|| AudioVariant::from_name(s).map(Self::Audio))
    }
}

/// RFC 6381 CODECS attribute string for `EXT-X-STREAM-INF`. Returns a
/// best-effort token combining the resolved video + audio codecs;
/// falls back to `avc1.640028,mp4a.40.2` (H.264 High@4.0 + AAC LC)
/// when probe data is missing AND the transcode target is unknown.
fn codecs_string(
    video_codec: Option<&str>,
    video_profile: Option<&str>,
    video_level: Option<u32>,
    audio_codec: Option<&str>,
) -> String {
    let video = video_codec.map(|c| video_codec_token(c, video_profile, video_level));
    let audio = audio_codec.map(audio_codec_token);
    match (video, audio) {
        (Some(v), Some(a)) => format!("{v},{a}"),
        (Some(v), None) => v,
        (None, Some(a)) => a,
        (None, None) => "avc1.640028,mp4a.40.2".to_string(),
    }
}

fn video_codec_token(codec: &str, profile: Option<&str>, level: Option<u32>) -> String {
    match codec.to_ascii_lowercase().as_str() {
        "h264" | "avc" | "avc1" => {
            // RFC 6381 avc1.PPCCLL where PP = profile_idc, CC =
            // constraint set flags, LL = level_idc — all hex bytes.
            let (profile_idc, constraints) = avc_profile_idc(profile);
            let level_idc = level.unwrap_or(40) & 0xFF; // 40 = level 4.0
            format!("avc1.{profile_idc:02x}{constraints:02x}{level_idc:02x}")
        }
        "hevc" | "h265" | "hvc1" | "hev1" => {
            // hvc1.<profile>.<flags>.L<level>.B0 — Main profile, Main
            // tier, generic_profile_idc only; full HVCC parsing is
            // future work.
            let prof = match profile.unwrap_or("Main").to_ascii_lowercase().as_str() {
                p if p.contains("main 10") => 2,
                p if p.contains("rext") => 4,
                _ => 1,
            };
            let level_idc = level.unwrap_or(120); // 120 = level 4.0
            format!("hvc1.{prof}.4.L{level_idc}.B0")
        }
        "vp9" => {
            // RFC 7741 vp09.<profile>.<level>.<bitdepth>. Use sane
            // defaults: profile 0, level 4.1, 8-bit.
            let prof: u32 = profile
                .and_then(|p| p.strip_prefix("Profile "))
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            "vp09.".to_string() + &format!("{prof:02}.41.08")
        }
        "av1" => "av01.0.04M.08".to_string(),
        other => other.to_string(),
    }
}

fn avc_profile_idc(profile: Option<&str>) -> (u8, u8) {
    // (profile_idc, constraint_set_flags). Conservative defaults: the
    // most-compatible profile (Constrained Baseline) when unknown.
    match profile.unwrap_or("").to_ascii_lowercase().as_str() {
        p if p.contains("high 10") => (0x6E, 0x00),
        p if p.contains("high 4:2:2") => (0x7A, 0x00),
        p if p.contains("high 4:4:4") => (0xF4, 0x00),
        p if p.contains("high") => (0x64, 0x00),
        p if p.contains("main") => (0x4D, 0x40),
        p if p.contains("extended") => (0x58, 0x00),
        p if p.contains("constrained baseline") => (0x42, 0xE0),
        p if p.contains("baseline") => (0x42, 0x00),
        _ => (0x64, 0x00), // default: High@<level>
    }
}

fn audio_codec_token(codec: &str) -> String {
    match codec.to_ascii_lowercase().as_str() {
        "aac" => "mp4a.40.2".to_string(), // AAC LC
        "he-aac" | "aac_he" => "mp4a.40.5".to_string(),
        "mp3" => "mp4a.40.34".to_string(),
        "opus" => "opus".to_string(),
        "vorbis" => "vorbis".to_string(),
        "ac3" => "ac-3".to_string(),
        "eac3" => "ec-3".to_string(),
        "flac" => "flac".to_string(),
        other => other.to_string(),
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
    /// MediaProbe-derived codec metadata; piped into CODECS attribute
    /// on master + variant playlists so client CanPlayDecision sees
    /// the truth instead of `avc1.640028,mp4a.40.2`.
    video_codec: Option<String>,
    video_profile: Option<String>,
    video_level: Option<u32>,
    audio_codec: Option<String>,
    kind: pharos_core::MediaKind,
    /// P8 — embedded subtitle tracks surfaced as `EXT-X-MEDIA` lines
    /// on the master playlist so HLS clients render a track selector.
    subtitle_tracks: Vec<pharos_core::SubtitleTrack>,
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
        video_codec: item.probe.video_codec.clone(),
        video_profile: item.probe.video_profile.clone(),
        video_level: item.probe.video_level,
        audio_codec: item.probe.audio_codec.clone(),
        kind: item.kind,
        subtitle_tracks: item.probe.subtitle_tracks.clone(),
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
    let bitrate_cap = extract_session_bitrate_cap(&state, &req).await;

    // P1 — real CODECS from probe, falling back to the hardcoded
    // string only when no probe data is available.
    let codecs = codecs_string(
        item.video_codec.as_deref(),
        item.video_profile.as_deref(),
        item.video_level,
        item.audio_codec.as_deref(),
    );

    let mut body = String::new();
    body.push_str("#EXTM3U\n#EXT-X-VERSION:3\n");

    // P8 — softsub. Emit EXT-X-MEDIA per subtitle track so HLS
    // clients render a subtitle selector instead of forcing burn-in.
    let has_subs =
        !item.subtitle_tracks.is_empty() && !matches!(item.kind, pharos_core::MediaKind::Audio);
    if has_subs {
        for (i, track) in item.subtitle_tracks.iter().enumerate() {
            let name = subtitle_display_name(track, i);
            let lang = track.language.as_deref().unwrap_or("und");
            let default = if track.is_default { "YES" } else { "NO" };
            let forced = if track.is_forced { "YES" } else { "NO" };
            body.push_str(&format!(
                "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"{name}\",\
                 LANGUAGE=\"{lang}\",DEFAULT={default},FORCED={forced},AUTOSELECT=YES,\
                 URI=\"/videos/{id}/subtitles/{idx}.m3u8?{qs}\"\n",
                idx = track.stream_index,
            ));
        }
    }

    if matches!(item.kind, pharos_core::MediaKind::Audio) {
        // P3 — audio-only HLS: no RESOLUTION token, audio CODECS only.
        let audio_codec_token = item
            .audio_codec
            .as_deref()
            .map(audio_codec_token)
            .unwrap_or_else(|| "mp4a.40.2".to_string());
        // Baseline (legacy `main` variant) — single-rung audio.
        let baseline_bw = target_video_bitrate(item.source_bitrate_bps) + 128_000;
        body.push_str(&format!(
            "#EXT-X-STREAM-INF:BANDWIDTH={baseline_bw},CODECS=\"{audio_codec_token}\"\n\
             /Videos/{id}/main.m3u8?{qs}\n"
        ));
        for av in AudioVariant::ladder_for(item.source_bitrate_bps, bitrate_cap) {
            body.push_str(&format!(
                "#EXT-X-STREAM-INF:BANDWIDTH={bw},CODECS=\"mp4a.40.2\"\n\
                 /Videos/{id}/variants/{name}.m3u8?{qs}\n",
                bw = av.audio_bitrate_bps(),
                name = av.name(),
            ));
        }
        return Ok(HttpResponse::Ok()
            .content_type("application/vnd.apple.mpegurl")
            .body(body));
    }

    let ladder = Variant::ladder_for(item.height, bitrate_cap);

    // Compute the aspect-ratio-preserving render width for each
    // variant's `RESOLUTION=` hint. Falls back to omitting
    // RESOLUTION when the source dimensions weren't probed.
    let aspect_w = item
        .width
        .zip(item.height)
        .map(|(w, h)| w as f64 / h as f64);

    // Backwards-compat single-variant entry. Older players that
    // ignore EXT-X-STREAM-INF iteration still pick the first one.
    let baseline_bw = target_video_bitrate(item.source_bitrate_bps) + 128_000;
    let baseline_res = match (item.width, item.height) {
        (Some(w), Some(h)) => format!(",RESOLUTION={w}x{h}"),
        _ => String::new(),
    };
    let sub_attr = if has_subs { ",SUBTITLES=\"subs\"" } else { "" };
    body.push_str(&format!(
        "#EXT-X-STREAM-INF:BANDWIDTH={baseline_bw},CODECS=\"{codecs}\"{baseline_res}{sub_attr}\n\
         /Videos/{id}/main.m3u8?{qs}\n"
    ));

    // W3 — quality ladder filtered by session cap (P2).
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
            "#EXT-X-STREAM-INF:BANDWIDTH={bw},CODECS=\"{codecs}\"{resolution}{sub_attr}\n\
             /Videos/{id}/variants/{name}.m3u8?{qs}\n",
            bw = v.advertised_bandwidth(),
            name = v.name(),
        ));
    }
    Ok(HttpResponse::Ok()
        .content_type("application/vnd.apple.mpegurl")
        .body(body))
}

/// Render a human-readable label for a subtitle track. Falls back to
/// `Track {idx}` when neither title nor language is present.
fn subtitle_display_name(track: &pharos_core::SubtitleTrack, idx: usize) -> String {
    if let Some(title) = track.title.as_ref().filter(|s| !s.is_empty()) {
        return title.clone();
    }
    if let Some(lang) = track.language.as_ref().filter(|s| !s.is_empty()) {
        return lang.to_ascii_uppercase();
    }
    format!("Track {}", idx + 1)
}

/// Pull the negotiated `max_video_bitrate_bps` from the transcode
/// session if a PlaySessionId is present and the session is alive.
/// Returns `None` when no PSID was supplied OR the session has no
/// bitrate cap recorded.
async fn extract_session_bitrate_cap(state: &AppState, req: &HttpRequest) -> Option<u64> {
    use crate::api::jellyfin::device_profile::Decision;
    let psid = req
        .query_string()
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| k.eq_ignore_ascii_case("PlaySessionId"))
        .map(|(_, v)| v.to_string())?;
    let session = state.transcode_sessions.get(&psid).await.ok().flatten()?;
    match session.decision {
        Decision::Transcode {
            max_video_bitrate_bps,
            ..
        } => max_video_bitrate_bps,
        _ => None,
    }
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
    if AnyVariant::from_name(&variant).is_none() {
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
    let av =
        AnyVariant::from_name(&variant).ok_or_else(|| error::ErrorNotFound("unknown variant"))?;
    serve_segment(state, user, id, seg, Some(av), q).await
}

async fn serve_segment(
    state: web::Data<AppState>,
    _user: AuthUser,
    id: String,
    seg: u32,
    variant: Option<AnyVariant>,
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

    // P2 — pull negotiated bitrate cap from the live session (if any)
    // so we can clamp the variant override below.
    let session_cap = session.as_ref().and_then(|s| match &s.decision {
        crate::api::jellyfin::device_profile::Decision::Transcode {
            max_video_bitrate_bps,
            ..
        } => *max_video_bitrate_bps,
        _ => None,
    });

    let mut opts = build_segment_opts(
        session,
        &item,
        start_ticks,
        duration_ticks,
        q.audio_stream_index,
        q.subtitle_stream_index,
    );
    // W3 — variant overrides the video-bitrate cap negotiated by
    // PlaybackInfo. P2 — clamp the override against the session cap so
    // a 4 Mbps 1080p rung never outruns a 1 Mbps mobile profile.
    // P3 — audio variants override the audio cap, skip video.
    if let Some(v) = variant {
        match v {
            AnyVariant::Video(vv) => {
                let target = match session_cap {
                    Some(cap) => vv.video_bitrate_bps().min(cap),
                    None => vv.video_bitrate_bps(),
                };
                opts.video_bitrate_bps = Some(target);
            }
            AnyVariant::Audio(av) => {
                opts.audio_bitrate_bps = Some(av.audio_bitrate_bps());
                opts.video = None;
                opts.video_bitrate_bps = None;
                // Drop subtitle burn-in: makes no sense in an audio-
                // only stream.
                opts.burn_subtitle_stream_index = None;
            }
        }
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
        match session.decision {
            Decision::Transcode {
                target_container,
                target_video_codec,
                target_audio_codec,
                max_video_bitrate_bps,
            } => {
                let container =
                    Container::from_name(&target_container).unwrap_or(Container::Mpegts);
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
            // P9 — VideoRemux: copy video, transcode audio (or copy
            // if codec already matches). Container always swaps to
            // the profile target. Burn-in stripped (copy path can't
            // filter).
            Decision::VideoRemux {
                target_container,
                target_audio_codec,
            } => {
                let container =
                    Container::from_name(&target_container).unwrap_or(Container::Mpegts);
                let audio = target_audio_codec
                    .as_deref()
                    .and_then(AudioCodec::from_name)
                    .or(Some(AudioCodec::Aac));
                return TranscodeOptions {
                    container,
                    video: Some(VideoCodec::Copy),
                    audio,
                    video_bitrate_bps: None,
                    audio_bitrate_bps: Some(128_000),
                    start_position_ticks: start_ticks,
                    duration_ticks: Some(duration_ticks),
                    audio_source_stream_index: audio_stream_index,
                    burn_subtitle_stream_index: None,
                };
            }
            _ => {}
        }
    }

    // Fallback path: no session registered → conservative defaults.
    // P6 — copy the video bitstream when the source codec is already
    // mpegts-compatible (h264 / hevc). Saves a full re-encode for
    // clients that bypass PlaybackInfo (rare but real). Other codecs
    // (vp9 / av1 / mpeg2) still re-encode to H.264 for safety.
    let (video, video_bitrate_bps) = match item.probe.video_codec.as_deref() {
        Some(c) if matches!(c.to_ascii_lowercase().as_str(), "h264" | "hevc" | "h265") => {
            // Copy + no -b:v cap (bitstream copies passthrough source bitrate).
            // Subtitle burn-in is incompatible with `-c:v copy`, so the
            // burn-in arg gets stripped in `build_args` already; we just
            // don't pass a per-stream subtitle index.
            (Some(VideoCodec::Copy), None)
        }
        _ => (
            Some(VideoCodec::H264),
            Some(target_video_bitrate(item.probe.bitrate_bps)),
        ),
    };
    // When we copy video, burn-in is impossible (would require
    // re-encode). Drop subtitle_stream_index in that case so the
    // transcoder doesn't error.
    let burn_subtitle_stream_index = if matches!(video, Some(VideoCodec::Copy)) {
        None
    } else {
        subtitle_stream_index
    };
    TranscodeOptions {
        container: Container::Mpegts,
        video,
        audio: Some(AudioCodec::Aac),
        video_bitrate_bps,
        audio_bitrate_bps: Some(128_000),
        start_position_ticks: start_ticks,
        duration_ticks: Some(duration_ticks),
        audio_source_stream_index: audio_stream_index,
        burn_subtitle_stream_index,
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
        let v = Variant::ladder_for(Some(2160), None);
        assert_eq!(v.len(), 4);
        assert!(v.contains(&Variant::P1080));

        // 720p source → 720p + 480p + 360p only.
        let v = Variant::ladder_for(Some(720), None);
        assert_eq!(v.len(), 3);
        assert!(!v.contains(&Variant::P1080));
        assert!(v.contains(&Variant::P720));

        // 240p source — no variant matches; ladder still includes 360p
        // so a phone has *something* to play.
        let v = Variant::ladder_for(Some(240), None);
        assert_eq!(v, vec![Variant::P360]);

        // Unknown height — assume 1080p.
        let v = Variant::ladder_for(None, None);
        assert_eq!(v.len(), 4);
    }

    #[::core::prelude::v1::test]
    fn variant_ladder_drops_rungs_above_session_cap() {
        // 1 Mbps cap → only 360p (800k) qualifies; 480p (1.5 Mbps),
        // 720p (3 Mbps), 1080p (5 Mbps) all drop.
        let v = Variant::ladder_for(Some(2160), Some(1_000_000));
        assert_eq!(v, vec![Variant::P360]);

        // 3 Mbps cap admits 720p + 480p + 360p (all ≤ 3 Mbps).
        let v = Variant::ladder_for(Some(2160), Some(3_000_000));
        assert_eq!(v.len(), 3);
        assert!(!v.contains(&Variant::P1080));
        assert!(v.contains(&Variant::P720));

        // No cap = full ladder.
        let v = Variant::ladder_for(Some(2160), None);
        assert_eq!(v.len(), 4);
    }

    #[::core::prelude::v1::test]
    fn audio_variant_ladder_filters_against_source_and_cap() {
        // 320 kbps source + no cap → 256 / 192 / 128 / 96 / 64.
        let v = AudioVariant::ladder_for(Some(320_000), None);
        assert_eq!(v.len(), 5);

        // 100 kbps cap → only 96k + 64k.
        let v = AudioVariant::ladder_for(Some(320_000), Some(100_000));
        assert_eq!(v, vec![AudioVariant::A96, AudioVariant::A64]);

        // 50 kbps source → no rung qualifies, fallback to 64k.
        let v = AudioVariant::ladder_for(Some(50_000), None);
        assert_eq!(v, vec![AudioVariant::A64]);
    }

    #[::core::prelude::v1::test]
    fn audio_variant_name_roundtrip() {
        for av in [
            AudioVariant::A256,
            AudioVariant::A192,
            AudioVariant::A128,
            AudioVariant::A96,
            AudioVariant::A64,
        ] {
            assert_eq!(AudioVariant::from_name(av.name()), Some(av));
        }
        assert_eq!(AudioVariant::from_name("nope"), None);
    }

    #[::core::prelude::v1::test]
    fn codecs_string_emits_rfc6381_for_common_codecs() {
        // H.264 High@4.0 + AAC LC.
        assert_eq!(
            codecs_string(Some("h264"), Some("High"), Some(40), Some("aac")),
            "avc1.640028,mp4a.40.2"
        );
        // VP9 Profile 0 + Opus.
        assert_eq!(
            codecs_string(Some("vp9"), Some("Profile 0"), None, Some("opus")),
            "vp09.00.41.08,opus"
        );
        // HEVC Main 10.
        let s = codecs_string(Some("hevc"), Some("Main 10"), Some(150), None);
        assert!(s.starts_with("hvc1.2.4.L150"), "{s}");
        // Audio-only fallback (no video codec).
        assert_eq!(codecs_string(None, None, None, Some("mp3")), "mp4a.40.34");
        // No probe data at all → backward-compat fallback.
        assert_eq!(
            codecs_string(None, None, None, None),
            "avc1.640028,mp4a.40.2"
        );
    }

    #[::core::prelude::v1::test]
    fn variant_name_roundtrip() {
        for v in [Variant::P1080, Variant::P720, Variant::P480, Variant::P360] {
            assert_eq!(Variant::from_name(v.name()), Some(v));
        }
        assert_eq!(Variant::from_name("nope"), None);
    }

    fn item_with_video_codec(codec: Option<&str>) -> pharos_core::MediaItem {
        pharos_core::MediaItem {
            id: 1,
            path: "/x".into(),
            title: "t".into(),
            kind: MediaKind::Movie,
            probe: pharos_core::MediaProbe {
                duration_ms: Some(60_000),
                width: Some(1920),
                height: Some(1080),
                bitrate_bps: Some(4_000_000),
                video_codec: codec.map(|s| s.to_string()),
                ..Default::default()
            },
            series: None,
            created_at: None,
        }
    }

    #[::core::prelude::v1::test]
    fn fallback_keeps_h264_transcode_for_unknown_codec() {
        // VP9 / AV1 / mpeg2 etc. → safe H.264 re-encode.
        let item = item_with_video_codec(Some("vp9"));
        let opts = build_segment_opts(None, &item, 0, 60_000_000, None, None);
        assert!(matches!(
            opts.video,
            Some(pharos_transcode::VideoCodec::H264)
        ));
        assert!(opts.video_bitrate_bps.is_some());
    }

    #[::core::prelude::v1::test]
    fn fallback_emits_copy_for_h264_source() {
        // P6 — h264 in mpegts container needs no re-encode.
        let item = item_with_video_codec(Some("h264"));
        let opts = build_segment_opts(None, &item, 0, 60_000_000, None, None);
        assert!(matches!(
            opts.video,
            Some(pharos_transcode::VideoCodec::Copy)
        ));
        // Copy = no -b:v cap (passthrough source bitrate).
        assert!(opts.video_bitrate_bps.is_none());
    }

    #[::core::prelude::v1::test]
    fn fallback_emits_copy_for_hevc_source() {
        for codec in ["hevc", "h265", "HEVC", "Hevc"] {
            let item = item_with_video_codec(Some(codec));
            let opts = build_segment_opts(None, &item, 0, 60_000_000, None, None);
            assert!(
                matches!(opts.video, Some(pharos_transcode::VideoCodec::Copy)),
                "codec {codec} did not copy",
            );
        }
    }

    #[::core::prelude::v1::test]
    fn fallback_strips_subtitle_burn_in_when_copying_video() {
        // Burn-in needs re-encode; with `-c:v copy` it has to be a no-op.
        let item = item_with_video_codec(Some("hevc"));
        let opts = build_segment_opts(None, &item, 0, 60_000_000, None, Some(2));
        assert!(opts.burn_subtitle_stream_index.is_none());
    }

    #[::core::prelude::v1::test]
    fn fallback_keeps_subtitle_burn_in_when_transcoding() {
        // Re-encode path retains the requested burn-in index.
        let item = item_with_video_codec(Some("vp9"));
        let opts = build_segment_opts(None, &item, 0, 60_000_000, None, Some(2));
        assert_eq!(opts.burn_subtitle_stream_index, Some(2));
    }

    #[::core::prelude::v1::test]
    fn fallback_falls_back_to_h264_when_probe_has_no_video_codec() {
        // Defensive: a probe row predating the codec migration shows
        // no video codec; we must still pick a working target.
        let item = item_with_video_codec(None);
        let opts = build_segment_opts(None, &item, 0, 60_000_000, None, None);
        assert!(matches!(
            opts.video,
            Some(pharos_transcode::VideoCodec::H264)
        ));
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
