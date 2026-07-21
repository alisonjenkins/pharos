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

use crate::api::jellyfin::ci_query::CiQuery;
use crate::{
    api::jellyfin::auth_extractor::{extract_token, AuthUser},
    api::jellyfin::fmp4,
    state::AppState,
};
use actix_web::{error, web, HttpRequest, HttpResponse, Responder};
use pharos_core::{MediaStore, Prober};
use pharos_scanner::FfmpegProber;
use pharos_transcode::{
    AudioCodec, Container, FfmpegTranscoder, SegmentAudio, SegmentContainer, SegmentOpts,
    SegmentVideo, VideoCodec,
};

/// Segment length in seconds. 6 s matches Apple's HLS authoring spec
/// recommendation and what most clients ask for; Jellyfin's own
/// default is the same.
const SEGMENT_SECONDS: f64 = 6.0;

use pharos_core::time::Ticks;

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
        )
        // VP9-in-fMP4 HLS. The H.264/MPEG-TS ladder above cannot serve
        // Firefox/Zen (no H.264 in MSE), so those clients get a VP9 fMP4
        // HLS stream instead of a progressive WebM — HLS restores seeking,
        // resume, and track-switching. Segments are self-contained
        // fragmented mp4 the `fmp4` module splits into a shared init +
        // tfdt-corrected media (see that module + `serve_vp9_segment`).
        .route("/videos/{id}/vp9/master.m3u8", web::get().to(vp9_master))
        .route("/videos/{id}/vp9/main.m3u8", web::get().to(vp9_variant))
        .route("/videos/{id}/vp9/init.mp4", web::get().to(vp9_init))
        // Continuous-audio rendition (A/V-sync fix): a separate HLS audio
        // group backed by ONE ffmpeg session (see hls_cache::ensure_audio_hls).
        // Video segments are audio-free; the player syncs the two by PTS.
        .route(
            "/videos/{id}/vp9/audio.m3u8",
            web::get().to(vp9_audio_playlist),
        )
        .route(
            "/videos/{id}/vp9/audio/{name}",
            web::get().to(vp9_audio_file),
        )
        .route("/videos/{id}/vp9/{seg}.m4s", web::get().to(vp9_segment))
        // `DELETE /Videos/ActiveEncodings` — jellyfin-web calls
        // `apiClient.stopActiveEncodings(playSessionId)` as the FIRST step of a
        // mid-playback audio/subtitle/quality switch (its `changeStream` tears
        // down the old transcode before requesting a new PlaybackInfo). Vanilla
        // Jellyfin answers 204; pharos returned 404 (route absent), and
        // jellyfin-web's switch promise chain is UNGUARDED (no `.catch`), so the
        // rejection killed the whole switch — the new PlaybackInfo never fired
        // and the audio never changed (only a full stop+resume worked). This is
        // exactly why switching worked on real Jellyfin but not pharos.
        .route(
            "/videos/activeencodings",
            web::delete().to(stop_active_encodings),
        );
}

/// `DELETE /Videos/ActiveEncodings?deviceId=…&PlaySessionId=…`. Stop the named
/// play session's transcode so a client switching tracks can start a fresh
/// one. pharos transcodes per-segment on demand (no long-lived encoder), so
/// "stopping" just drops the session — subsequent segment requests under it
/// 410 (see `check_session`) and the client is about to open a new session
/// anyway. Always 204, matching Jellyfin (jellyfin-web ignores the body).
async fn stop_active_encodings(
    state: web::Data<AppState>,
    _user: AuthUser,
    req: HttpRequest,
) -> HttpResponse {
    // PlaySessionId is a query param; match case-insensitively (clients send
    // `PlaySessionId`, some `playSessionId`).
    let psid = req.query_string().split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        k.eq_ignore_ascii_case("playsessionid")
            .then(|| v.to_string())
    });
    if let Some(psid) = psid {
        if let Err(e) = state.transcode_sessions.remove(&psid).await {
            tracing::warn!(error = %e, psid, "stop_active_encodings: session remove failed");
        }
    }
    HttpResponse::NoContent().finish()
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
        .insert_header(playlist_cache_control(false))
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

/// The codecs the HLS **segments** actually carry — the transcode OUTPUT,
/// which is what the master playlist's `CODECS` attribute must advertise.
/// `/master.m3u8` is always a transcode: EVERY source is re-encoded to
/// H.264 + AAC (B45 — `build_segment_opts` never stream-copies on the
/// segmented surface). Advertising the *source* codec (e.g. `mpeg4` for a
/// legacy AVI/DivX/Xvid rip, or `hvc1` for an HEVC source) makes the
/// browser's `MediaSource.isTypeSupported` reject the stream as unplayable
/// *before* it fetches a single segment — surfacing as jellyfin-web's
/// "Playback Error".
fn hls_output_codecs_string(
    _src_video: Option<&str>,
    _src_profile: Option<&str>,
    _src_level: Option<u32>,
) -> String {
    // Re-encoded to H.264 (libx264 defaults). Advertise a conservative
    // avc1 token; the source codec/profile/level describe the discarded
    // input, never the segment bytes.
    codecs_string(Some("h264"), None, None, Some("aac"))
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
    /// Source fps × 1000. Drives frame-aligned segment boundaries so the
    /// playlist's per-segment EXTINF matches the transcoder's actual (frame-
    /// snapped) cut points — otherwise a fixed 6.0 EXTINF drifts against the
    /// real video timeline on a non-integer-fps source.
    frame_rate_mille: Option<u32>,
    kind: pharos_core::MediaKind,
}

async fn load_hls_item(state: &AppState, id_str: &str) -> Result<HlsItem, actix_web::Error> {
    let id: u64 = pharos_jellyfin_api::dto::parse_item_id(id_str)
        .ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
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
        frame_rate_mille: item.probe.frame_rate_mille,
        kind: item.kind,
    })
}

/// Pick the bitrate we cap the encoder at. Clamp source bitrate into a
/// sane window — we never spend > 8 Mbps on a transcode (modest CPU
/// budget) and never less than 500 kbps so low-bitrate sources still
/// look watchable post-transcode.
const HLS_MIN_BITRATE_BPS: u64 = 500_000;
// The ceiling honours the client's negotiated MaxStreamingBitrate up to a
// bound the CPU-only box can actually ENCODE in realtime.
//
// B50 raised this 8M→40M to end VP9 graininess — but 40M is unencodable in
// realtime by the software VP9 encoder: a 1080p segment took 32s (vs the 6s
// budget), freezing playback (Supergirl VP9, 2026-07-14). The B50 benchmark
// used synthetic noise, which encodes far faster than real film detail.
// B52 caps at 12M: a real improvement over the old 8M (measured grain win),
// still within the ~1.5-2x realtime margin observed for 1080p VP9 on this
// box, and `effective_video_bitrate` bounds it by the SOURCE bitrate so a
// low-bitrate source (most content) is never wastefully over-encoded.
// x264/VAAPI could sustain more, but one ceiling keeps the negotiation
// simple and VP9 (desktop-Linux Firefox, B43) is the realtime-tightest path.
const HLS_MAX_BITRATE_BPS: u64 = 12_000_000;

fn target_video_bitrate(source: Option<u64>) -> u64 {
    source
        .unwrap_or(HLS_MAX_BITRATE_BPS)
        .clamp(HLS_MIN_BITRATE_BPS, HLS_MAX_BITRATE_BPS)
}

/// B50 — the video bitrate a transcoded segment actually targets. Honours
/// the client's negotiated cap (its quality-picker choice), but never
/// exceeds the SOURCE bitrate (re-encoding above source wastes CPU +
/// bandwidth for zero quality gain — and VP9/x264 are at least as efficient
/// as the source codec, so they need no more than it) nor the HLS ceiling.
/// `None` cap → the source-derived target; `None` source → the cap/ceiling.
fn effective_video_bitrate(negotiated_cap: Option<u64>, source: Option<u64>) -> u64 {
    let want = negotiated_cap
        .unwrap_or(HLS_MAX_BITRATE_BPS)
        .min(HLS_MAX_BITRATE_BPS);
    let bounded = match source {
        Some(src) => want.min(src),
        None => want,
    };
    bounded.clamp(HLS_MIN_BITRATE_BPS, HLS_MAX_BITRATE_BPS)
}

/// Min of two optional caps treating `None` as "no constraint". Used to fold
/// the live-session cap together with a URL-carried `VideoBitrate` ceiling.
fn min_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (some, None) | (None, some) => some,
    }
}

/// Fix (Lace incident): parse a `VideoBitrate=` ceiling from a request query
/// string. PlaybackInfo bakes it into the transcode URL for remote clients and
/// `playback_qs` rides it master → variant → segment, so the cap survives a
/// GC'd session and reaches the remux re-encode paths (which carry no cap in
/// the registered Decision). `0` / unparseable → `None`.
fn qs_video_bitrate_cap(qs: &str) -> Option<u64> {
    qs.split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| k.eq_ignore_ascii_case("VideoBitrate"))
        .and_then(|(_, v)| v.parse::<u64>().ok())
        .filter(|n| *n > 0)
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
    // Fold the live-session cap together with any URL-carried `VideoBitrate`
    // ceiling (Lace incident) so both the ladder filter AND the advertised
    // `main` bandwidth reflect a remote client's connection ceiling.
    let bitrate_cap = min_opt(
        extract_session_bitrate_cap(&state, &req).await,
        qs_video_bitrate_cap(req.query_string()),
    );

    // Advertise the codecs the transcoded segments actually carry (H.264 +
    // AAC, or a stream-copied h264/hevc source) — NOT the raw source codec.
    // A legacy mpeg4/DivX source re-encodes to H.264, so advertising `mpeg4`
    // made the browser reject the stream before fetching a segment.
    let codecs = hls_output_codecs_string(
        item.video_codec.as_deref(),
        item.video_profile.as_deref(),
        item.video_level,
    );

    let mut body = String::new();
    body.push_str("#EXTM3U\n#EXT-X-VERSION:3\n");
    // P18 — Safari refuses to seek on a master playlist without this
    // tag. Asserting independent segments is true for h264/hevc HLS
    // (each segment starts on an IDR / SPS boundary).
    body.push_str("#EXT-X-INDEPENDENT-SEGMENTS\n");

    // Text subtitles are delivered as an External rendition via PlaybackInfo
    // (jellyfin-web renders them — SubtitlesOctopus for ASS / cue JSON). We do
    // NOT advertise an in-manifest EXT-X-MEDIA:TYPE=SUBTITLES rendition: hls.js
    // would render it as a second, WebVTT-flattened copy on top of the External
    // one ("subtitle shown twice"). Image subs still burn into the transcode.

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
            .insert_header(playlist_cache_control(true))
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
    // ignore EXT-X-STREAM-INF iteration still pick the first one. Advertise the
    // CAP-bounded bitrate (Lace incident) so hls.js ABR doesn't overshoot a
    // remote client's ceiling when it picks this `main` rung.
    let baseline_bw = effective_video_bitrate(bitrate_cap, item.source_bitrate_bps) + 128_000;
    let baseline_res = match (item.width, item.height) {
        (Some(w), Some(h)) => format!(",RESOLUTION={w}x{h}"),
        _ => String::new(),
    };
    body.push_str(&format!(
        "#EXT-X-STREAM-INF:BANDWIDTH={baseline_bw},CODECS=\"{codecs}\"{baseline_res}\n\
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
            "#EXT-X-STREAM-INF:BANDWIDTH={bw},CODECS=\"{codecs}\"{resolution}\n\
             /Videos/{id}/variants/{name}.m3u8?{qs}\n",
            bw = v.advertised_bandwidth(),
            name = v.name(),
        ));
    }
    Ok(HttpResponse::Ok()
        .content_type("application/vnd.apple.mpegurl")
        .insert_header(playlist_cache_control(true))
        .body(body))
}

/// Cache-Control for HLS playlists. The playlist *body* embeds the
/// caller's bearer token (`api_key=…` on every segment/sub URL), so it
/// is per-user secret and MUST NOT be stored by any shared cache/CDN/
/// proxy — `public` previously let a shared cache serve user A's token
/// to user B (token leak). `no-store` keeps the token out of all caches;
/// playlists are cheap to regenerate per request.
fn playlist_cache_control(_is_master: bool) -> (actix_web::http::header::HeaderName, &'static str) {
    (actix_web::http::header::CACHE_CONTROL, "no-store")
}

/// P18 — query-string-only parser for `StartTimeTicks`, mirroring
/// the stream.rs helper. Pulled out so both modules don't depend on
/// the actix HttpRequest type for a simple ticks lookup.
fn parse_start_time_ticks_qs(qs: &str) -> u64 {
    for kv in qs.split('&') {
        if let Some((k, v)) = kv.split_once('=') {
            if k.eq_ignore_ascii_case("StartTimeTicks") {
                return v.parse::<u64>().unwrap_or(0);
            }
        }
    }
    0
}

/// Render a human-readable label for a subtitle track. Falls back to
/// `Track {idx}` when neither title nor language is present.
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
    body.push_str("#EXT-X-INDEPENDENT-SEGMENTS\n");
    body.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");
    body.push_str(&format!(
        "#EXT-X-TARGETDURATION:{}\n",
        SEGMENT_SECONDS as u32
    ));
    // P18 — resume hint. When the client embedded `StartTimeTicks`
    // in the playlist URL, advertise the offset so the player jumps
    // straight there instead of scanning from segment 0.
    let start_ticks = parse_start_time_ticks_qs(req.query_string());
    if start_ticks > 0 {
        let secs = Ticks(start_ticks).seconds();
        body.push_str(&format!("#EXT-X-START:TIME-OFFSET={secs:.3},PRECISE=YES\n"));
    }
    body.push_str("#EXT-X-MEDIA-SEQUENCE:0\n");
    for seg in 0..segment_count {
        // Frame-aligned duration matching the transcoder's actual cut points,
        // clamped by the remaining media at the tail.
        let (start_secs, dur_secs) = segment_time_range(seg, item.frame_rate_mille);
        let remaining = (duration - start_secs).max(0.01);
        let len = dur_secs.min(remaining);
        body.push_str(&format!("#EXTINF:{len:.3},\n"));
        // Lowercase: T31 routes are registered lowercase; emit the
        // canonical form so HLS players don't pay a middleware rewrite
        // for every segment.
        body.push_str(&format!("/videos/{id}/hls1/{variant}/{seg}.ts?{qs}\n"));
    }
    body.push_str("#EXT-X-ENDLIST\n");
    Ok(HttpResponse::Ok()
        .content_type("application/vnd.apple.mpegurl")
        .insert_header(playlist_cache_control(false))
        .body(body))
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
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
    req: HttpRequest,
    path: web::Path<(String, u32)>,
    q: CiQuery<SegmentQuery>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id, seg) = path.into_inner();
    serve_segment(state, user, req, id, seg, None, q).await
}

async fn segment_named(
    state: web::Data<AppState>,
    user: AuthUser,
    req: HttpRequest,
    path: web::Path<(String, String, u32)>,
    q: CiQuery<SegmentQuery>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id, variant, seg) = path.into_inner();
    let av =
        AnyVariant::from_name(&variant).ok_or_else(|| error::ErrorNotFound("unknown variant"))?;
    serve_segment(state, user, req, id, seg, Some(av), q).await
}

async fn serve_segment(
    state: web::Data<AppState>,
    _user: AuthUser,
    req: HttpRequest,
    id: String,
    seg: u32,
    variant: Option<AnyVariant>,
    q: CiQuery<SegmentQuery>,
) -> Result<HttpResponse, actix_web::Error> {
    let id_num: u64 = pharos_jellyfin_api::dto::parse_item_id(&id)
        .ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
    // A client is actively pulling segments → tell the background backfill to
    // stand down so its whole-file decodes don't starve live transcoding.
    state.note_playback_activity();
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

    // Bounds-check the requested segment against the VOD grid the playlist
    // enumerated (`ceil(duration/6)`). An over-index — a stale playlist, a
    // client bug, or a probe duration that overshoots the real media — must be
    // a clean 404, not a 500 from an empty transcode or an empty-tail segment
    // cached and served forever as a 200. Only bound when the duration is known
    // (the common case); a legacy row without a probed duration keeps the old
    // permissive behaviour rather than risk rejecting a valid segment.
    if let Some(dur_ms) = item.probe.duration_ms {
        let grid =
            super::seek::SegmentGrid::new(dur_ms as f64 / 1000.0, item.probe.frame_rate_mille);
        if grid.checked(seg).is_none() {
            return Err(error::ErrorNotFound("segment index past end of media"));
        }
    }

    // Frame-aligned boundaries keep audio + video locked across independent
    // per-segment transcodes (see `segment_start_secs`).
    let (start_secs, dur_secs) = segment_time_range(seg, item.probe.frame_rate_mille);
    let start_ticks = Ticks::from_seconds(start_secs).0;
    let duration_ticks = Ticks::from_seconds(dur_secs).0;

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
    // Fix (Lace incident): apply the URL-carried `VideoBitrate` ceiling as a
    // hard min over whatever the session / variant / no-session fallback chose.
    // Monotone (only ever lowers), so it caps the `main` rendition on a GC'd
    // session and the remux re-encode paths — which carry no cap in the
    // registered Decision — without disturbing the normal session-cap flow.
    if let (Some(cap), Some(cur)) = (
        qs_video_bitrate_cap(req.query_string()),
        opts.video_bitrate_bps,
    ) {
        opts.video_bitrate_bps = Some(cur.min(cap).clamp(HLS_MIN_BITRATE_BPS, HLS_MAX_BITRATE_BPS));
    }

    // B51 — the client's ACTUAL subtitle pick (codec-relative, pre-gate), so
    // the prefetch caches the exact variant the client will request for
    // upcoming segments (not this segment's gated value).
    let wanted_burn = opts.burn_subtitle_stream_index;
    // B46 — strip the burn for segments the subtitle track provably leaves
    // empty. MUST run before the T87 hint, the ETag, the prefetch AND the
    // cache read: all of them key on the (post-gating) burn index.
    gate_image_sub_burn(&state, &item, &mut opts, start_secs, dur_secs).await;
    // Burn a TEXT/ASS sub from the small cached `.ass` sidecar + fontsdir, not
    // the whole source per segment. Resolved paths propagate into the prefetch
    // clones below.
    resolve_text_burn_assets(&state, &item, &mut opts).await;

    // T87 — remember this play session's exact variant for SyncPlay seek
    // prewarming (same as the VP9 path).
    if let Some(psid) = q.play_session_id.as_deref() {
        state.note_segment_opts(psid, id_num, &opts);
    }
    // P18 — stable ETag derived from cache key inputs. Same
    // `(media_id, seg, audio_idx, sub_idx, bitrate)` tuple drives the
    // disk cache, so the ETag implicitly invalidates whenever the
    // cached bytes would.
    let etag = segment_etag(
        id_num,
        seg,
        opts.audio_source_stream_index,
        opts.burn_subtitle_stream_index,
        opts.video_bitrate_bps,
        opts.video.map(|c| c.ffmpeg_codec()).unwrap_or("none"),
    );

    // 304 short-circuit: matched If-None-Match → no body, no ffmpeg.
    if let Some(inm) = req
        .headers()
        .get(actix_web::http::header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    {
        if inm.split(',').any(|t| t.trim() == etag) {
            return Ok(HttpResponse::NotModified()
                .insert_header((actix_web::http::header::ETAG, etag.as_str()))
                .insert_header((
                    actix_web::http::header::CACHE_CONTROL,
                    "public, max-age=31536000, immutable",
                ))
                .finish());
        }
    }

    // T42: when an HLS cache is wired, route through it. Otherwise
    // fall back to live transcoding (every request spawns ffmpeg).
    if let Some(cache) = state.hls.as_ref() {
        // Warm the next few segments so a fast / >1x client doesn't stall on
        // on-demand transcode (spawned before this segment's own read so they
        // pipeline across the CPU pool).
        spawn_segment_prefetch(&state, &item, seg, &opts, wanted_burn);
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
            .insert_header((actix_web::http::header::ETAG, etag.as_str()))
            .insert_header((
                actix_web::http::header::CACHE_CONTROL,
                "public, max-age=31536000, immutable",
            ))
            .body(bytes));
    }

    // Uncached live path. Prefer the load-balancing scheduler (spreads
    // across every GPU + CPU, crash-isolated worker) when available;
    // fall back to a direct inline ffmpeg otherwise.
    if let Some(sched) = state.transcode_scheduler.as_ref() {
        match sched
            .submit_live(item.path.clone(), opts.to_transcode_options())
            .await
        {
            Ok(stream) => {
                return Ok(HttpResponse::Ok()
                    .content_type(opts.container.content_type())
                    .insert_header((actix_web::http::header::ETAG, etag.as_str()))
                    .insert_header((
                        actix_web::http::header::CACHE_CONTROL,
                        "public, max-age=31536000, immutable",
                    ))
                    .streaming(stream));
            }
            Err(e) => {
                // Busy / worker error — fall through to inline ffmpeg so
                // the request still succeeds.
                tracing::warn!(error = %e, "scheduler live transcode failed; inline fallback");
            }
        }
    }

    let transcoder = FfmpegTranscoder::new();
    let stream = transcoder
        .transcode(&item.path, &opts.to_transcode_options())
        .await
        .map_err(|e| error::ErrorInternalServerError(format!("transcode: {e}")))?;
    Ok(HttpResponse::Ok()
        .content_type(opts.container.content_type())
        .insert_header((actix_web::http::header::ETAG, etag.as_str()))
        .insert_header((
            actix_web::http::header::CACHE_CONTROL,
            "public, max-age=31536000, immutable",
        ))
        .streaming(stream.into_stream()))
}

/// P18 — stable weak-ETag string for a segment. Encodes every
/// dimension that drives the disk-cache filename so mutating any of
/// them produces a different ETag.
fn segment_etag(
    media_id: u64,
    seg: u32,
    audio_idx: Option<u32>,
    sub_idx: Option<u32>,
    bitrate: Option<u64>,
    video_codec: &str,
) -> String {
    use xxhash_rust::xxh3::xxh3_64;
    let key = format!(
        "{media_id}-{seg}-{audio}-{sub}-{br}-{video_codec}",
        audio = audio_idx.map_or_else(|| "d".to_string(), |n| n.to_string()),
        sub = sub_idx.map_or_else(|| "off".to_string(), |n| n.to_string()),
        br = bitrate.map_or_else(|| "auto".to_string(), |b| b.to_string()),
    );
    let h = xxh3_64(key.as_bytes()) & 0x7FFFFFFFFFFFFFFF;
    format!("W/\"seg-{h:016x}\"")
}

/// Resolve the per-segment [`SegmentOpts`] for this request.
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
) -> SegmentOpts {
    use crate::api::jellyfin::device_profile::Decision;

    // `AudioStreamIndex` / `SubtitleStreamIndex` arrive as ABSOLUTE ffprobe
    // stream indices (jellyfin-web's convention), but the encoder selects by
    // per-CODEC index (`-map 0:a:N`, subtitle-filter `si=N`). Convert via each
    // track's position among its own codec's streams — identical to the VP9
    // path (`vp9_segment_opts`). Passing the absolute index straight through
    // (the previous behaviour) picked the wrong audio/subtitle whenever the
    // wanted stream wasn't the first of its codec — e.g. a file with subtitle
    // streams interleaved before the second audio track. `None` (unknown
    // index) falls back to ffmpeg's default selection rather than mis-mapping.
    let audio_stream_index = audio_stream_index.and_then(|abs| {
        codec_relative_index(item.probe.audio_tracks.iter().map(|t| t.stream_index), abs)
    });
    // Task 6 — burn either IMAGE subtitles (PGS/VOBSUB, unchanged) OR a
    // TEXT/ASS subtitle the client explicitly asked to burn (Tasks 4/5 only
    // forward a text index here for burn-required clients; the default path
    // still delivers text subs out-of-band as a separate External
    // rendition). `burn_subtitle_is_text` tells Task 7's transcoder which
    // ffmpeg filter graph to build.
    let mut subtitle_is_text = false;
    let subtitle_stream_index = subtitle_stream_index.and_then(|abs| {
        let codec = item
            .probe
            .subtitle_tracks
            .iter()
            .find(|t| t.stream_index == abs)
            .map(|t| t.codec.clone().unwrap_or_default());
        let is_image = codec
            .as_deref()
            .map(super::subtitles::is_image_subtitle_codec)
            .unwrap_or(false);
        let is_text = codec
            .as_deref()
            .map(|c| pharos_jellyfin_api::dto::is_text_subtitle_codec(Some(c)))
            .unwrap_or(false);
        if !is_image && !is_text {
            return None;
        }
        subtitle_is_text = is_text;
        codec_relative_index(
            item.probe.subtitle_tracks.iter().map(|t| t.stream_index),
            abs,
        )
    });

    if let Some(session) = session {
        match session.decision {
            Decision::Transcode {
                target_container,
                target_video_codec,
                target_audio_codec,
                max_video_bitrate_bps,
            } => {
                // The /hls1/*.ts segment route always serves mpegts H.264 for
                // a video item — pharos has no VP9/AV1 encoder and the segment
                // Content-Type (video/mp2t) + master-playlist codecs assume
                // mpegts H.264. A client profile that nominally asked for e.g.
                // mp4/vp9 is ignored here (hls.js demuxes mpegts regardless);
                // honouring it would emit fMP4/VP9 the .ts surface can't carry.
                let is_video = matches!(
                    item.kind,
                    pharos_core::MediaKind::Movie | pharos_core::MediaKind::Episode
                );
                let container = if is_video {
                    SegmentContainer::Mpegts
                } else {
                    // V30 — the segment surface only carries segment
                    // containers; a nominal mp4/webm profile target lowers
                    // to mpegts (hls.js demuxes it regardless), fmp4 stays.
                    match Container::from_name(&target_container) {
                        Some(Container::Fmp4) => SegmentContainer::Fmp4,
                        _ => SegmentContainer::Mpegts,
                    }
                };
                let video = if is_video {
                    Some(SegmentVideo::H264)
                } else {
                    // V30 — only re-encodable segment codecs; anything else
                    // (including a nominal "copy") lowers to H.264.
                    match target_video_codec
                        .as_deref()
                        .and_then(VideoCodec::from_name)
                    {
                        Some(VideoCodec::Vp9) => Some(SegmentVideo::Vp9),
                        _ => Some(SegmentVideo::H264),
                    }
                };
                let audio = match target_audio_codec
                    .as_deref()
                    .and_then(AudioCodec::from_name)
                {
                    Some(AudioCodec::Opus) => Some(SegmentAudio::Opus),
                    // Aac, or anything the segment surface can't carry
                    // (mp3/flac/vorbis/copy) → AAC re-encode.
                    _ => Some(SegmentAudio::Aac),
                };
                return SegmentOpts {
                    container,
                    video,
                    audio,
                    // B50 — honour the negotiated cap, bounded by source.
                    video_bitrate_bps: Some(effective_video_bitrate(
                        max_video_bitrate_bps,
                        item.probe.bitrate_bps,
                    )),
                    audio_bitrate_bps: Some(128_000),
                    start_position_ticks: start_ticks,
                    duration_ticks: Some(duration_ticks),
                    audio_source_stream_index: audio_stream_index,
                    burn_subtitle_stream_index: subtitle_stream_index,
                    burn_subtitle_is_text: subtitle_is_text,
                    burn_subtitle_ass_path: None,
                    burn_fonts_dir: None,
                };
            }
            // P9/B45 — VideoRemux (video codec compatible, container/audio
            // not) still RE-ENCODES on the segmented-HLS surface. `-c:v copy`
            // per-segment HLS is structurally broken: ffmpeg can only cut on
            // source keyframes, so segment durations diverge from the uniform
            // EXTINF grid; `-output_ts_offset` is inert under stream copy
            // (ffmpeg 8.1), so every segment restarts its timeline at ~0; and
            // passthrough multichannel AAC is undecodable in Firefox's MSE.
            // Chrome's hls.js happened to tolerate all three — Firefox fatals
            // on the first append and playback never starts. Copy remux
            // remains correct on the PROGRESSIVE /stream path (one continuous
            // output, no per-segment cuts); it must never reach this one.
            Decision::VideoRemux { .. } => {
                return SegmentOpts {
                    container: SegmentContainer::Mpegts,
                    video: Some(SegmentVideo::H264),
                    audio: Some(SegmentAudio::Aac),
                    video_bitrate_bps: Some(target_video_bitrate(item.probe.bitrate_bps)),
                    audio_bitrate_bps: Some(128_000),
                    start_position_ticks: start_ticks,
                    duration_ticks: Some(duration_ticks),
                    audio_source_stream_index: audio_stream_index,
                    burn_subtitle_stream_index: subtitle_stream_index,
                    burn_subtitle_is_text: subtitle_is_text,
                    burn_subtitle_ass_path: None,
                    burn_fonts_dir: None,
                };
            }
            _ => {}
        }
    }

    // Fallback path: no session registered → conservative defaults.
    // B45 — ALWAYS re-encode video, even an h264 source. An earlier
    // optimization stream-copied h264 here, but `-c:v copy` per-segment HLS
    // is structurally broken (keyframe-sloppy durations off the EXTINF grid,
    // `-output_ts_offset` inert under copy so every segment restarts at
    // PTS≈0, multichannel AAC passthrough Firefox can't decode) — see the
    // VideoRemux arm above. Re-encode keeps every segment frame-exact on
    // the shared timeline.
    SegmentOpts {
        container: SegmentContainer::Mpegts,
        video: Some(SegmentVideo::H264),
        audio: Some(SegmentAudio::Aac),
        video_bitrate_bps: Some(target_video_bitrate(item.probe.bitrate_bps)),
        audio_bitrate_bps: Some(128_000),
        start_position_ticks: start_ticks,
        duration_ticks: Some(duration_ticks),
        audio_source_stream_index: audio_stream_index,
        burn_subtitle_stream_index: subtitle_stream_index,
        burn_subtitle_is_text: subtitle_is_text,
        burn_subtitle_ass_path: None,
        burn_fonts_dir: None,
    }
}

/// B46 — pad the per-segment window test so an event starting a hair past
/// the segment boundary (or ending a hair before it) still burns: a frame
/// or two of missing subtitle at a segment edge is invisible to gating
/// jitter but very visible to the viewer.
const BURN_GATE_PAD_MS: u64 = 500;

/// B46 — per-segment burn gating. Image-subtitle burn (overlay decode +
/// composite + re-encode) runs BELOW realtime for VP9 (~6-11 s per 6 s
/// segment observed live, B44 rollout), yet a forced track (the Na'vi
/// case) is SPARSE — most segments contain no event at all. Strip the
/// burn index when the track's event-window timeline proves this segment
/// empty: the segment takes the plain fast path AND shares its cache key
/// with non-burn playback (`sub=off`), so it's usually already warm.
///
/// Fail-open by construction: an unknown/failed/in-flight scan keeps the
/// burn (`EventWindows::Unknown`), so gating can only ever REMOVE
/// provably-empty burns, never lose a visible subtitle. The first ask
/// kicks off the once-ever background scan (persisted per file+mtime+track).
#[tracing::instrument(
    name = "gate_image_sub_burn",
    skip_all,
    fields(media.id = %item.id, burn_idx = opts.burn_subtitle_stream_index)
)]
async fn gate_image_sub_burn(
    state: &AppState,
    item: &pharos_core::MediaItem,
    opts: &mut SegmentOpts,
    start_secs: f64,
    dur_secs: f64,
) {
    let Some(rel_idx) = opts.burn_subtitle_stream_index else {
        return;
    };
    // Task 6 — the image-event-window scan only covers IMAGE subtitle
    // tracks. A TEXT/ASS burn fails open (always keeps the index): the
    // `subtitles=` filter Task 7 wires up no-ops on a segment with no
    // active cue, so there's no correctness reason to gate it, and no
    // event-window data exists for text tracks to gate against anyway.
    if opts.burn_subtitle_is_text {
        return;
    }
    let Some(subs) = state.subtitles.as_ref() else {
        return;
    };
    let mtime = pharos_cache::subtitle_cache::mtime_secs(&item.path).await;
    match subs
        .image_sub_event_windows(&item.path, mtime, rel_idx)
        .await
    {
        pharos_cache::subtitle_cache::EventWindows::Known(windows) => {
            let start_ms = ((start_secs * 1000.0) as u64).saturating_sub(BURN_GATE_PAD_MS);
            let end_ms = ((start_secs + dur_secs) * 1000.0) as u64 + BURN_GATE_PAD_MS;
            if !pharos_transcode::subwin::any_window_overlaps(&windows, start_ms, end_ms) {
                tracing::debug!(
                    media.id = item.id,
                    rel_idx,
                    start_secs,
                    "burn gated off: no subtitle event in segment window"
                );
                opts.burn_subtitle_stream_index = None;
            }
        }
        pharos_cache::subtitle_cache::EventWindows::Unknown => {}
    }
}

/// Point a live TEXT/ASS burn at the small pre-extracted `.ass` sidecar + a
/// font directory instead of the whole source container. ffmpeg's `subtitles`
/// filter opens a SECOND demuxer on its `filename=` at init — ONCE PER SEGMENT
/// — and reads the WHOLE file to gather subtitle packets + embedded fonts, so
/// pointing it at the multi-GB NFS source re-demuxes the entire MKV every 6 s
/// segment (the documented whole-file-demux stutter). The cached sidecar keeps
/// the source's ABSOLUTE event times, so the transcoder's `setpts` alignment is
/// unchanged. Leaves the fields `None` (transcoder falls back to the source
/// `filename=<src>:si=N` form — correct, just slower) whenever the sidecar or
/// fonts can't be produced.
///
/// Must run AFTER `gate_image_sub_burn` (which may clear the burn index) and
/// BEFORE the cache read. Only touches the TEXT/ASS burn; image-sub burn
/// overlays the source bitmap stream and needs neither field. The resolved
/// paths are deterministic per (item, sub) and NOT part of the segment cache
/// key, so a prefetch/live pair that resolve differently still share a key and
/// produce identical output — only encode cost differs.
#[tracing::instrument(
    name = "resolve_text_burn_assets",
    skip_all,
    fields(media.id = %item.id, burn_idx = opts.burn_subtitle_stream_index)
)]
async fn resolve_text_burn_assets(
    state: &AppState,
    item: &pharos_core::MediaItem,
    opts: &mut SegmentOpts,
) {
    if !opts.burn_subtitle_is_text {
        return;
    }
    let Some(rel_idx) = opts.burn_subtitle_stream_index else {
        return;
    };
    // The burn index is subtitle-relative (`si=N`); the ASS cache + extraction
    // key on the ABSOLUTE ffprobe stream index. Invert `codec_relative_index`
    // (position among all subtitle tracks) to recover it.
    let Some(abs_idx) = item
        .probe
        .subtitle_tracks
        .iter()
        .map(|t| t.stream_index)
        .nth(rel_idx as usize)
    else {
        return;
    };
    // Materialize the small `.ass` sidecar (extract-if-cold, bg-gated); leave
    // the field None on failure so the transcoder uses the source-file form.
    match super::subtitles::ensure_ass_sidecar_path(state, item, abs_idx).await {
        Some(p) => opts.burn_subtitle_ass_path = Some(p),
        None => return,
    }
    // libass needs the embedded fonts to render styled ASS. Extract EVERY
    // attachment in one source open (`ensure_all_attachments`) and hand libass
    // the directory via `fontsdir`. No attachments → no fontsdir (defaults).
    if let Some(images) = state.images.as_ref() {
        let indices: Vec<u32> = item
            .probe
            .attachments
            .iter()
            .map(|a| a.stream_index)
            .collect();
        if !indices.is_empty() {
            match images
                .ensure_all_attachments(item.id, &item.path, &indices)
                .await
            {
                Ok(dir) => opts.burn_fonts_dir = Some(dir),
                Err(e) => tracing::debug!(
                    media.id = item.id,
                    error = %e,
                    "font attachment extract failed; burning ASS without fontsdir"
                ),
            }
        }
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
            } else if k.eq_ignore_ascii_case("StartTimeTicks") {
                // B74 — forward the resume offset from master → variant playlist
                // so `render_variant_playlist` can emit `EXT-X-START:TIME-OFFSET`
                // (P18). Without this the native app resumes at 0:00.
                parts.push(format!("StartTimeTicks={v}"));
            } else if k.eq_ignore_ascii_case("VideoBitrate") {
                // Fix (Lace incident): forward the connection-aware bitrate
                // ceiling master → variant → segment so it caps the encoder even
                // when the live session is gone or the path re-encodes a remux.
                parts.push(format!("VideoBitrate={v}"));
            }
        }
    }
    parts.join("&")
}

// ── VP9-in-fMP4 HLS ─────────────────────────────────────────────────────
//
// Firefox/Zen cannot decode H.264 in MSE, so the H.264/MPEG-TS ladder is
// useless to them; they get VP9 instead. Progressive WebM plays but cannot
// seek or report a resume position — so, like Jellyfin, pharos serves VP9 as
// fMP4 HLS. Each `.m4s` is generated on demand exactly like a `.ts` segment
// (independent `ffmpeg -ss/-t` run with SOURCE-anchored timestamps — see
// pharos-transcode — plus a codec-keyed cache), then post-processed by
// `fmp4::process_segment` into moof-only media (negative tfdt clamped).

/// RFC 7741 CODECS token for the VP9 fMP4 output. Profile 0 (8-bit 4:2:0),
/// level 4.0 (covers ≤ 1080p30), which is what the encoder emits. Firefox is
/// lenient about the exact VP9 level in `isTypeSupported`, but the profile +
/// bit-depth must be right for MSE to accept the stream.
const VP9_HLS_CODECS: &str = "vp09.00.40.08,opus";

/// Master playlist for the VP9 fMP4 path. One variant (the source-capped
/// bitrate); subtitle tracks surface as soft `EXT-X-MEDIA` selectors, matching
/// the H.264 master. jellyfin-web loads this as the negotiated TranscodingUrl.
async fn vp9_master(
    state: web::Data<AppState>,
    _user: AuthUser,
    req: HttpRequest,
    path: web::Path<String>,
) -> Result<HttpResponse, actix_web::Error> {
    let id = path.into_inner();
    let item = load_hls_item(&state, &id).await?;
    let qs = playback_qs(&req);
    let bitrate = target_video_bitrate(item.source_bitrate_bps) + 128_000;

    let mut body = String::new();
    body.push_str("#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-INDEPENDENT-SEGMENTS\n");
    // NOTE: text subtitles are delivered as an External rendition via
    // PlaybackInfo (`DeliveryUrl` → SubtitlesOctopus for ASS / cue JSON), which
    // jellyfin-web renders itself. We deliberately do NOT also advertise an
    // in-manifest `EXT-X-MEDIA:TYPE=SUBTITLES` rendition here: hls.js would
    // render it as a second (WebVTT-flattened, unstyled) copy on top of the
    // External one — the "subtitle shown twice" bug. Image subs still burn in.
    // Continuous-audio rendition: a separate audio group so the audio is one
    // gapless encode (no per-segment preskip drift/clicks). The video variant
    // references it via AUDIO="aud"; video segments carry no audio.
    body.push_str(&format!(
        "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"Audio\",DEFAULT=YES,\
         AUTOSELECT=YES,URI=\"/videos/{id}/vp9/audio.m3u8?{qs}\"\n"
    ));
    let resolution = match (item.width, item.height) {
        (Some(w), Some(h)) => format!(",RESOLUTION={w}x{h}"),
        _ => String::new(),
    };
    body.push_str(&format!(
        "#EXT-X-STREAM-INF:BANDWIDTH={bitrate},CODECS=\"{VP9_HLS_CODECS}\",AUDIO=\"aud\"{resolution}\n\
         /videos/{id}/vp9/main.m3u8?{qs}\n"
    ));
    Ok(HttpResponse::Ok()
        .content_type("application/vnd.apple.mpegurl")
        .insert_header(playlist_cache_control(true))
        .body(body))
}

/// `GET /videos/{id}/vp9/audio.m3u8` — the continuous-audio rendition playlist.
/// Kicks the one-ffmpeg audio session (see `hls_cache::ensure_audio_hls`) and
/// serves a COMPLETE VOD playlist synthesised from the source duration (init +
/// N × 6 s segments), so the client can request every audio segment
/// immediately; `vp9_audio_file` polls for each as the session produces it
/// (ahead of the playhead — no whole-file wait). The audio's own 6 s /
/// Opus-aligned segments need NOT match the video's frame-aligned boundaries:
/// as a separate rendition the player syncs the two by PTS.
async fn vp9_audio_playlist(
    state: web::Data<AppState>,
    _user: AuthUser,
    req: HttpRequest,
    path: web::Path<String>,
    q: CiQuery<SegmentQuery>,
) -> Result<HttpResponse, actix_web::Error> {
    let id = path.into_inner();
    let media_id: u64 = pharos_jellyfin_api::dto::parse_item_id(&id)
        .ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
    let item = fetch_item(&state, media_id).await?;
    let qs = playback_qs(&req);
    // Honour the client's AudioStreamIndex (multi-audio titles like Code
    // Geass) so switching track selects a different rendition session.
    let audio_rel = resolve_audio_rel(&item, q.audio_stream_index);
    // Start (or reuse) the audio session in the background.
    let Some(cache) = state.hls.as_ref() else {
        return Err(error::ErrorNotFound("no cache"));
    };
    cache
        .ensure_audio_hls(
            &item.path,
            media_id,
            audio_rel,
            Some(128_000),
            item.probe.frame_rate_mille,
        )
        .await
        .map_err(|e| error::ErrorInternalServerError(format!("audio session: {e}")))?;
    // B103 — take duration through `load_hls_item`, which falls back to a live
    // ffprobe when the persisted probe lacks it. The old `unwrap_or(0.0)` had
    // NO fallback, so a row missing `duration_ms` collapsed the whole audio
    // timeline to a single 6 s segment — the client could then only seek within
    // the first segment (the same `.max(1)` truncation the video variant
    // already guards against via this path).
    let duration = load_hls_item(&state, &id).await?.duration_seconds.max(0.0);
    let segment_count = ((duration / SEGMENT_SECONDS).ceil() as u32).max(1);
    let mut body = String::with_capacity(128 + segment_count as usize * 48);
    body.push_str("#EXTM3U\n#EXT-X-VERSION:7\n");
    body.push_str(&format!(
        "#EXT-X-TARGETDURATION:{}\n",
        SEGMENT_SECONDS as u32
    ));
    body.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-MEDIA-SEQUENCE:0\n");
    body.push_str(&format!(
        "#EXT-X-MAP:URI=\"/videos/{id}/vp9/audio/init.mp4?{qs}\"\n"
    ));
    for seg in 0..segment_count {
        // B105 — frame-align the EXTINF grid to the video variant
        // (`segment_time_range`) so the audio playlist advertises the same
        // segment boundaries the video does; a uniform 6.0 grid drifts against
        // the frame-snapped video timeline on a non-integer-fps source.
        let (start_secs, dur_secs) = segment_time_range(seg, item.probe.frame_rate_mille);
        let remaining = (duration - start_secs).max(0.01);
        let len = dur_secs.min(remaining);
        body.push_str(&format!("#EXTINF:{len:.3},\n"));
        body.push_str(&format!("/videos/{id}/vp9/audio/a{seg}.m4s?{qs}\n"));
    }
    body.push_str("#EXT-X-ENDLIST\n");
    Ok(HttpResponse::Ok()
        .content_type("application/vnd.apple.mpegurl")
        .insert_header(playlist_cache_control(false))
        .body(body))
}

/// `GET /videos/{id}/vp9/audio/{name}` — serve one file (`init.mp4` /
/// `aN.m4s`) of the continuous-audio session, polling briefly for the session
/// to produce it. Ensures the session is running (idempotent) so a segment
/// requested before the playlist still works.
async fn vp9_audio_file(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<(String, String)>,
    q: CiQuery<SegmentQuery>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id, name) = path.into_inner();
    let media_id: u64 = pharos_jellyfin_api::dto::parse_item_id(&id)
        .ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
    let Some(cache) = state.hls.as_ref() else {
        return Err(error::ErrorNotFound("no cache"));
    };
    let item = fetch_item(&state, media_id).await?;
    let audio_rel = resolve_audio_rel(&item, q.audio_stream_index);
    // B42 — tell the cache WHICH segment this request needs: a deep seek
    // (segment far past the running from-0 session's progress) spawns a
    // second, seeked session instead of 404ing "not ready" until the
    // whole-file encode crawls there (observed live: Avatar seek to ~1h40m
    // → a996.m4s 404 → hls.js stalled the seek).
    let want_seg = name
        .strip_prefix('a')
        .and_then(|r| r.strip_suffix(".m4s"))
        .and_then(|r| r.parse::<u32>().ok())
        .unwrap_or(0);
    let dir = cache
        .ensure_audio_hls_covering(
            &item.path,
            media_id,
            audio_rel,
            Some(128_000),
            want_seg,
            item.probe.frame_rate_mille,
        )
        .await
        .map_err(|e| error::ErrorInternalServerError(format!("audio session: {e}")))?;
    let bytes = cache
        .audio_hls_file(&dir, &name)
        .await
        .map_err(|_| error::ErrorNotFound("audio segment not ready"))?;
    let ctype = if name.ends_with(".mp4") {
        "video/mp4"
    } else {
        Container::Fmp4.content_type()
    };
    Ok(HttpResponse::Ok()
        .content_type(ctype)
        .insert_header((
            actix_web::http::header::CACHE_CONTROL,
            "public, max-age=31536000, immutable",
        ))
        .body(bytes))
}

/// VOD variant playlist for the VP9 fMP4 path: an `EXT-X-MAP` init segment
/// followed by `.m4s` media segments. Segment count comes from the source
/// duration, identical to the H.264 variant.
async fn vp9_variant(
    state: web::Data<AppState>,
    _user: AuthUser,
    req: HttpRequest,
    path: web::Path<String>,
) -> Result<HttpResponse, actix_web::Error> {
    let id = path.into_inner();
    let item = load_hls_item(&state, &id).await?;
    let duration = item.duration_seconds;
    let segment_count = ((duration / SEGMENT_SECONDS).ceil() as u32).max(1);
    let qs = playback_qs(&req);

    let mut body = String::with_capacity(256 + segment_count as usize * 48);
    body.push_str("#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-INDEPENDENT-SEGMENTS\n");
    body.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");
    body.push_str(&format!(
        "#EXT-X-TARGETDURATION:{}\n",
        SEGMENT_SECONDS as u32
    ));
    // fMP4 requires the init segment be declared before any media.
    body.push_str(&format!(
        "#EXT-X-MAP:URI=\"/videos/{id}/vp9/init.mp4?{qs}\"\n"
    ));
    let start_ticks = parse_start_time_ticks_qs(req.query_string());
    if start_ticks > 0 {
        let secs = Ticks(start_ticks).seconds();
        body.push_str(&format!("#EXT-X-START:TIME-OFFSET={secs:.3},PRECISE=YES\n"));
    }
    body.push_str("#EXT-X-MEDIA-SEQUENCE:0\n");
    for seg in 0..segment_count {
        // Frame-aligned EXTINF matching the transcoder's actual cut points —
        // a fixed 6.0 drifts against the real video timeline on a non-integer-
        // fps source and desyncs A/V over a long title.
        let (start_secs, dur_secs) = segment_time_range(seg, item.frame_rate_mille);
        let remaining = (duration - start_secs).max(0.01);
        let len = dur_secs.min(remaining);
        body.push_str(&format!("#EXTINF:{len:.3},\n"));
        body.push_str(&format!("/videos/{id}/vp9/{seg}.m4s?{qs}\n"));
    }
    body.push_str("#EXT-X-ENDLIST\n");
    Ok(HttpResponse::Ok()
        .content_type("application/vnd.apple.mpegurl")
        .insert_header(playlist_cache_control(false))
        .body(body))
}

/// Serve the shared fMP4 init segment (`ftyp`+`moov`). Generated by
/// transcoding segment 0 and splitting off its moov — the init is byte-
/// identical across a source's segments, so segment 0 is representative.
async fn vp9_init(
    state: web::Data<AppState>,
    _user: AuthUser,
    req: HttpRequest,
    path: web::Path<String>,
    q: CiQuery<SegmentQuery>,
) -> Result<HttpResponse, actix_web::Error> {
    let id_num: u64 = pharos_jellyfin_api::dto::parse_item_id(&path.into_inner())
        .ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
    let item = fetch_item(&state, id_num).await?;
    check_session(&state, q.play_session_id.as_deref()).await?;
    let mut opts = vp9_segment_opts(
        &state,
        &req,
        &item,
        0,
        q.audio_stream_index,
        q.subtitle_stream_index,
    )
    .await;
    // B46 — same gating as the media segments so the init shares seg 0's
    // cache key.
    {
        let (start_secs, dur_secs) = segment_time_range(0, item.probe.frame_rate_mille);
        gate_image_sub_burn(&state, &item, &mut opts, start_secs, dur_secs).await;
    }
    resolve_text_burn_assets(&state, &item, &mut opts).await;
    let raw = vp9_segment_raw(&state, &item, 0, &opts).await?;
    let processed = fmp4::process_segment(&raw)
        .map_err(|e| error::ErrorInternalServerError(format!("fmp4 init: {e}")))?;
    Ok(HttpResponse::Ok()
        .content_type("video/mp4")
        .insert_header((
            actix_web::http::header::CACHE_CONTROL,
            "public, max-age=31536000, immutable",
        ))
        .body(processed.init))
}

/// Serve one VP9 fMP4 media segment (`moof`+`mdat`, `tfdt`-corrected).
async fn vp9_segment(
    state: web::Data<AppState>,
    _user: AuthUser,
    req: HttpRequest,
    path: web::Path<(String, u32)>,
    q: CiQuery<SegmentQuery>,
) -> Result<HttpResponse, actix_web::Error> {
    let (id, seg) = path.into_inner();
    let id_num: u64 = pharos_jellyfin_api::dto::parse_item_id(&id)
        .ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
    // Active playback → background backfill yields (see serve_segment).
    state.note_playback_activity();
    let item = fetch_item(&state, id_num).await?;
    check_session(&state, q.play_session_id.as_deref()).await?;
    // Bounds-check against the VOD grid the playlist enumerated. An over-index
    // used to reach the encoder, produce no frames past EOF, and surface as a
    // NoMoof/NoMoov → 500; make it a clean 404 (only when duration is known).
    if let Some(dur_ms) = item.probe.duration_ms {
        let grid =
            super::seek::SegmentGrid::new(dur_ms as f64 / 1000.0, item.probe.frame_rate_mille);
        if grid.checked(seg).is_none() {
            return Err(error::ErrorNotFound("segment index past end of media"));
        }
    }
    let mut opts = vp9_segment_opts(
        &state,
        &req,
        &item,
        seg,
        q.audio_stream_index,
        q.subtitle_stream_index,
    )
    .await;
    // B51 — the client's ACTUAL subtitle pick (pre-gate) for the prefetch.
    let wanted_burn = opts.burn_subtitle_stream_index;
    // B46 — strip provably-empty burns BEFORE the hint/prefetch/cache read
    // (they all key on the burn index).
    {
        let (start_secs, dur_secs) = segment_time_range(seg, item.probe.frame_rate_mille);
        gate_image_sub_burn(&state, &item, &mut opts, start_secs, dur_secs).await;
    }
    // Burn a TEXT/ASS sub from the cached sidecar + fontsdir (see the mpegts
    // handler); the prefetch clones inherit the resolved paths.
    resolve_text_burn_assets(&state, &item, &mut opts).await;
    // T87 — remember this play session's exact variant so a SyncPlay seek
    // can prewarm its segments before the client even applies the command.
    if let Some(psid) = q.play_session_id.as_deref() {
        state.note_segment_opts(psid, item.id, &opts);
    }
    // Warm the next few segments in the background so a fast / >1x client finds
    // them cached instead of stalling. Spawned BEFORE this segment's own
    // transcode so N and N+1.. queue together across the CPU pool.
    spawn_segment_prefetch(&state, &item, seg, &opts, wanted_burn);
    let raw = vp9_segment_raw(&state, &item, seg, &opts).await?;
    // A/V-sync diagnostic (T-avsync): log each track's tfdt + content duration
    // so real playback reveals a per-segment gap/overlap or an audio-vs-video
    // duration mismatch — the mechanism behind the reported drift + clicks.
    // traf order: 0 = video, 1 = audio (the -map order in the encoder args).
    let timing = fmp4::segment_track_timing(&raw);
    if let (Some(v), Some(a)) = (timing.first(), timing.get(1)) {
        tracing::info!(
            media.id = item.id,
            seg,
            v_tfdt = v.tfdt_secs,
            v_dur = v.duration_secs,
            v_end = v.tfdt_secs + v.duration_secs,
            a_tfdt = a.tfdt_secs,
            a_dur = a.duration_secs,
            a_end = a.tfdt_secs + a.duration_secs,
            av_dur_delta_ms = (v.duration_secs - a.duration_secs) * 1000.0,
            av_tfdt_delta_ms = (v.tfdt_secs - a.tfdt_secs) * 1000.0,
            a_timescale = a.timescale,
            "vp9 segment A/V timing"
        );
    }
    let processed = fmp4::process_segment(&raw)
        .map_err(|e| error::ErrorInternalServerError(format!("fmp4 seg {seg}: {e}")))?;
    Ok(HttpResponse::Ok()
        .content_type(Container::Fmp4.content_type())
        .insert_header((
            actix_web::http::header::CACHE_CONTROL,
            "public, max-age=31536000, immutable",
        ))
        .body(processed.media))
}

/// Segments to speculatively transcode ahead of the one just requested. A
/// client draining its buffer fast — or playing at >1x — then finds the next
/// segments already warm in the cache instead of stalling on an on-demand
/// transcode. This decouples the client's consumption rate from the encoder's
/// per-segment latency: at 4x, 6s of content is consumed every 1.5s, but with
/// a few segments pipelined ahead across the CPU pool the player never waits.
///
/// Kept small on purpose: prefetch jobs queue on the SAME scheduler as live
/// segments (which already load-balances across every CPU), and the per-key
/// single-flight in `HlsSegmentCache` coalesces a prefetch with the client's
/// own eventual request — so a segment is never transcoded twice and a live
/// request that catches up simply awaits the in-flight prefetch.
///
/// Permit-aware: every software video segment (VP9 AND x264/x265) runs
/// `sw_encode_threads()` (≈4) threads and the CPU device admits
/// `cores / 4` concurrent jobs (`default_cpu_permits`), so `1 live +
/// prefetch` must fit inside that budget with a slot or two spare for
/// audio / trickplay / a second viewer. A fixed 4 was measured on the
/// 16-core box to run 5 concurrent encodes with `queue_wait_ms = 0` —
/// i.e. NOT queued but all racing → each segment took 2-3.6 s instead of
/// the ~0.6 s single-stream benchmark. Also capped at 4 ahead (~24 s of
/// content): past that, a big box (say 64 cores → 16 permits) would flood
/// the queue with speculative work and starve a second stream's LIVE
/// segment behind a wall of one viewer's prefetch.
fn segment_prefetch_ahead() -> u32 {
    let permits = pharos_transcode::device::default_cpu_permits() as u32;
    // Leave 1 slot for the live segment and ~1 for audio/trickplay; floor
    // at 1 so tiny boxes still pipeline one ahead, cap at 4 (~24 s buffer).
    permits.saturating_sub(2).clamp(1, 4)
}

/// Frame-aligned start time (seconds) of segment `seg`: the nominal
/// `seg * SEGMENT_SECONDS` boundary snapped to the nearest source video frame.
///
/// Why: each HLS segment is an INDEPENDENT transcode seeked to its boundary.
/// When the source fps doesn't divide the segment length evenly (23.976 fps →
/// 143.856 frames per 6 s), the nominal boundary lands mid-frame and the
/// encoder snaps the video's first frame to the frame grid — so the video's
/// real start walks off the audio's exact-seconds grid, a little more each
/// segment. That accumulates into audible A/V desync over an episode
/// (~6 ms/segment measured for 23.976 fps → >1 s across a 25-min title).
/// Snapping BOTH tracks' seek + tfdt anchor to the SAME frame time locks them
/// together (measured: a constant −preskip offset, zero accumulation).
/// Falls back to the nominal grid when fps is unknown.
fn segment_start_secs(seg: u32, fps_mille: Option<u32>) -> f64 {
    // ONE canonical frame-snap definition, shared with `seek::SegmentGrid` so
    // the playlist EXTINF, each segment's `-ss`, the audio anchor and the
    // SyncPlay prewarm provably read the same grid.
    super::seek::frame_snapped_start(seg, fps_mille)
}

/// The `(start, duration)` of segment `seg` in seconds, both frame-aligned so
/// consecutive segments butt-join exactly (segment N ends where N+1 begins).
fn segment_time_range(seg: u32, fps_mille: Option<u32>) -> (f64, f64) {
    let start = segment_start_secs(seg, fps_mille);
    let end = segment_start_secs(seg + 1, fps_mille);
    (start, (end - start).max(0.001))
}

/// The segment indices to prefetch after serving `base_seg`: the next
/// [`segment_prefetch_ahead`], stopping at `total_segs` (exclusive) when the
/// item's length is known so we never queue a past-EOF transcode. `None`
/// total = length unknown → prefetch the full window optimistically.
fn prefetch_target_segments(base_seg: u32, total_segs: Option<u32>, ahead_count: u32) -> Vec<u32> {
    let mut out = Vec::new();
    for ahead in 1..=ahead_count {
        let seg = base_seg.saturating_add(ahead);
        if total_segs.is_some_and(|total| seg >= total) {
            break;
        }
        out.push(seg);
    }
    out
}

/// Fire-and-forget warm of the next [`segment_prefetch_ahead`] segments into
/// the HLS cache, using the same `opts` (audio/subtitle/bitrate/codec) as the
/// segment just served — only the start position advances. No-op without a
/// cache; bounded by the item's total segment count (from its probed
/// duration) so it never transcodes past EOF.
/// T87 — SyncPlay seek prewarm: start transcoding the segments at
/// `position_ms` for EVERY play session currently streaming `media_id`, each
/// with its own recorded variant (audio pick, burned sub, codec). Called the
/// moment `/SyncPlay/Seek` is dispatched — the server knows the target
/// seconds before any client applies the command at `When` and asks for
/// data, so the slowest member's segments are cooking (or done) by then.
/// VP9 sessions also get their audio-rendition session pre-seeked (B42).
pub(super) fn prewarm_group_seek(state: &web::Data<AppState>, media_id: u64, position_ms: u64) {
    let state = state.clone();
    actix_web::rt::spawn(async move {
        let Ok(item) = fetch_item(&state, media_id).await else {
            return;
        };
        let variants = state.segment_opts_for_media(media_id);
        if variants.is_empty() {
            return;
        }
        let target = (position_ms as f64 / (SEGMENT_SECONDS * 1000.0)) as u32;
        tracing::info!(
            media.id = media_id,
            target_seg = target,
            variants = variants.len(),
            "syncplay seek prewarm: warming target segments"
        );
        let total_segs = item
            .probe
            .duration_ms
            .map(|ms| ((ms as f64) / (SEGMENT_SECONDS * 1000.0)).ceil() as u32)
            .unwrap_or(u32::MAX);
        for opts in variants {
            // Warm the LANDING segment and the two behind it, directly (the
            // regular prefetch helper only warms base+1.. on the assumption
            // the base itself is being served — here nothing serves it yet).
            for seg in target..(target + 3).min(total_segs) {
                let state = state.clone();
                let item = item.clone();
                let mut o = opts.clone();
                let (start_secs, dur_secs) = segment_time_range(seg, item.probe.frame_rate_mille);
                o.start_position_ticks = Ticks::from_seconds(start_secs).0;
                o.duration_ticks = Some(Ticks::from_seconds(dur_secs).0);
                actix_web::rt::spawn(async move {
                    // B46 — gate per TARGET segment (the hinted opts carry the
                    // hinting segment's burn decision, not this one's).
                    gate_image_sub_burn(&state, &item, &mut o, start_secs, dur_secs).await;
                    let Some(cache) = state.hls.as_ref() else {
                        return;
                    };
                    let _ = cache
                        .segment_bytes_keyed(
                            item.id,
                            seg,
                            o.audio_source_stream_index,
                            o.burn_subtitle_stream_index,
                            &item.path,
                            &o,
                        )
                        .await;
                });
            }
            // VP9 (fMP4) sessions: pre-seek the audio-rendition session too —
            // its whole-file encoder is what stalled deep seeks (B42).
            if matches!(opts.container, SegmentContainer::Fmp4) {
                if let Some(cache) = state.hls.as_ref() {
                    let _ = cache
                        .ensure_audio_hls_covering(
                            &item.path,
                            media_id,
                            opts.audio_source_stream_index,
                            Some(128_000),
                            target,
                            item.probe.frame_rate_mille,
                        )
                        .await;
                }
            }
        }
    });
}

/// How far ahead (in segments) the window-aware burn prefetch looks for the
/// next subtitle-bearing segments, and how many it front-loads per request.
/// B51 — burn segments encode ~2× slower than plain ones (overlay decode +
/// composite; ~10 s per 6 s segment at the negotiated bitrate), and they
/// cluster at dialogue. The shallow near-prefetch can't absorb a run of
/// them, so the buffer drains exactly when a subtitle appears. Front-load
/// the upcoming burn segments during the fast quiet stretches instead:
/// ~2.5 min of lookahead, a few per request, topped up on every served
/// segment as the horizon rolls forward.
const PREFETCH_BURN_HORIZON_SEGS: u32 = 24;
const PREFETCH_BURN_MAX: usize = 4;

/// Prefetch one segment into the HLS cache with the client's REAL subtitle
/// selection (`wanted_burn`, pre-gate), then gate it per THIS segment's
/// window — so the cached bytes match the exact key the client will request.
/// Spawned fire-and-forget.
fn spawn_one_prefetch(
    state: &web::Data<AppState>,
    item: &pharos_core::MediaItem,
    seg: u32,
    opts: &SegmentOpts,
    wanted_burn: Option<u32>,
) {
    let state = state.clone();
    let item = item.clone();
    let mut o = opts.clone();
    // Same frame-aligned boundary the live handler uses, so the prefetched
    // bytes are byte-identical to (and cache-key-match) the client's own
    // eventual request for this segment.
    let (start_secs, dur_secs) = segment_time_range(seg, item.probe.frame_rate_mille);
    o.start_position_ticks = Ticks::from_seconds(start_secs).0;
    o.duration_ticks = Some(Ticks::from_seconds(dur_secs).0);
    // B51 — carry the client's ACTUAL subtitle pick, NOT the requesting
    // segment's gated value. The old code cloned the served segment's opts,
    // so while in a quiet (burn-gated-off) stretch every upcoming
    // subtitle-bearing segment was prefetched WITHOUT the burn — a different
    // cache key from what the client then requests WITH the sub, so it always
    // missed and encoded live, hanging exactly when the subtitle appeared.
    o.burn_subtitle_stream_index = wanted_burn;
    // actix arbiter spawn: the future awaits I/O + the scheduler channel
    // (the encode runs in the transcode worker pool, not here), so it
    // yields the worker immediately and never blocks request handling.
    actix_web::rt::spawn(async move {
        // Gate for THIS segment's window (sparse tracks flip burn on/off
        // between segments; the cache key follows the burn index).
        gate_image_sub_burn(&state, &item, &mut o, start_secs, dur_secs).await;
        let Some(cache) = state.hls.as_ref() else {
            return;
        };
        if let Err(e) = cache
            .segment_bytes_keyed(
                item.id,
                seg,
                o.audio_source_stream_index,
                o.burn_subtitle_stream_index,
                &item.path,
                &o,
            )
            .await
        {
            tracing::debug!(media.id = item.id, seg, error = %e, "segment prefetch failed");
        }
    });
}

fn spawn_segment_prefetch(
    state: &web::Data<AppState>,
    item: &pharos_core::MediaItem,
    base_seg: u32,
    opts: &SegmentOpts,
    wanted_burn: Option<u32>,
) {
    if state.hls.is_none() {
        return;
    }
    let total_segs = item
        .probe
        .duration_ms
        .map(|ms| ((ms as f64) / (SEGMENT_SECONDS * 1000.0)).ceil() as u32);
    // Near shallow prefetch: the next few segments, all of them (fast).
    let near = prefetch_target_segments(base_seg, total_segs, segment_prefetch_ahead());
    let near_end = base_seg + segment_prefetch_ahead();
    for seg in &near {
        spawn_one_prefetch(state, item, *seg, opts, wanted_burn);
    }

    // B51 — window-aware deep prefetch: when a subtitle is selected, look
    // past the near window for the upcoming SLOW burn segments and front-
    // load them, so a dialogue burst is already cached when the playhead
    // reaches it. Only the burn segments (sparse) are deep-prefetched; the
    // fast non-burn ones encode on demand.
    let Some(burn_idx) = wanted_burn else {
        return;
    };
    let Some(subs) = state.subtitles.clone() else {
        return;
    };
    let state = state.clone();
    let item = item.clone();
    let opts = opts.clone();
    actix_web::rt::spawn(async move {
        let mtime = pharos_cache::subtitle_cache::mtime_secs(&item.path).await;
        let windows = match subs
            .image_sub_event_windows(&item.path, mtime, burn_idx)
            .await
        {
            pharos_cache::subtitle_cache::EventWindows::Known(w) => w,
            // Not scanned yet → nothing to front-load; the near prefetch
            // still covers the immediate segments.
            pharos_cache::subtitle_cache::EventWindows::Unknown => return,
        };
        let targets = burn_prefetch_targets(
            near_end + 1,
            base_seg + PREFETCH_BURN_HORIZON_SEGS,
            total_segs,
            &windows,
            item.probe.frame_rate_mille,
            PREFETCH_BURN_MAX,
        );
        for seg in &targets {
            spawn_one_prefetch(&state, &item, *seg, &opts, wanted_burn);
        }
        if !targets.is_empty() {
            tracing::debug!(
                media.id = item.id,
                base_seg,
                burn_segments_prefetched = targets.len(),
                "front-loaded upcoming subtitle-burn segments"
            );
        }
    });
}

/// B51 — the segments in `[from_seg, to_seg]` that OVERLAP a subtitle event
/// window (the slow burn segments), capped at `max`. Pure so the front-load
/// selection is unit-tested without a running transcode. `windows` must be
/// merged/sorted (the scan output).
fn burn_prefetch_targets(
    from_seg: u32,
    to_seg: u32,
    total_segs: Option<u32>,
    windows: &[(u64, u64)],
    fps_mille: Option<u32>,
    max: usize,
) -> Vec<u32> {
    let mut out = Vec::new();
    for seg in from_seg..=to_seg {
        if total_segs.is_some_and(|t| seg >= t) {
            break;
        }
        let (start_secs, dur_secs) = segment_time_range(seg, fps_mille);
        let start_ms = ((start_secs * 1000.0) as u64).saturating_sub(BURN_GATE_PAD_MS);
        let end_ms = ((start_secs + dur_secs) * 1000.0) as u64 + BURN_GATE_PAD_MS;
        if pharos_transcode::subwin::any_window_overlaps(windows, start_ms, end_ms) {
            out.push(seg);
            if out.len() >= max {
                break;
            }
        }
    }
    out
}

async fn fetch_item(state: &AppState, id: u64) -> Result<pharos_core::MediaItem, actix_web::Error> {
    state.stores.get(id).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("not found"),
        other => error::ErrorInternalServerError(other.to_string()),
    })
}

/// W4 — enforce the PlaySessionId (if supplied) is still live, matching the
/// `.ts` segment handler: a GC'd/stopped session must not keep serving bytes.
async fn check_session(state: &AppState, psid: Option<&str>) -> Result<(), actix_web::Error> {
    if let Some(psid) = psid {
        match state.transcode_sessions.get(psid).await {
            Ok(Some(_)) => {}
            Ok(None) => return Err(error::ErrorGone("play session expired")),
            Err(e) => {
                return Err(error::ErrorInternalServerError(format!(
                    "transcode session lookup: {e}"
                )))
            }
        }
    }
    Ok(())
}

/// Build the per-segment VP9/fMP4 [`SegmentOpts`]. Always VP9 in a
/// fragmented-mp4 container; the bitrate cap follows the negotiated session
/// (if any) then the source-derived clamp.
async fn vp9_segment_opts(
    state: &AppState,
    req: &HttpRequest,
    item: &pharos_core::MediaItem,
    seg: u32,
    // Audio track selection no longer applies to the (audio-free) video
    // segment; it drives the separate audio rendition instead. Kept in the
    // signature for the call sites.
    _audio_stream_index: Option<u32>,
    subtitle_stream_index: Option<u32>,
) -> SegmentOpts {
    // Frame-aligned boundaries keep audio + video locked across independent
    // per-segment transcodes (see `segment_start_secs`).
    let (start_secs, dur_secs) = segment_time_range(seg, item.probe.frame_rate_mille);
    let start_ticks = Ticks::from_seconds(start_secs).0;
    let duration_ticks = Ticks::from_seconds(dur_secs).0;
    // Fold the live-session cap with any URL-carried `VideoBitrate` ceiling
    // (Lace incident) so a remote Firefox on VP9 is capped too.
    let cap = min_opt(
        extract_session_bitrate_cap(state, req).await,
        qs_video_bitrate_cap(req.query_string()),
    );
    // B50 — honour the client's negotiated cap, bounded by source + ceiling.
    let bitrate = effective_video_bitrate(cap, item.probe.bitrate_bps);
    // `AudioStreamIndex` / `SubtitleStreamIndex` arrive as ABSOLUTE ffprobe
    // stream indices (jellyfin-web's convention), but the encoder args select
    // by per-CODEC index (`-map 0:a:N`, subtitle-filter `si=N`). Convert via
    // each track's position among its own codec's streams — matching the
    // progressive-webm handler so multi-audio selection + subtitle burn-in
    // pick the right track.
    // Task 6 — burn either IMAGE subtitles (PGS/VOBSUB, unchanged) OR a
    // TEXT/ASS subtitle the client explicitly asked to burn (Tasks 4/5 only
    // forward a text index here for burn-required clients; the default path
    // still delivers text subs out-of-band as a separate External
    // rendition). `burn_subtitle_is_text` tells Task 7's transcoder which
    // ffmpeg filter graph to build.
    let mut sub_is_text = false;
    let sub_rel = subtitle_stream_index.and_then(|abs| {
        let codec = item
            .probe
            .subtitle_tracks
            .iter()
            .find(|t| t.stream_index == abs)
            .map(|t| t.codec.clone().unwrap_or_default());
        let is_image = codec
            .as_deref()
            .map(super::subtitles::is_image_subtitle_codec)
            .unwrap_or(false);
        let is_text = codec
            .as_deref()
            .map(|c| pharos_jellyfin_api::dto::is_text_subtitle_codec(Some(c)))
            .unwrap_or(false);
        if !is_image && !is_text {
            return None;
        }
        sub_is_text = is_text;
        codec_relative_index(
            item.probe.subtitle_tracks.iter().map(|t| t.stream_index),
            abs,
        )
    });
    SegmentOpts {
        container: SegmentContainer::Fmp4,
        video: Some(SegmentVideo::Vp9),
        // Video segments are AUDIO-FREE (A/V-sync fix): audio is served as a
        // separate continuous-encode rendition (see vp9_audio_playlist), so it
        // carries no per-segment Opus preskip. `audio: None` → `-an`. This also
        // makes the video segments faster (no per-segment audio mux/encode).
        audio: None,
        video_bitrate_bps: Some(bitrate),
        audio_bitrate_bps: None,
        start_position_ticks: start_ticks,
        duration_ticks: Some(duration_ticks),
        audio_source_stream_index: None,
        burn_subtitle_stream_index: sub_rel,
        burn_subtitle_is_text: sub_is_text,
        burn_subtitle_ass_path: None,
        burn_fonts_dir: None,
    }
}

/// Convert a client `AudioStreamIndex` (absolute ffprobe index) to the
/// per-codec relative index the continuous-audio session's `-map 0:a:N` wants.
/// `None` (no selection / unknown index) → the session's default track.
fn resolve_audio_rel(
    item: &pharos_core::MediaItem,
    audio_stream_index: Option<u32>,
) -> Option<u32> {
    audio_stream_index.and_then(|abs| {
        codec_relative_index(item.probe.audio_tracks.iter().map(|t| t.stream_index), abs)
    })
}

/// Map an absolute ffprobe stream index to its position among the streams of
/// one codec kind (what ffmpeg's `0:a:N` / `subtitles=si=N` expect). Returns
/// `None` when the absolute index isn't in the list (unknown track → let
/// ffmpeg default-select rather than mis-map).
fn codec_relative_index(abs_indices: impl Iterator<Item = u32>, abs: u32) -> Option<u32> {
    abs_indices
        .enumerate()
        .find(|(_, i)| *i == abs)
        .map(|(pos, _)| pos as u32)
}

/// Produce the raw bytes of one self-contained fragmented-mp4 segment, using
/// the same three tiers as the `.ts` path: codec-keyed disk cache first
/// (production), then the load-balancing scheduler, then an inline ffmpeg
/// fallback. fMP4 surgery needs the whole segment in memory, so the streaming
/// tiers are collected to a `Vec`.
async fn vp9_segment_raw(
    state: &AppState,
    item: &pharos_core::MediaItem,
    seg: u32,
    opts: &SegmentOpts,
) -> Result<Vec<u8>, actix_web::Error> {
    if let Some(cache) = state.hls.as_ref() {
        return cache
            .segment_bytes_keyed(
                item.id,
                seg,
                opts.audio_source_stream_index,
                opts.burn_subtitle_stream_index,
                &item.path,
                opts,
            )
            .await
            .map_err(|e| error::ErrorInternalServerError(format!("segment cache: {e}")));
    }
    if let Some(sched) = state.transcode_scheduler.as_ref() {
        match sched
            .submit_live(item.path.clone(), opts.to_transcode_options())
            .await
        {
            Ok(stream) => return collect_stream(stream).await,
            Err(e) => {
                tracing::warn!(error = %e, "vp9 scheduler live transcode failed; inline fallback")
            }
        }
    }
    let transcoder = FfmpegTranscoder::new();
    let stream = transcoder
        .transcode(&item.path, &opts.to_transcode_options())
        .await
        .map_err(|e| error::ErrorInternalServerError(format!("transcode: {e}")))?;
    collect_stream(stream.into_stream()).await
}

/// Drain a byte stream (`io::Result<Bytes>` items) into a single `Vec<u8>`.
async fn collect_stream<S>(mut stream: S) -> Result<Vec<u8>, actix_web::Error>
where
    S: futures_util::Stream<Item = std::io::Result<actix_web::web::Bytes>> + Unpin,
{
    use futures_util::StreamExt;
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|e| error::ErrorInternalServerError(format!("transcode: {e}")))?;
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

#[cfg(test)]
mod stream_index_tests {
    use super::codec_relative_index;

    #[test]
    fn maps_absolute_audio_to_per_codec_index() {
        // jellyfin-web sends `AudioStreamIndex` as an ABSOLUTE ffprobe stream
        // index; the encoder selects by per-codec index (`-map 0:a:N`). This
        // conversion is what lets a viewer pick the Japanese track on a
        // multi-audio anime. Layout: video@0, jpn audio@1, eng dub audio@2.
        let audio_abs = [1u32, 2];
        assert_eq!(codec_relative_index(audio_abs.iter().copied(), 1), Some(0)); // jpn → 0:a:0
        assert_eq!(codec_relative_index(audio_abs.iter().copied(), 2), Some(1)); // eng → 0:a:1

        // Non-contiguous (subtitle streams interleaved): video@0, subtitle@1,
        // audio@2, subtitle@3, audio@4 → the two audio tracks are 0:a:0 / 0:a:1.
        let non_contig = [2u32, 4];
        assert_eq!(codec_relative_index(non_contig.iter().copied(), 2), Some(0));
        assert_eq!(codec_relative_index(non_contig.iter().copied(), 4), Some(1));

        // An absolute index that isn't an audio stream → None, so the caller
        // falls back to ffmpeg's default selection rather than mis-mapping.
        assert_eq!(codec_relative_index(audio_abs.iter().copied(), 3), None);
    }

    use super::build_segment_opts;
    use pharos_core::{AudioTrack, MediaItem, MediaProbe, SubtitleTrack};

    fn item_with_tracks(video_codec: &str) -> MediaItem {
        MediaItem {
            id: 1,
            probe: MediaProbe {
                video_codec: Some(video_codec.into()),
                // video@0, audio@1, audio@2 (absolute ffprobe indices).
                audio_tracks: vec![
                    AudioTrack {
                        stream_index: 1,
                        ..Default::default()
                    },
                    AudioTrack {
                        stream_index: 2,
                        ..Default::default()
                    },
                ],
                // subtitle@3, subtitle@5 — IMAGE codec (PGS) so they burn in
                // (text subs are delivered External and never burned, so the
                // burn-index mapping only applies to image subs).
                subtitle_tracks: vec![
                    SubtitleTrack {
                        stream_index: 3,
                        codec: Some("hdmv_pgs_subtitle".into()),
                        ..Default::default()
                    },
                    SubtitleTrack {
                        stream_index: 5,
                        codec: Some("hdmv_pgs_subtitle".into()),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn segment_opts_map_absolute_audio_to_per_codec_index() {
        // The .ts (H.264) segment path must convert the ABSOLUTE AudioStreamIndex
        // jellyfin-web sends to the per-codec index ffmpeg's `-map 0:a:N` wants —
        // exactly like the VP9 path. Passing the absolute index straight through
        // selected the WRONG audio track (or none) whenever audio wasn't the
        // first streams. Source is HEVC so it re-encodes to H.264 (burn-in
        // allowed → the subtitle index is threaded too).
        let item = item_with_tracks("hevc");
        // abs audio 2 → relative 1; abs subtitle 5 → relative 1.
        let opts = build_segment_opts(None, &item, 0, 60_000_000, Some(2), Some(5));
        assert_eq!(
            opts.audio_source_stream_index,
            Some(1),
            "absolute audio index 2 must map to per-codec 0:a:1"
        );
        assert_eq!(
            opts.burn_subtitle_stream_index,
            Some(1),
            "absolute subtitle index 5 must map to per-codec si=1"
        );
    }

    #[test]
    fn segment_opts_unknown_audio_index_falls_back_to_default() {
        // An absolute index not among the audio streams → None → ffmpeg default
        // selection, never a mis-map.
        let item = item_with_tracks("hevc");
        let opts = build_segment_opts(None, &item, 0, 60_000_000, Some(9), None);
        assert_eq!(opts.audio_source_stream_index, None);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    // `use actix_web::test` shadows the bare `#[test]` attribute; qualify it.
    #[::core::prelude::v1::test]
    fn segment_boundaries_frame_align_and_butt_join() {
        // 23.976 fps (24000/1001): nominal 6 s boundaries land mid-frame, so
        // they snap to the frame grid — and consecutive segments must join
        // exactly (segment N end == segment N+1 start) with no gap/overlap.
        let fps = Some(23_976);
        let mut prev_end = 0.0_f64;
        for seg in 0..20u32 {
            let (start, dur) = segment_time_range(seg, fps);
            // Butt-join: this segment starts where the previous ended.
            assert!(
                (start - prev_end).abs() < 1e-6,
                "seg {seg} start {start} != prev end {prev_end}"
            );
            // Start is snapped to an exact frame boundary.
            let frames = start * 23_976.0 / 1000.0;
            assert!(
                (frames - frames.round()).abs() < 1e-6,
                "seg {seg} start {start} not on a frame boundary"
            );
            // Duration stays within a frame of the nominal 6 s.
            assert!((dur - SEGMENT_SECONDS).abs() < 0.05, "seg {seg} dur {dur}");
            prev_end = start + dur;
        }
        // Unknown fps → exact nominal grid (no snapping possible).
        assert_eq!(segment_time_range(3, None), (18.0, 6.0));
    }

    #[::core::prelude::v1::test]
    fn prefetch_window_bounds_to_eof() {
        // Fixed count (the real count is core-aware — see segment_prefetch_ahead).
        // Unknown length → the full window from base+1.
        assert_eq!(prefetch_target_segments(0, None, 4), vec![1, 2, 3, 4]);
        assert_eq!(prefetch_target_segments(10, None, 4), vec![11, 12, 13, 14]);
        // Known length → stop before the last segment index (exclusive).
        assert_eq!(prefetch_target_segments(8, Some(12), 4), vec![9, 10, 11]);
        assert_eq!(prefetch_target_segments(10, Some(12), 4), vec![11]);
        // At/near the end → nothing to prefetch.
        assert_eq!(prefetch_target_segments(11, Some(12), 4), Vec::<u32>::new());
        assert_eq!(prefetch_target_segments(20, Some(12), 4), Vec::<u32>::new());
    }

    #[::core::prelude::v1::test]
    fn prefetch_ahead_is_core_aware_and_bounded() {
        // Never zero (always pipeline at least one ahead), and stays modest so a
        // single stream's concurrent encodes don't oversubscribe the cores.
        let n = segment_prefetch_ahead();
        assert!((1..=8).contains(&n), "prefetch-ahead {n} out of sane range");
    }

    use crate::auth::BuiltinAuth;
    use crate::state::Stores;
    use actix_web::test;
    use actix_web::App;
    use pharos_core::{
        MediaItem, MediaKind, MediaStore, SecretString, TokenStore, UserId, UserPolicy, UserRecord,
        UserStore,
    };

    async fn seed() -> (web::Data<AppState>, String) {
        let stores = Stores::connect("sqlite::memory:").await.unwrap();
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

    #[actix_web::test]
    async fn stop_active_encodings_returns_204() {
        // jellyfin-web DELETEs /Videos/ActiveEncodings as the first step of a
        // mid-playback track switch; a 404 here kills its unguarded switch
        // promise chain (the audio never changes). Must answer 204 like Jellyfin.
        let (state, token) = seed().await;
        let app = test::init_service(App::new().app_data(state).configure(register)).await;
        let req = test::TestRequest::delete()
            .uri(&format!(
                "/videos/activeencodings?deviceId=d&PlaySessionId=nope&api_key={token}"
            ))
            .to_request();
        let resp = test::call_service(&app, req).await;
        // 204 even for an unknown session (idempotent stop, Jellyfin-compatible).
        assert_eq!(resp.status(), 204);
    }

    #[actix_web::test]
    async fn stop_active_encodings_requires_auth() {
        let (state, _t) = seed().await;
        let app = test::init_service(App::new().app_data(state).configure(register)).await;
        let req = test::TestRequest::delete()
            .uri("/videos/activeencodings?PlaySessionId=x")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 401);
    }

    #[::core::prelude::v1::test]
    fn burn_prefetch_picks_only_window_segments_capped() {
        // 6 s segments, 24 fps. Windows placed in segment INTERIORS (away
        // from the ±500 ms boundary pad, which is shared with the live gate
        // so prefetch and playback produce identical cache keys): 68-70 s is
        // inside segment 11 only; 122-124 s inside segment 20 only.
        let windows = vec![(68_000, 70_000), (122_000, 124_000)];
        let got = burn_prefetch_targets(3, 30, Some(200), &windows, Some(24_000), 4);
        assert_eq!(got, vec![11, 20], "only the window-overlapping segments");

        // Cap honoured: a dense dialogue run yields at most `max`.
        let dense: Vec<(u64, u64)> = (0..20).map(|i| (i * 6_000, i * 6_000 + 6_000)).collect();
        let got = burn_prefetch_targets(3, 30, Some(200), &dense, Some(24_000), 4);
        assert_eq!(got.len(), 4, "front-load is bounded, got {got:?}");

        // No windows in range → nothing (quiet stretch, near prefetch covers it).
        let got = burn_prefetch_targets(3, 10, Some(200), &[(600_000, 606_000)], Some(24_000), 4);
        assert!(got.is_empty());

        // Never past EOF.
        let got = burn_prefetch_targets(3, 30, Some(12), &[(66_000, 72_000)], Some(24_000), 4);
        assert!(
            got.iter().all(|&s| s < 12),
            "past-EOF segment queued: {got:?}"
        );
    }

    async fn seed_with_probe(probe: pharos_core::MediaProbe) -> (web::Data<AppState>, String) {
        let stores = Stores::connect("sqlite::memory:").await.unwrap();
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
                metadata: Default::default(),
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
        // A source below the ceiling is honoured (better than the old 8M cap).
        assert_eq!(target_video_bitrate(Some(10_000_000)), 10_000_000);
        // Above the 12 Mbps ceiling clamps (B52 — 40M was unencodable in
        // realtime on the CPU-only box; froze VP9 playback).
        assert_eq!(target_video_bitrate(Some(25_000_000)), HLS_MAX_BITRATE_BPS);
        assert_eq!(target_video_bitrate(Some(2_500_000)), 2_500_000);
        assert_eq!(target_video_bitrate(None), HLS_MAX_BITRATE_BPS);
    }

    #[::core::prelude::v1::test]
    fn effective_video_bitrate_honours_cap_bounded_by_source() {
        // Client picks 40 Mbps, source is 25 Mbps → clamped to the 12 Mbps
        // realtime ceiling (B52), not the source (which the box can't encode
        // in realtime).
        assert_eq!(
            effective_video_bitrate(Some(40_000_000), Some(25_000_000)),
            HLS_MAX_BITRATE_BPS,
            "an over-ceiling pick+source clamps to the realtime ceiling"
        );
        // Source BELOW the ceiling is honoured (no wasteful over-encode).
        assert_eq!(
            effective_video_bitrate(Some(40_000_000), Some(9_000_000)),
            9_000_000,
            "a 40 Mbps pick on a 9 Mbps source encodes at source"
        );
        // Client cap BELOW source → honour the client's (lower) choice.
        assert_eq!(
            effective_video_bitrate(Some(6_000_000), Some(25_000_000)),
            6_000_000
        );
        // No cap → source-derived, up to the ceiling.
        assert_eq!(
            effective_video_bitrate(None, Some(30_000_000)),
            HLS_MAX_BITRATE_BPS
        );
        // Both above the ceiling → clamp to the ceiling.
        assert_eq!(
            effective_video_bitrate(Some(80_000_000), Some(60_000_000)),
            HLS_MAX_BITRATE_BPS
        );
        // Unknown source → the cap (bounded by ceiling).
        assert_eq!(effective_video_bitrate(Some(10_000_000), None), 10_000_000);
        // Tiny source still floored at the minimum.
        assert_eq!(
            effective_video_bitrate(Some(40_000_000), Some(100_000)),
            HLS_MIN_BITRATE_BPS
        );
    }

    #[::core::prelude::v1::test]
    fn qs_video_bitrate_cap_parses_case_insensitively() {
        assert_eq!(
            qs_video_bitrate_cap("PlaySessionId=abc&VideoBitrate=6000000&api_key=x"),
            Some(6_000_000)
        );
        // Case-insensitive key, as jellyfin clients vary the casing.
        assert_eq!(
            qs_video_bitrate_cap("videobitrate=4000000"),
            Some(4_000_000)
        );
        // Absent, zero, and unparseable all yield None (no constraint).
        assert_eq!(qs_video_bitrate_cap("PlaySessionId=abc"), None);
        assert_eq!(qs_video_bitrate_cap("VideoBitrate=0"), None);
        assert_eq!(qs_video_bitrate_cap("VideoBitrate=lots"), None);
    }

    #[::core::prelude::v1::test]
    fn min_opt_folds_caps_treating_none_as_unbounded() {
        assert_eq!(min_opt(Some(6_000_000), Some(2_000_000)), Some(2_000_000));
        assert_eq!(min_opt(Some(6_000_000), None), Some(6_000_000));
        assert_eq!(min_opt(None, Some(6_000_000)), Some(6_000_000));
        assert_eq!(min_opt(None, None), None);
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
    fn hls_output_codecs_advertise_transcode_target_not_source() {
        // A legacy AVI/DivX (mpeg4) source is re-encoded to H.264 + AAC:
        // the master playlist must NOT advertise `mpeg4` (which the browser
        // rejects), but an avc1 token. This is the "Playback Error" fix.
        let s = hls_output_codecs_string(Some("mpeg4"), Some("Simple Profile"), Some(1));
        assert!(s.starts_with("avc1."), "mpeg4 must advertise avc1, got {s}");
        assert!(s.ends_with(",mp4a.40.2"), "audio must be AAC, got {s}");
        assert!(!s.contains("mpeg4"), "must not leak the source codec: {s}");

        // vp9 / mpeg2 likewise re-encode to H.264.
        assert!(hls_output_codecs_string(Some("vp9"), None, None).starts_with("avc1."));
        assert!(hls_output_codecs_string(Some("mpeg2video"), None, None).starts_with("avc1."));

        // An h264 source stays h264 in the segments → advertise avc1 with its
        // real profile/level + AAC audio.
        assert_eq!(
            hls_output_codecs_string(Some("h264"), Some("High"), Some(40)),
            "avc1.640028,mp4a.40.2"
        );
        // hevc source is RE-ENCODED to H.264 by this pipeline (no HEVC output
        // encoder), so the master must advertise avc1 — NOT hvc1, which browsers
        // reject as undecodable HEVC even though the segments are H.264.
        let hevc = hls_output_codecs_string(Some("hevc"), Some("Main"), Some(120));
        assert!(
            hevc.starts_with("avc1."),
            "hevc must advertise avc1, got {hevc}"
        );
        assert!(!hevc.contains("hvc1"), "must not leak hvc1: {hevc}");
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
                // One IMAGE subtitle stream at ABSOLUTE ffprobe index 2 →
                // per-codec index si=0. Image (PGS) so the burn-in tests
                // exercise the absolute→relative mapping (text subs are never
                // burned — they're delivered as an External rendition).
                subtitle_tracks: vec![pharos_core::SubtitleTrack {
                    stream_index: 2,
                    codec: Some("hdmv_pgs_subtitle".into()),
                    ..Default::default()
                }],
                ..Default::default()
            },
            series: None,
            created_at: None,
            metadata: Default::default(),
        }
    }

    #[::core::prelude::v1::test]
    fn fallback_keeps_h264_transcode_for_unknown_codec() {
        // VP9 / AV1 / mpeg2 etc. → safe H.264 re-encode.
        let item = item_with_video_codec(Some("vp9"));
        let opts = build_segment_opts(None, &item, 0, 60_000_000, None, None);
        assert!(matches!(
            opts.video,
            Some(pharos_transcode::SegmentVideo::H264)
        ));
        assert!(opts.video_bitrate_bps.is_some());
    }

    #[::core::prelude::v1::test]
    fn fallback_reencodes_even_h264_source() {
        // B45 — `-c:v copy` per-segment HLS is structurally broken (PTS reset
        // every segment because `-output_ts_offset` is inert under copy,
        // keyframe-sloppy durations off the EXTINF grid, multichannel AAC
        // passthrough Firefox can't decode). Even an h264 source re-encodes.
        let item = item_with_video_codec(Some("h264"));
        let opts = build_segment_opts(None, &item, 0, 60_000_000, None, None);
        assert!(matches!(
            opts.video,
            Some(pharos_transcode::SegmentVideo::H264)
        ));
        assert!(
            opts.video_bitrate_bps.is_some(),
            "re-encode needs a -b:v cap"
        );
    }

    #[::core::prelude::v1::test]
    fn fallback_reencodes_hevc_to_h264() {
        // HEVC must be RE-ENCODED, never copied: the master playlist advertises
        // avc1 for an HEVC source, so copying HEVC bytes under that manifest
        // breaks h264-only clients (Firefox/Safari) with a manifestParsingError
        // and poisons the segment cache with an undecodable segment.
        for codec in ["hevc", "h265", "HEVC", "Hevc"] {
            let item = item_with_video_codec(Some(codec));
            let opts = build_segment_opts(None, &item, 0, 60_000_000, None, None);
            assert!(
                matches!(opts.video, Some(pharos_transcode::SegmentVideo::H264)),
                "codec {codec} must re-encode to H264, got {:?}",
                opts.video,
            );
            assert!(
                opts.video_bitrate_bps.is_some(),
                "re-encode needs a -b:v cap"
            );
        }
    }

    #[::core::prelude::v1::test]
    fn fallback_burns_image_subs_on_h264_source() {
        // B45 — h264 sources re-encode now, so image-sub burn-in works on
        // them too (it silently no-op'd under the old `-c:v copy` fallback).
        let item = item_with_video_codec(Some("h264"));
        let opts = build_segment_opts(None, &item, 0, 60_000_000, None, Some(2));
        assert_eq!(opts.burn_subtitle_stream_index, Some(0));
    }

    #[::core::prelude::v1::test]
    fn video_remux_session_reencodes_on_segment_surface() {
        // B45 — a VideoRemux decision (copy-compatible video) must still
        // re-encode on the segmented-HLS surface; `-c:v copy` segments are
        // structurally broken (see build_segment_opts).
        let item = item_with_video_codec(Some("h264"));
        let session = crate::transcode_sessions::TranscodeSession {
            media_id: item.id,
            decision: crate::api::jellyfin::device_profile::Decision::VideoRemux {
                target_container: "ts".into(),
                target_audio_codec: Some("aac".into()),
            },
            source_probe: item.probe.clone(),
        };
        let opts = build_segment_opts(Some(session), &item, 0, 60_000_000, None, None);
        assert!(matches!(
            opts.video,
            Some(pharos_transcode::SegmentVideo::H264)
        ));
        assert!(
            opts.video_bitrate_bps.is_some(),
            "re-encode needs a -b:v cap"
        );
        assert!(matches!(
            opts.audio,
            Some(pharos_transcode::SegmentAudio::Aac)
        ));
        assert!(matches!(
            opts.container,
            pharos_transcode::SegmentContainer::Mpegts
        ));
    }

    #[::core::prelude::v1::test]
    fn fallback_keeps_subtitle_burn_in_when_transcoding() {
        // Re-encode path retains the requested burn-in, MAPPED from the
        // absolute ffprobe index (2) to the per-codec subtitle index (si=0).
        let item = item_with_video_codec(Some("vp9"));
        let opts = build_segment_opts(None, &item, 0, 60_000_000, None, Some(2));
        assert_eq!(opts.burn_subtitle_stream_index, Some(0));
    }

    /// Task 6 fixture — extends `item_with_video_codec`'s single IMAGE track
    /// (abs idx 2) with a second, TEXT/ASS track at abs idx 3 (per-codec
    /// relative index 1, since the subtitle-filter's `si=N` counts across
    /// ALL subtitle streams in appearance order, not per codec kind).
    fn item_with_subs() -> pharos_core::MediaItem {
        let mut item = item_with_video_codec(Some("h264"));
        item.probe.subtitle_tracks.push(pharos_core::SubtitleTrack {
            stream_index: 3,
            codec: Some("ass".into()),
            ..Default::default()
        });
        item
    }

    #[::core::prelude::v1::test]
    fn build_segment_opts_burns_text_sub_and_flags_is_text() {
        // Task 6 — a TEXT/ASS `SubtitleStreamIndex` (Tasks 4/5 forward these
        // for burn-required clients) must now yield a burn index too, MAPPED
        // to the per-codec relative index (abs 3 -> si=1), with
        // `burn_subtitle_is_text` set so Task 7 picks the `subtitles=`
        // filter instead of `overlay`.
        let item = item_with_subs();
        let opts = build_segment_opts(None, &item, 0, 60_000_000, None, Some(3));
        assert_eq!(opts.burn_subtitle_stream_index, Some(1));
        assert!(opts.burn_subtitle_is_text);

        // The existing IMAGE track still burns, but is NOT flagged text.
        let opts = build_segment_opts(None, &item, 0, 60_000_000, None, Some(2));
        assert_eq!(opts.burn_subtitle_stream_index, Some(0));
        assert!(!opts.burn_subtitle_is_text);

        // No selection -> no burn, and the flag stays false.
        let opts = build_segment_opts(None, &item, 0, 60_000_000, None, None);
        assert_eq!(opts.burn_subtitle_stream_index, None);
        assert!(!opts.burn_subtitle_is_text);
    }

    #[actix_web::test]
    async fn vp9_segment_opts_burns_text_sub_and_flags_is_text() {
        // Task 6 — same contract as `build_segment_opts` above, exercised on
        // the VP9/fMP4 segment-opts builder (brief's specified test target).
        let stores = Stores::connect("sqlite::memory:").await.unwrap();
        let state = web::Data::new(AppState::new(stores, "t".into()));
        let req = test::TestRequest::default().to_http_request();
        let item = item_with_subs();

        let opts = vp9_segment_opts(&state, &req, &item, 0, None, Some(3)).await;
        assert_eq!(opts.burn_subtitle_stream_index, Some(1));
        assert!(opts.burn_subtitle_is_text);

        let opts = vp9_segment_opts(&state, &req, &item, 0, None, Some(2)).await;
        assert_eq!(opts.burn_subtitle_stream_index, Some(0));
        assert!(!opts.burn_subtitle_is_text);

        let opts = vp9_segment_opts(&state, &req, &item, 0, None, None).await;
        assert_eq!(opts.burn_subtitle_stream_index, None);
        assert!(!opts.burn_subtitle_is_text);
    }

    #[::core::prelude::v1::test]
    fn fallback_falls_back_to_h264_when_probe_has_no_video_codec() {
        // Defensive: a probe row predating the codec migration shows
        // no video codec; we must still pick a working target.
        let item = item_with_video_codec(None);
        let opts = build_segment_opts(None, &item, 0, 60_000_000, None, None);
        assert!(matches!(
            opts.video,
            Some(pharos_transcode::SegmentVideo::H264)
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
