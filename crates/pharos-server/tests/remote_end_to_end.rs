#![allow(clippy::unwrap_used, clippy::expect_used)]
//! 008 — a URL becomes a playable library item, over the real HTTP surface.
//!
//! Every other 008 test proves one module in isolation: the codec string comes
//! apart, the chunk arithmetic is right, the resolver parses a captured
//! document, the cache serves a range. None of them prove the JOIN, and the
//! joins are where this feature's risk actually lives — a resolution made in
//! one place is consumed in three others, and the byte path leaves the process
//! entirely and comes back through a loopback socket.
//!
//! That distinction is not theoretical here. The 2026-08-01 AV1 outage was a
//! decision that every unit got right and the seam between two of them got
//! wrong; it cost a day. This file exists so 008's seams are not discovered the
//! same way.
//!
//! **Where it stops.** It does not run ffmpeg. A real segment encode needs the
//! scheduler, an out-of-process worker and a fixture corpus, which in this repo
//! means the `#[ignore]`d `PHAROS_TEST_FIXTURES` pattern
//! (`ffmpeg_integration.rs`) and no coverage on an ordinary PR. Everything up
//! to the point where ffmpeg is handed a locator IS covered, including that the
//! locator resolves to bytes the cache really serves. The encode itself is the
//! one leg already exercised elsewhere for local sources, and it cannot tell a
//! loopback URL from a file.

use actix_web::{http::StatusCode, test, web, App};
use pharos_core::{SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore};
use pharos_server::{
    api::{jellyfin, pharos as pharos_api},
    auth::BuiltinAuth,
    middleware::LowercasePath,
    remote::{source_cache::SourceCache, RemoteResolver, ResolverCache},
    state::{AppState, Stores},
};
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

/// A stub standing in for `yt-dlp`, emitting a fixed `-J` document.
///
/// The real binary is not on the test path and would reach YouTube if it were.
/// What matters for the seams below is the SHAPE of its answer, which this
/// reproduces exactly — including the two-URL adaptive form, the `avc1.640028`
/// codec string that has to come apart into three probe fields, and the
/// `duration` without which ingestion is refused.
fn stub_ytdlp(dir: &std::path::Path, doc: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("stub-ytdlp");
    let mut f = std::fs::File::create(&bin).unwrap();
    write!(f, "#!/bin/sh\ncat <<'YTDLPEOF'\n{doc}\nYTDLPEOF\n").unwrap();
    f.set_permissions(std::fs::Permissions::from_mode(0o755))
        .unwrap();
    bin
}

/// An upstream that answers range requests, standing in for the CDN.
async fn upstream(body: Vec<u8>) -> (String, actix_web::dev::ServerHandle) {
    use actix_web::{HttpRequest, HttpResponse, HttpServer};
    let served = web::Data::new(body);
    let srv = HttpServer::new(move || {
        App::new().app_data(served.clone()).default_service(web::to(
            |req: HttpRequest, body: web::Data<Vec<u8>>| async move {
                let range = req
                    .headers()
                    .get(actix_web::http::header::RANGE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("bytes="))
                    .and_then(|v| v.split_once('-'))
                    .and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?)));
                let (a, b) = range.unwrap_or((0, body.len() - 1));
                let b = b.min(body.len() - 1);
                HttpResponse::PartialContent()
                    .insert_header((
                        actix_web::http::header::CONTENT_RANGE,
                        format!("bytes {a}-{b}/{}", body.len()),
                    ))
                    .body(body[a..=b].to_vec())
            },
        ))
    })
    .bind(("127.0.0.1", 0))
    .unwrap();
    let base = format!("http://{}", srv.addrs()[0]);
    let srv = srv.run();
    let handle = srv.handle();
    tokio::spawn(srv);
    (base, handle)
}

fn adaptive_doc(video: &str, audio: &str, vcodec: &str) -> String {
    format!(
        r#"{{"id":"dQw4w9WgXcQ","extractor":"youtube","extractor_key":"Youtube",
           "title":"Never Gonna Give You Up","duration":212,
           "thumbnail":"https://i.ytimg.com/vi/dQw4w9WgXcQ/maxres.jpg",
           "width":1920,"height":1080,"fps":25.0,
           "vcodec":"{vcodec}","acodec":"mp4a.40.2","ext":"mp4","tbr":2500.0,
           "filesize_approx":66000000,
           "requested_formats":[
             {{"url":"{video}","vcodec":"{vcodec}","acodec":"none"}},
             {{"url":"{audio}","vcodec":"none","acodec":"mp4a.40.2"}}]}}"#
    )
}

fn app(
    state: web::Data<AppState>,
) -> App<
    impl actix_web::dev::ServiceFactory<
        actix_web::dev::ServiceRequest,
        Config = (),
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
        InitError = (),
    >,
> {
    App::new()
        .app_data(state)
        .wrap(LowercasePath)
        .configure(jellyfin::configure)
        .configure(pharos_api::remote_items::routes)
}

macro_rules! auth_header {
    ($token:expr) => {
        (
            "X-Emby-Authorization",
            format!(
                "MediaBrowser Client=\"t\", Device=\"d\", DeviceId=\"i\", Version=\"1\", Token=\"{}\"",
                $token
            ),
        )
    };
}

struct Harness {
    state: web::Data<AppState>,
    token: String,
    cache: Arc<SourceCache>,
    _td: TempDir,
    _upstream: actix_web::dev::ServerHandle,
}

async fn harness(vcodec: &str) -> (Harness, String) {
    let td = TempDir::new().unwrap();
    let (base, up) = upstream((0..60_000u32).map(|i| (i % 251) as u8).collect()).await;
    let doc = adaptive_doc(
        &format!("{base}/video?sig=v"),
        &format!("{base}/audio?sig=a"),
        vcodec,
    );
    let bin = stub_ytdlp(td.path(), &doc);

    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("p")).unwrap();
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "u".into(),
            password_hash: hash,
            policy: UserPolicy {
                admin: true,
                ..UserPolicy::default()
            },
        })
        .await
        .unwrap();
    let token = stores.issue(uid, "t").await.unwrap().0.expose().to_string();

    let cache = Arc::new(
        SourceCache::new(
            td.path().join("cache"),
            pharos_server::bg_io::NetworkGate::new(4),
            u64::MAX,
        )
        .await,
    );
    let port = pharos_server::remote::source_cache::server::spawn(cache.clone())
        .expect("the loopback range listener must start");
    let resolver = Arc::new(ResolverCache::new(
        RemoteResolver::new(bin.to_string_lossy().to_string(), Duration::from_secs(10)),
        Duration::from_secs(600),
        Some(cache.clone()),
    ));
    let state = web::Data::new(
        AppState::new(stores, "srv".into())
            .with_remote_resolver(resolver)
            .with_remote_cache(cache.clone(), port),
    );
    (
        Harness {
            state,
            token,
            cache,
            _td: td,
            _upstream: up,
        },
        base,
    )
}

/// Paste a URL, get a playable item — the whole 008 chain over the wire.
#[actix_web::test]
async fn a_url_becomes_an_item_a_client_can_play() {
    let (h, _base) = harness("avc1.640028").await;
    let app = test::init_service(app(h.state.clone())).await;

    // ---- ingestion
    let req = test::TestRequest::post()
        .uri("/Pharos/Remote/Items")
        .insert_header(auth_header!(&h.token))
        .set_json(serde_json::json!({"url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ"}))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK, "ingestion must succeed");
    let body: serde_json::Value = test::read_body_json(resp).await;
    let id = body["Id"].as_str().expect("an item id to play").to_string();
    assert_eq!(body["Name"], "Never Gonna Give You Up");

    // ---- the row is URL-backed, and the descriptive facts survived the
    // resolver's parse. `RunTimeTicks` matters on its own: an item with no
    // duration renders a 0s playlist and re-probes the network per request.
    let req = test::TestRequest::get()
        .uri(&format!("/Items/{id}?Fields=Path"))
        .insert_header(auth_header!(&h.token))
        .to_request();
    let item: serde_json::Value = test::read_body_json(test::call_service(&app, req).await).await;
    assert!(
        item["Path"]
            .as_str()
            .unwrap_or_default()
            .starts_with("ytdlp://"),
        "the stored path must be the STABLE reference, not a signed URL: {:?}",
        item["Path"]
    );
    assert_eq!(
        item["RunTimeTicks"].as_i64(),
        Some(212 * 10_000_000),
        "a duration must be persisted at ingestion"
    );

    // ---- PlaybackInfo: transcode only, and it says why.
    let req = test::TestRequest::post()
        .uri(&format!("/Items/{id}/PlaybackInfo"))
        .insert_header(auth_header!(&h.token))
        .set_json(serde_json::json!({}))
        .to_request();
    let pbi: serde_json::Value = test::read_body_json(test::call_service(&app, req).await).await;
    let src = &pbi["MediaSources"][0];
    assert_eq!(
        src["SupportsDirectPlay"], false,
        "a URL-backed source has no file for a client to open"
    );
    assert_eq!(src["SupportsDirectStream"], false);
    assert_eq!(src["SupportsTranscoding"], true);
    let turl = src["TranscodingUrl"]
        .as_str()
        .expect("stock jellyfin-web plays this through hls.js, so it needs a TranscodingUrl");
    assert!(
        turl.contains("master.m3u8"),
        "the transcoding url must be an HLS master: {turl}"
    );

    // ---- the video codec reached the wire as a NAME, not as the RFC 6381
    // token yt-dlp reports.
    let stream = src["MediaStreams"]
        .as_array()
        .and_then(|s| s.iter().find(|s| s["Type"] == "Video"))
        .expect("a video stream");
    assert_eq!(stream["Codec"], "h264", "not `avc1.640028`");
    assert_eq!(stream["Width"].as_i64(), Some(1920));

    // ---- and it came apart into the three PROBE fields the CODECS attribute
    // is built from. `MediaStreams` carries no profile/level for any source, so
    // the row is where this is observable — and it is the row the playlist
    // builder reads. Stored whole, the CODECS attribute is built from a CODECS
    // attribute and Safari matches no variant.
    let row = pharos_core::MediaStore::list(&h.state.stores)
        .await
        .unwrap()
        .into_iter()
        .find(|i| i.path.to_string_lossy().starts_with("ytdlp://"))
        .expect("the ingested row");
    assert_eq!(row.probe.video_codec.as_deref(), Some("h264"));
    assert_eq!(row.probe.video_profile.as_deref(), Some("High"));
    assert_eq!(
        row.probe.video_level,
        Some(40),
        "H.264 level is x10; taking HEVC's x30 scale here would advertise a \
         level no client matches"
    );

    // ---- the master playlist renders, and carries the per-item source
    // generation. Without `s=`, a re-resolved source keeps serving the previous
    // one's segments out of a browser cache held `immutable` for a year.
    let req = test::TestRequest::get()
        .uri(&format!("/Videos/{id}/master.m3u8"))
        .insert_header(auth_header!(&h.token))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "the master playlist must render"
    );
    let m3u8 = String::from_utf8(test::read_body(resp).await.to_vec()).unwrap();
    assert!(m3u8.starts_with("#EXTM3U"), "{m3u8}");

    // ---- the MEDIA playlist is where the generation has to appear: segments
    // and init ship `Cache-Control: immutable, max-age=31536000`, so without a
    // per-item `s=` a re-resolved source keeps serving the previous one's bytes
    // out of the browser for a year. The master's variant URIs point at these
    // playlists, which are not immutable, so they do not need it.
    let variant = m3u8
        .lines()
        .find(|l| l.contains("/variants/") || l.contains("/main.m3u8"))
        .expect("a rendition URI");
    let req = test::TestRequest::get()
        .uri(variant)
        .insert_header(auth_header!(&h.token))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "the media playlist must render"
    );
    let media = String::from_utf8(test::read_body(resp).await.to_vec()).unwrap();
    assert!(
        media.contains("s="),
        "every segment URI must carry the per-item source generation: {media}"
    );
}

/// The byte path, end to end: what ffmpeg would be handed really serves the
/// upstream's bytes, through the loopback cache rather than the CDN.
///
/// This is the seam no unit test reaches. `SourceCache` is proven against a
/// URL a test handed it; here the URL comes out of the resolver, having been
/// through ingestion, and the read goes over a real socket.
#[actix_web::test]
async fn the_locator_handed_to_ffmpeg_serves_the_upstream_bytes() {
    let (h, base) = harness("avc1.640028").await;
    let app = test::init_service(app(h.state.clone())).await;
    let req = test::TestRequest::post()
        .uri("/Pharos/Remote/Items")
        .insert_header(auth_header!(&h.token))
        .set_json(serde_json::json!({"url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ"}))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), StatusCode::OK);

    let r = pharos_core::origin::RemoteRef::parse("ytdlp://youtube/dQw4w9WgXcQ").unwrap();
    let resolver = h.state.remote.as_ref().expect("a resolver");
    let media = resolver.locate(&r).await.expect("the item must resolve");
    assert!(
        media.video().starts_with(&base),
        "the resolver must point at the upstream it was told about: {}",
        media.video()
    );

    // Register + read through the cache exactly as the playback path does.
    let key = h.cache.register(media.video()).await.expect("register");
    let bytes = h.cache.read(&key, 0, 4096).await.expect("read");
    let expected: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    assert_eq!(bytes, expected, "the cache must serve the upstream's bytes");

    // And a second read is served from disk — the whole reason the cache
    // exists, since otherwise every segment re-opens an HTTPS connection.
    h.cache.read(&key, 0, 4096).await.expect("re-read");
    assert!(h.cache.held_bytes().await > 0, "the bytes must be retained");
}

/// A YouTube "best" source is AV1, and AV1 is exactly what the deployment's
/// GPU cannot decode (B179). The two features must compose: the codec the
/// resolver stores has to be one `SourceCodec::from_name` understands, or the
/// decode gate silently falls back to "unknown" and the reason in the metric
/// is wrong even though the outcome happens to be safe.
#[actix_web::test]
async fn a_remote_av1_source_names_a_codec_the_decode_gate_understands() {
    let (h, _base) = harness("av01.0.08M.10").await;
    let app = test::init_service(app(h.state.clone())).await;
    let req = test::TestRequest::post()
        .uri("/Pharos/Remote/Items")
        .insert_header(auth_header!(&h.token))
        .set_json(serde_json::json!({"url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ"}))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), StatusCode::OK);

    // Found by its stable path rather than by the wire id: the point of the
    // assertion below is what the RESOLVER stored, and routing that lookup
    // through the id mapping would fail for a reason unrelated to it.
    let items = pharos_core::MediaStore::list(&h.state.stores)
        .await
        .unwrap();
    let item = items
        .iter()
        .find(|i| i.path.to_string_lossy().starts_with("ytdlp://"))
        .expect("the ingested row");
    let codec = item
        .probe
        .video_codec
        .as_deref()
        .expect("a remote item must name its video codec");
    assert_eq!(
        codec, "av1",
        "stored whole (`av01.0.08M.10`) this parses to None, the gate reports \
         source_codec=\"unknown\", and nobody can tell an unnameable container \
         from the codec that caused the outage"
    );
    assert_eq!(
        pharos_transcode::SourceCodec::from_name(codec),
        Some(pharos_transcode::SourceCodec::Av1),
        "and the decode gate must recognise it, so a remote AV1 source is \
         denied hardware decode on a card with no AV1 block rather than \
         failing every frame"
    );
}
