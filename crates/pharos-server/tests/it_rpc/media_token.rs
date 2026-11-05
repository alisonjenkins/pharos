//! B542 / V437: `Session.mediaToken` gives a native client a token it can put
//! in an image or stream URL without exposing its Full device token. The token
//! reaches GET/HEAD on item images, `/videos/{id}/` and `/audio/{id}/` and
//! nothing else, and stops working when its lifetime ends.
//! Disarm check: with the Media arm of `scope_permits` answering `true` (like
//! Full), `a_media_token_is_refused_everywhere_else` fails on `GET /Items`.

use std::rc::Rc;
use std::time::Duration;

use actix_web::{http::StatusCode, test, web};
use pharos_api::pharos_capnp::{bootstrap, done, error, media_token_result, session};
use pharos_api::Connection;
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, UserId, UserPolicy, UserRecord,
    UserStore,
};
use pharos_server::api::pharos::rpc;
use pharos_server::auth::BuiltinAuth;
use pharos_server::state::{AppState, Stores};

use crate::bootstrap::LOCAL;
use crate::common::build_app;
use crate::login::login;

const POSTER: &[u8] = b"MEDIA-TOKEN-POSTER";
const MOVIE: &[u8] = b"MEDIA-TOKEN-MOVIE-BYTES";

struct Rig {
    state: web::Data<AppState>,
    /// Keeps the artwork, media and cache directories alive.
    _dir: tempfile::TempDir,
}

/// One user ("guest" / "hunter2") and movie 7 with a poster, in a sqlite
/// store with an image and an HLS cache.
async fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let poster = dir.path().join("poster.jpg");
    std::fs::write(&poster, POSTER).unwrap();
    let movie = dir.path().join("movie.webm");
    std::fs::write(&movie, MOVIE).unwrap();
    std::fs::create_dir_all(dir.path().join("images")).unwrap();
    std::fs::create_dir_all(dir.path().join("hls")).unwrap();
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let hash = BuiltinAuth::new(stores.clone())
        .hash_password(&SecretString::new("hunter2"))
        .unwrap();
    stores
        .create(UserRecord {
            id: UserId::new(),
            name: "guest".into(),
            password_hash: hash,
            policy: UserPolicy::default(),
        })
        .await
        .unwrap();
    stores
        .put(MediaItem {
            id: 7,
            path: movie,
            title: "Movie".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(10_000),
                width: Some(1280),
                height: Some(720),
                bitrate_bps: Some(1_500_000),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
    stores
        .set_artwork(7, "Primary", "tmdb", &poster.to_string_lossy())
        .await
        .unwrap();
    let state = AppState::new(stores, "pharos-media".into())
        .with_image_cache(pharos_cache::ImageCache::new(dir.path().join("images")))
        .with_hls_cache(pharos_cache::HlsSegmentCache::new(
            dir.path().join("hls"),
            32 * 1024 * 1024,
        ));
    Rig {
        state: web::Data::new(state),
        _dir: dir,
    }
}

fn bootstrap_client(rig: &Rig, media_ttl_secs: Option<u32>) -> bootstrap::Client {
    let ctx = rpc::ConnectionCtx::admit(None, LOCAL, &rpc::limits::ConnectionLimits::default(), 16)
        .unwrap();
    if let Some(secs) = media_ttl_secs {
        ctx.set_media_token_ttl_secs(secs);
    }
    let conn = Rc::new(Connection::new());
    rpc::bootstrap_client(rig.state.clone(), ctx, &conn)
}

/// The token and its expiry, or the error's kind name.
async fn media_token(session: &session::Client) -> Result<(String, u64), &'static str> {
    let mut req = session.media_token_request();
    req.get().init_ctx();
    let reply = req.send().promise.await.unwrap();
    match reply.get().unwrap().get_result().unwrap().which().unwrap() {
        media_token_result::Token(t) => {
            let t = t.unwrap();
            Ok((
                t.get_token().unwrap().to_string().unwrap(),
                t.get_expires_at_ms(),
            ))
        }
        media_token_result::Error(e) => Err(match e.unwrap().get_kind().which().unwrap() {
            error::kind::Revoked(()) => "revoked",
            error::kind::Unavailable(_) => "unavailable",
            _ => "other",
        }),
    }
}

async fn signed_in(rig: &Rig, media_ttl_secs: Option<u32>) -> (session::Client, bootstrap::Client) {
    let client = bootstrap_client(rig, media_ttl_secs);
    let (session, _) = login(&client, "guest", "hunter2").await.ok().unwrap();
    (session, client)
}

/// The status `method uri` answers; an extractor's error counts as its status.
macro_rules! status {
    ($app:expr, $method:expr, $uri:expr) => {{
        let req = test::TestRequest::default()
            .method($method.parse().unwrap())
            .uri($uri)
            .to_request();
        match test::try_call_service(&$app, req).await {
            Ok(resp) => resp.status(),
            Err(e) => e.as_response_error().status_code(),
        }
    }};
}

#[actix_web::test]
async fn a_media_token_loads_images_and_streams() {
    let rig = rig().await;
    let (session, _) = signed_in(&rig, None).await;
    let (token, expires_at_ms) = media_token(&session).await.unwrap();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    assert!(
        expires_at_ms > now_ms + 21_000_000 && expires_at_ms <= now_ms + 21_700_000,
        "the default lifetime is six hours: {expires_at_ms} vs {now_ms}"
    );
    let app = test::init_service(build_app(rig.state.clone())).await;

    let image = format!("/Items/7/Images/Primary?api_key={token}");
    let resp = test::call_service(&app, test::TestRequest::get().uri(&image).to_request()).await;
    let st = resp.status();
    let b = test::read_body(resp).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(b.as_ref(), POSTER);
    assert_eq!(status!(app, "HEAD", &image), StatusCode::OK);

    let stream = format!("/Videos/7/stream?static=true&api_key={token}");
    let resp = test::call_service(&app, test::TestRequest::get().uri(&stream).to_request()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(test::read_body(resp).await.as_ref(), MOVIE);

    // The master playlist, and the variant playlist it points at, both
    // answer, and every child URL carries the media token, not a Full one.
    let master = format!("/Videos/7/master.m3u8?api_key={token}");
    let body =
        test::call_and_read_body(&app, test::TestRequest::get().uri(&master).to_request()).await;
    let master_text = String::from_utf8(body.to_vec()).unwrap();
    let variant = master_text
        .lines()
        .find(|l| !l.starts_with('#') && !l.is_empty())
        .unwrap_or_else(|| panic!("no variant in:\n{master_text}"));
    assert!(
        variant.contains(&token),
        "child URL lost the token: {variant}"
    );
    assert_eq!(status!(app, "GET", variant), StatusCode::OK);
}

/// The master playlist's first segment, over the real transcoder.
#[actix_web::test]
#[ignore = "requires ffmpeg + PHAROS_TEST_FIXTURES"]
async fn a_media_token_fetches_an_hls_segment() {
    let Some(fixtures) = std::env::var_os("PHAROS_TEST_FIXTURES") else {
        eprintln!("skipping: no PHAROS_TEST_FIXTURES");
        return;
    };
    let rig = rig().await;
    let video = std::path::PathBuf::from(fixtures).join("video.webm");
    assert!(video.exists(), "fixture video.webm is missing");
    rig.state
        .stores
        .put(MediaItem {
            id: 8,
            path: video,
            title: "Clip".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(3_000),
                width: Some(320),
                height: Some(240),
                bitrate_bps: Some(500_000),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
    let (session, _) = signed_in(&rig, None).await;
    let (token, _) = media_token(&session).await.unwrap();
    let app = test::init_service(build_app(rig.state.clone())).await;
    let get = |uri: String| test::TestRequest::get().uri(&uri).to_request();

    let master =
        test::call_and_read_body(&app, get(format!("/Videos/8/master.m3u8?api_key={token}"))).await;
    let first_uri = |playlist: &[u8]| {
        String::from_utf8(playlist.to_vec())
            .unwrap()
            .lines()
            .find(|l| !l.starts_with('#') && !l.is_empty())
            .unwrap()
            .to_owned()
    };
    let variant = first_uri(&master);
    let playlist = test::call_and_read_body(&app, get(variant)).await;
    let segment = first_uri(&playlist);
    assert!(
        segment.contains(&token),
        "segment URL lost the token: {segment}"
    );
    let resp = test::call_service(&app, get(segment)).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[actix_web::test]
async fn a_media_token_is_refused_everywhere_else() {
    let rig = rig().await;
    let (session, client) = signed_in(&rig, None).await;
    let (token, _) = media_token(&session).await.unwrap();
    let app = test::init_service(build_app(rig.state.clone())).await;

    let mut wrong = Vec::new();
    for (method, uri) in [
        ("GET", "/Items"),
        ("GET", "/Users/Me"),
        ("GET", "/Items/7"),
        ("POST", "/Items/7/PlaybackInfo"),
        ("DELETE", "/Items/7/Images/Primary"),
        ("GET", "/Sessions"),
    ] {
        let sep = if uri.contains('?') { '&' } else { '?' };
        let uri = format!("{uri}{sep}api_key={token}");
        let status = status!(app, method, &uri);
        if status != StatusCode::FORBIDDEN && status != StatusCode::UNAUTHORIZED {
            wrong.push(format!("{method} {uri}: {status}"));
        }
    }
    assert!(wrong.is_empty(), "a media token got through:\n{wrong:?}");

    // It cannot resume a native session either.
    let mut req = client.resume_request();
    req.get().init_ctx();
    req.get().set_device_token(token.as_str());
    let reply = req.send().promise.await.unwrap();
    let status = reply
        .get()
        .unwrap()
        .get_result()
        .unwrap()
        .get_status()
        .unwrap();
    let done::Error(err) = status.which().unwrap() else {
        panic!("a media token must not resume a session");
    };
    assert!(matches!(
        err.unwrap().get_kind().which().unwrap(),
        error::kind::Revoked(()) | error::kind::NotPermitted(_)
    ));
}

#[actix_web::test]
async fn a_media_token_stops_working_when_its_lifetime_ends() {
    let rig = rig().await;
    let (session, _) = signed_in(&rig, Some(1)).await;
    let (token, _) = media_token(&session).await.unwrap();
    let app = test::init_service(build_app(rig.state.clone())).await;
    // Images stay anonymous (B286), so the stream is what shows the expiry.
    let stream = format!("/Videos/7/stream?static=true&api_key={token}");
    assert_eq!(status!(app, "GET", &stream), StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(2200)).await;
    assert_eq!(status!(app, "GET", &stream), StatusCode::UNAUTHORIZED);
}

#[actix_web::test]
async fn a_logged_out_session_gets_no_media_token() {
    let rig = rig().await;
    let (session, _) = signed_in(&rig, None).await;
    let mut req = session.logout_request();
    req.get().init_ctx();
    req.send().promise.await.unwrap();
    assert_eq!(media_token(&session).await, Err("revoked"));
}
