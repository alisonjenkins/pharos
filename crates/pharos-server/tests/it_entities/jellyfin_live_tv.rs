#![allow(clippy::unwrap_used, clippy::expect_used)]
//! /LiveTv endpoints — Channels list, single channel, stream redirect,
//! Programs EPG (T47).

use actix_web::{test, web, App};
use pharos_core::{SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore};
use pharos_discovery::live_tv::M3uXmltvBackend;
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};
use tempfile::TempDir;

const SAMPLE_M3U: &str = r#"#EXTM3U
#EXTINF:-1 tvg-id="bbc1" tvg-logo="https://example/bbc.png" tvg-chno="1" group-title="UK",BBC One
http://example/bbc1.ts
#EXTINF:-1 tvg-id="cnn" group-title="News",CNN
http://example/cnn.ts
"#;

const SAMPLE_XMLTV: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<tv>
  <channel id="bbc1"><display-name>BBC One</display-name></channel>
  <programme channel="bbc1" start="20240101100000 +0000" stop="20240101110000 +0000">
    <title>News at Ten</title>
    <desc>Top stories.</desc>
  </programme>
</tv>
"#;

async fn seed_state_with_live_tv() -> (web::Data<AppState>, String, TempDir) {
    seed_state_with_m3u(SAMPLE_M3U).await
}

async fn seed_state_with_m3u(m3u: &str) -> (web::Data<AppState>, String, TempDir) {
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

    let td = TempDir::new().unwrap();
    let m3u_path = td.path().join("p.m3u");
    let xml_path = td.path().join("epg.xml");
    tokio::fs::write(&m3u_path, m3u).await.unwrap();
    tokio::fs::write(&xml_path, SAMPLE_XMLTV).await.unwrap();
    let backend = M3uXmltvBackend::new();
    backend.load_m3u(&m3u_path).await.unwrap();
    backend.load_xmltv(&xml_path).await.unwrap();

    let state = web::Data::new(AppState::new(stores, "t".into()).with_live_tv(backend));
    (state, token.0.expose().to_string(), td)
}

async fn seed_state_no_live_tv() -> (web::Data<AppState>, String) {
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
    let state = web::Data::new(AppState::new(stores, "t".into()));
    (state, token.0.expose().to_string())
}

fn build_app(
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
}

#[actix_web::test]
async fn channels_returns_parsed_m3u_entries() {
    let (state, token, _td) = seed_state_with_live_tv().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/LiveTv/Channels")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"], 2);
    let items = v["Items"].as_array().unwrap();
    assert_eq!(items[0]["Id"], "bbc1");
    assert_eq!(items[0]["Name"], "BBC One");
    assert_eq!(items[0]["ChannelNumber"], "1");
    assert_eq!(items[0]["Type"], "Channel");
    assert_eq!(items[1]["Id"], "cnn");
}

#[actix_web::test]
async fn single_channel_404_for_unknown_id() {
    let (state, token, _td) = seed_state_with_live_tv().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/LiveTv/Channels/nope")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 404);
}

/// A fake IPTV provider: `/live/ts` answers the stream only to the
/// credentials the playlist URL carries, `/live/m3u8` answers an HLS
/// playlist, `/live/plain` the same playlist as text/plain and `/live/gone`
/// a 503.
fn fake_provider() -> actix_test::TestServer {
    actix_test::start(|| {
        App::new()
            .route(
                "/live/ts",
                web::get().to(|req: actix_web::HttpRequest| async move {
                    let auth = req
                        .headers()
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    // base64("prov-user:prov-pass")
                    if auth != "Basic cHJvdi11c2VyOnByb3YtcGFzcw=="
                        || req.query_string() != "password=prov-pass"
                    {
                        return actix_web::HttpResponse::Unauthorized().body("no creds");
                    }
                    actix_web::HttpResponse::Ok()
                        .insert_header(("content-type", "video/mp2t"))
                        .insert_header(("set-cookie", "provider=prov-pass"))
                        .body(&b"TS-PAYLOAD"[..])
                }),
            )
            .route(
                "/live/m3u8",
                web::get().to(|| async {
                    actix_web::HttpResponse::Ok()
                        .insert_header(("content-type", "application/vnd.apple.mpegurl"))
                        .body("#EXTM3U\nseg-prov-pass-1.ts\n")
                }),
            )
            .route(
                "/live/plain",
                web::get().to(|| async {
                    actix_web::HttpResponse::Ok()
                        .insert_header(("content-type", "text/plain"))
                        .body("#EXTM3U\nhttp://prov-user:prov-pass@cdn/seg-1.ts\n")
                }),
            )
            .route(
                "/live/gone",
                web::get().to(|| async {
                    actix_web::HttpResponse::ServiceUnavailable().body("prov-pass down")
                }),
            )
    })
}

fn stream_outcomes(
    snapshotter: &pharos_test_metrics::CumulativeSnapshotter,
) -> std::collections::BTreeMap<String, u64> {
    let mut out = std::collections::BTreeMap::new();
    for (key, _, _, value) in snapshotter.snapshot().into_vec() {
        if key.key().name() != "pharos_live_tv_stream_total" {
            continue;
        }
        let outcome = key
            .key()
            .labels()
            .find(|l| l.key() == "outcome")
            .map(|l| l.value().to_string())
            .unwrap_or_default();
        if let metrics_util::debugging::DebugValue::Counter(n) = value {
            *out.entry(outcome).or_default() += n;
        }
    }
    out
}

/// B267: the stream is relayed, never redirected, so the provider
/// credentials in the playlist URL reach the provider and nothing else: no
/// `Location`, no provider header, no credential anywhere in the reply. An
/// HLS playlist upstream and an upstream error answer 502 without the
/// upstream body. Each outcome counts under `pharos_live_tv_stream_total`.
/// Disarm check: with the old 302 restored, the relayed request answers 302
/// with the credentialed URL in `Location`.
#[actix_web::test]
async fn channel_stream_relays_without_leaking_provider_credentials() {
    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);

    let provider = fake_provider();
    let addr = provider.addr();
    let m3u = format!(
        "#EXTM3U\n\
         #EXTINF:-1 tvg-id=\"ts\",TS\nhttp://prov-user:prov-pass@{addr}/live/ts?password=prov-pass\n\
         #EXTINF:-1 tvg-id=\"hls\",HLS\nhttp://prov-user:prov-pass@{addr}/live/m3u8\n\
         #EXTINF:-1 tvg-id=\"plain\",Plain\nhttp://prov-user:prov-pass@{addr}/live/plain\n\
         #EXTINF:-1 tvg-id=\"down\",Down\nhttp://prov-user:prov-pass@{addr}/live/gone\n"
    );
    let (state, token, _td) = seed_state_with_m3u(&m3u).await;
    let app = test::init_service(build_app(state)).await;

    let mut leaks = Vec::new();
    let mut statuses = Vec::new();
    for channel in ["ts", "hls", "plain", "down", "nope"] {
        let req = test::TestRequest::get()
            .uri(&format!("/LiveTv/Channels/{channel}/Stream"))
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request();
        let resp = test::call_service(&app, req).await;
        statuses.push((channel, resp.status().as_u16()));
        for (name, value) in resp.headers() {
            let value = value.to_str().unwrap_or_default();
            if value.contains("prov-")
                || name.as_str() == "location"
                || name.as_str() == "set-cookie"
            {
                leaks.push(format!("{channel}: header {name}: {value}"));
            }
        }
        let body = test::read_body(resp).await;
        if channel == "ts" {
            assert_eq!(&body[..], b"TS-PAYLOAD");
        } else if String::from_utf8_lossy(&body).contains("prov-") {
            leaks.push(format!("{channel}: body {body:?}"));
        }
    }
    assert!(
        leaks.is_empty(),
        "provider details reached the client:\n{}",
        leaks.join("\n")
    );
    assert_eq!(
        statuses,
        vec![
            ("ts", 200),
            ("hls", 502),
            ("plain", 502),
            ("down", 502),
            ("nope", 404)
        ]
    );
    let expected: std::collections::BTreeMap<String, u64> = [
        ("relayed", 1),
        ("playlist_unsupported", 2),
        ("upstream_status", 1),
        ("unknown_channel", 1),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    assert_eq!(stream_outcomes(&snapshotter), expected);
}

/// B1150: the channel stream is a `<video>` element URL, so the sign-in
/// cookie authenticates it and the web UI's URL carries no token. With no
/// credential at all it is still a 401. Disarm check: with the route back on
/// `AuthUser`, the cookie-only request answers 401.
#[actix_web::test]
async fn channel_stream_authenticates_with_the_sign_in_cookie() {
    let provider = fake_provider();
    let addr = provider.addr();
    let m3u = format!(
        "#EXTM3U\n#EXTINF:-1 tvg-id=\"ts\",TS\n\
         http://prov-user:prov-pass@{addr}/live/ts?password=prov-pass\n"
    );
    let (state, token, _td) = seed_state_with_m3u(&m3u).await;
    let app = test::init_service(build_app(state)).await;

    let req = test::TestRequest::get()
        .uri("/LiveTv/Channels/ts/Stream")
        .cookie(actix_web::cookie::Cookie::new(
            "JellyfinAuth",
            token.as_str(),
        ))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    // The provider picks the type; sandboxed so it never runs as a page.
    let csp = resp
        .headers()
        .get("content-security-policy")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        csp.contains("sandbox") && csp.contains("default-src 'none'"),
        "{csp}"
    );
    assert_eq!(
        resp.headers().get("x-content-type-options").unwrap(),
        "nosniff"
    );
    assert_eq!(&test::read_body(resp).await[..], b"TS-PAYLOAD");

    let req = test::TestRequest::get()
        .uri("/LiveTv/Channels/ts/Stream")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401);
}

#[actix_web::test]
async fn programs_in_window_returns_epg() {
    let (state, token, _td) = seed_state_with_live_tv().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/LiveTv/Programs?minStartDate=2024-01-01T00:00:00Z&maxEndDate=2024-01-02T00:00:00Z")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"], 1);
    let items = v["Items"].as_array().unwrap();
    assert_eq!(items[0]["Name"], "News at Ten");
    assert_eq!(items[0]["ChannelId"], "bbc1");
    assert_eq!(items[0]["StartDate"], "2024-01-01T10:00:00.000Z");
    assert_eq!(items[0]["EndDate"], "2024-01-01T11:00:00.000Z");
}

#[actix_web::test]
async fn info_reports_enabled_when_backend_present() {
    let (state, token, _td) = seed_state_with_live_tv().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/LiveTv/Info")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["IsEnabled"], true);
}

#[actix_web::test]
async fn info_reports_disabled_when_no_backend() {
    let (state, token) = seed_state_no_live_tv().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/LiveTv/Info")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["IsEnabled"], false);
}

#[actix_web::test]
async fn channels_empty_when_no_backend() {
    let (state, token) = seed_state_no_live_tv().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/LiveTv/Channels")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"], 0);
}

#[actix_web::test]
async fn recordings_timers_seriestimers_return_empty() {
    let (state, token, _td) = seed_state_with_live_tv().await;
    let app = test::init_service(build_app(state)).await;
    for path in [
        "/LiveTv/Recordings",
        "/LiveTv/Timers",
        "/LiveTv/SeriesTimers",
        "/LiveTv/TunerHosts",
    ] {
        let req = test::TestRequest::get()
            .uri(path)
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request();
        let body = test::call_and_read_body(&app, req).await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["TotalRecordCount"], 0, "{path}");
    }
}

#[actix_web::test]
async fn channels_requires_auth() {
    let (state, _token, _td) = seed_state_with_live_tv().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/LiveTv/Channels")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401);
}

#[actix_web::test]
async fn recommended_programs_is_registered_because_home_dies_on_its_404() {
    // B17: jellyfin-web's home "On Now" section calls
    // /LiveTv/Programs/Recommended with NO .catch; the rejection from a 404
    // propagates into the home page's Promise.all and blanks EVERY home
    // section (Next Up, Resume, Latest). The endpoint must always answer —
    // an empty result makes the section hide itself cleanly.
    let (state, token) = seed_state_no_live_tv().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/LiveTv/Programs/Recommended?IsAiring=true&limit=1")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200, "home-critical endpoint must not 404");
    let v: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(v["TotalRecordCount"], 0);
    assert!(v["Items"].as_array().is_some_and(|a| a.is_empty()));
}

/// B267: a user whose `EnableLiveTvAccess` is off meets live TV as if none
/// were configured: `IsEnabled` false, empty channel and programme lists,
/// 404 for a channel and its stream. Each route counts once under
/// `pharos_live_tv_access_denied_total{route}`; a permitted user is served.
/// Disarm check: with `backend_for` ignoring the policy, the denied user is
/// shown the channels.
#[actix_web::test]
async fn live_tv_access_off_reads_as_not_configured() {
    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);

    let (state, allowed_token, _td) = seed_state_with_live_tv().await;
    let uid = UserId::new();
    state
        .stores
        .create(UserRecord {
            id: uid,
            name: "no-tv".into(),
            password_hash: SecretString::new("x"),
            policy: UserPolicy {
                enable_live_tv_access: false,
                ..UserPolicy::default()
            },
        })
        .await
        .unwrap();
    let denied_token = state.stores.issue(uid, "t").await.unwrap();
    let denied_token = denied_token.0.expose().to_string();
    let app = test::init_service(build_app(state)).await;

    let get = |uri: &str, token: &str| {
        test::TestRequest::get()
            .uri(uri)
            .insert_header(("X-Emby-Token", token))
            .to_request()
    };
    let info: serde_json::Value =
        test::call_and_read_body_json(&app, get("/LiveTv/Info", &denied_token)).await;
    assert_eq!(info["IsEnabled"], false);
    assert_eq!(info["Services"], serde_json::json!([]));
    for uri in [
        "/LiveTv/Channels",
        "/LiveTv/Programs?minStartDate=2024-01-01T00:00:00Z&maxEndDate=2024-01-02T00:00:00Z",
    ] {
        let v: serde_json::Value =
            test::call_and_read_body_json(&app, get(uri, &denied_token)).await;
        assert_eq!(v["TotalRecordCount"], 0, "{uri}: {v}");
    }
    for uri in ["/LiveTv/Channels/bbc1", "/LiveTv/Channels/bbc1/Stream"] {
        let resp = test::call_service(&app, get(uri, &denied_token)).await;
        assert_eq!(resp.status(), 404, "{uri}");
    }
    let shown: serde_json::Value =
        test::call_and_read_body_json(&app, get("/LiveTv/Channels", &allowed_token)).await;
    assert_eq!(shown["TotalRecordCount"], 2);

    let mut denied = std::collections::BTreeMap::new();
    for (key, _, _, value) in snapshotter.snapshot().into_vec() {
        if key.key().name() != "pharos_live_tv_access_denied_total" {
            continue;
        }
        let route = key
            .key()
            .labels()
            .find(|l| l.key() == "route")
            .map(|l| l.value().to_string())
            .unwrap_or_default();
        if let metrics_util::debugging::DebugValue::Counter(n) = value {
            *denied.entry(route).or_insert(0) += n;
        }
    }
    let expected: std::collections::BTreeMap<String, u64> =
        ["channel", "channels", "info", "programs", "stream"]
            .into_iter()
            .map(|r| (r.to_string(), 1))
            .collect();
    assert_eq!(denied, expected);
}
