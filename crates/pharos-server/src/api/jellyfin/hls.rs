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

async fn duration_seconds(
    state: &AppState,
    id_str: &str,
) -> Result<(f64, std::path::PathBuf), actix_web::Error> {
    let id: u64 = id_str
        .parse()
        .map_err(|_| error::ErrorBadRequest("invalid id"))?;
    let item = state.stores.get(id).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("not found"),
        other => error::ErrorInternalServerError(other.to_string()),
    })?;
    let prober = FfmpegProber::new();
    let info = prober
        .probe(&item.path)
        .await
        .map_err(|e| error::ErrorInternalServerError(format!("probe: {e}")))?;
    let secs = info
        .duration_ms
        .map(|ms| ms as f64 / 1000.0)
        .unwrap_or(0.0);
    Ok((secs, item.path))
}

async fn master_playlist(
    state: web::Data<AppState>,
    _user: AuthUser,
    req: HttpRequest,
    path: web::Path<String>,
) -> Result<impl Responder, actix_web::Error> {
    let id = path.into_inner();
    let (duration, _) = duration_seconds(&state, &id).await?;
    // Single variant for phase 1. Bitrate estimate just informs client.
    let body = format!(
        "#EXTM3U\n#EXT-X-VERSION:3\n\
         #EXT-X-STREAM-INF:BANDWIDTH=2500000,CODECS=\"avc1.640028,mp4a.40.2\",RESOLUTION=1920x1080\n\
         /Videos/{id}/main.m3u8?{}\n",
        token_qs(&req)
    );
    let _ = duration;
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
    let (duration, _) = duration_seconds(&state, &id).await?;
    let segment_count = (duration / SEGMENT_SECONDS).ceil() as u32;
    let segment_count = segment_count.max(1);
    let qs = token_qs(&req);
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

async fn segment(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<(String, u32)>,
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

    let opts = TranscodeOptions {
        container: Container::Mpegts,
        video: Some(VideoCodec::H264),
        audio: Some(AudioCodec::Aac),
        video_bitrate_bps: Some(2_500_000),
        audio_bitrate_bps: Some(128_000),
        start_position_ticks: start_ticks,
        duration_ticks: Some(duration_ticks),
    };

    // T42: when an HLS cache is wired, route through it. Otherwise
    // fall back to live transcoding (every request spawns ffmpeg).
    if let Some(cache) = state.hls.as_ref() {
        let bytes = cache
            .segment_bytes(id_num, seg, &item.path, &opts)
            .await
            .map_err(|e| error::ErrorInternalServerError(format!("segment cache: {e}")))?;
        return Ok(HttpResponse::Ok()
            .content_type(Container::Mpegts.content_type())
            .body(bytes));
    }

    let transcoder = FfmpegTranscoder::new();
    let stream = transcoder
        .transcode(&item.path, &opts)
        .await
        .map_err(|e| error::ErrorInternalServerError(format!("transcode: {e}")))?;
    Ok(HttpResponse::Ok()
        .content_type(Container::Mpegts.content_type())
        .streaming(stream.into_stream()))
}

/// Helper: produce `api_key=…` query string from the incoming request
/// so the embedded segment URLs carry forward the bearer token.
fn token_qs(req: &HttpRequest) -> String {
    match extract_token(req) {
        Some(t) => format!("api_key={t}"),
        None => String::new(),
    }
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
}
