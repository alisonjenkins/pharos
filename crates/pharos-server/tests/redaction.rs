#![allow(clippy::unwrap_used, clippy::expect_used)]
//! T44 — V8 ("auth tokens never logged") integration test.
//!
//! Drives a full Jellyfin auth flow (AuthenticateByName -> /Users/Me)
//! against an in-process pharos, capturing every `tracing` event the
//! handlers + extractors + sqlx layer emit. Asserts the issued bearer
//! token never appears in any captured event byte. Catches future
//! regressions where someone fmt-prints a SecretString via `.expose()`
//! into a tracing field.

use actix_test::TestServer;
use actix_web::{web, App};
use pharos_core::{SecretString, UserId, UserPolicy, UserRecord, UserStore};
use pharos_jellyfin_test_client::{DeviceInfo, JellyfinClient};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};
use pharos_sync::GroupRegistry;
use std::sync::{Arc, Mutex};
use tracing_subscriber::layer::SubscriberExt;

/// `MakeWriter` impl that funnels every formatted record into a shared
/// byte buffer. Lets the assertion at the end of the test grep the
/// complete captured stream.
#[derive(Clone)]
struct VecWriter(Arc<Mutex<Vec<u8>>>);

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for VecWriter {
    type Writer = SharedBuf;
    fn make_writer(&'a self) -> Self::Writer {
        SharedBuf(self.0.clone())
    }
}

struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for SharedBuf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

async fn boot() -> (TestServer, String) {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("hunter2")).unwrap();
    stores
        .create(UserRecord {
            id: UserId::new(),
            name: "ali".into(),
            password_hash: hash,
            policy: UserPolicy {
                admin: true,
                ..Default::default()
            },
        })
        .await
        .unwrap();
    let state = web::Data::new(AppState::new(stores, "pharos-redact".into()));
    let member_sinks = pharos_sync::MemberSinks::new();
    let registry = web::Data::new(GroupRegistry::spawn(std::sync::Arc::new(
        pharos_sync::LocalDelivery::new(member_sinks.clone()),
    )));
    let member_sinks = web::Data::new(member_sinks);
    let server = actix_test::start(move || {
        App::new()
            .app_data(state.clone())
            .app_data(registry.clone())
            .app_data(web::Data::new(pharos_sync::SessionHub::new()))
            .app_data(member_sinks.clone())
            .wrap(LowercasePath)
            .configure(jellyfin::configure)
    });
    let url = server.url("").trim_end_matches('/').to_string();
    (server, url)
}

#[test]
fn auth_flow_never_logs_token() {
    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let writer = VecWriter(buf.clone());

    // Subscriber routes every event/span field through `writer` at
    // DEBUG so even diagnostic spans the handlers may emit are
    // captured. set_global_default panics if another subscriber is
    // already installed in the test process — we live with that for a
    // single dedicated redaction test binary.
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_writer(writer)
            .with_target(true)
            .with_level(true),
    );
    let _guard = tracing::subscriber::set_default(subscriber);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let issued_token = rt.block_on(async {
        let (_server, base) = boot().await;
        let mut client = JellyfinClient::new(base, DeviceInfo::default());
        let auth = client
            .authenticate_by_name("ali", "hunter2")
            .await
            .expect("authenticate");
        let token = auth.access_token.clone();
        let _ = client.users_me().await.unwrap();
        let _ = client.items().await.unwrap();
        let _ = client.sessions().await.unwrap();
        token
    });

    drop(_guard);

    let captured = String::from_utf8_lossy(&buf.lock().unwrap().clone()).into_owned();

    // The exact issued token must not appear in any captured event.
    assert!(
        !captured.contains(&issued_token),
        "token bytes leaked into tracing stream:\n{captured}",
    );

    // Defence-in-depth: regex-scan for any 32-char lowercase hex chunk
    // outside known-public id contexts (user.id, server_id, etc.).
    let re = regex_lite::Regex::new(r"\b[0-9a-f]{32}\b").unwrap();
    for hit in re.find_iter(&captured) {
        let lo = hit.start().saturating_sub(40);
        let hi = (hit.end() + 40).min(captured.len());
        let context = &captured[lo..hi];
        if context.contains("user.id")
            || context.contains("user_id")
            || context.contains("server_id")
            || context.contains("session.id")
            || context.contains("session_id")
            || context.contains("UserId")
            || context.contains("SessionInfo")
            || context.contains("media.id")
            || context.contains("device")
        {
            continue;
        }
        panic!("32-char hex matched a non-id context — possible token leak:\n  …{context}…",);
    }
}

/// B247: a token sent as `?api_key=` (how element URLs authenticate) is
/// masked in the request span's `http.target`, which the error-status WARN
/// prints and every trace exports. Disarm check: with the stock
/// `tracing_actix_web::root_span!` in `StatusRootSpanBuilder`, the token
/// appears in the captured line.
#[actix_web::test]
async fn query_token_is_masked_in_the_request_span() {
    use actix_web::test;
    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_writer(VecWriter(buf.clone()))
            .with_ansi(false),
    );
    let _guard = tracing::subscriber::set_default(subscriber);

    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let state = web::Data::new(AppState::new(stores, "pharos-redact".into()));
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(tracing_actix_web::TracingLogger::<
                pharos_server::obs::StatusRootSpanBuilder,
            >::new())
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    let secret = "querytokenmustnotbelogged";
    let req = test::TestRequest::get()
        .uri(&format!("/Users/Me?maxWidth=300&api_key={secret}"))
        .to_request();
    let status = test::call_service(&app, req).await.status();
    assert_eq!(status.as_u16(), 401);

    let captured = String::from_utf8_lossy(&buf.lock().unwrap().clone()).into_owned();
    assert!(
        captured.contains("http request completed with error status"),
        "no request line captured:\n{captured}"
    );
    assert!(!captured.contains(secret), "token in the log:\n{captured}");
    assert!(
        captured.contains("api_key=REDACTED"),
        "target not masked:\n{captured}"
    );
}

/// B256: a PlaySessionId authorizes a stream on its own, so it is a
/// credential and the logs carry only its first 8 characters. Negotiating,
/// streaming with it and stopping it leaves the decision line joinable by
/// that prefix and the whole id nowhere, and so does a playback superseded
/// on the same device.
/// Disarm check: logging the full id in the decision line, or letting the
/// sqlite store spans record their `play_session_id` argument, puts it in the
/// captured log.
#[actix_web::test]
async fn play_session_id_is_logged_by_prefix_only() {
    use actix_web::test;
    use pharos_core::{MediaItem, MediaKind, MediaProbe, MediaStore as _, TokenStore as _};
    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_writer(VecWriter(buf.clone()))
            .with_ansi(false)
            // Span fields too: the store spans name the session they act on.
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NEW),
    );
    let _guard = tracing::subscriber::set_default(subscriber);

    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "u".into(),
            password_hash: SecretString::new("x"),
            policy: UserPolicy::default(),
        })
        .await
        .unwrap();
    let token = stores.issue(uid, "t").await.unwrap();
    let token = token.0.expose();
    stores
        .put(MediaItem {
            id: 1,
            path: "/m/1.mp4".into(),
            title: "V".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(60_000),
                container: Some("mp4".into()),
                video_codec: Some("h264".into()),
                audio_codec: Some("aac".into()),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
    let state = web::Data::new(AppState::new(stores, "pharos-redact".into()));
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(tracing_actix_web::TracingLogger::<
                pharos_server::obs::StatusRootSpanBuilder,
            >::new())
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;

    let profile = serde_json::json!({ "DeviceProfile": {
        "DirectPlayProfiles": [
            {"Container": "mp4", "Type": "Video", "VideoCodec": "h264", "AudioCodec": "aac"}
        ],
    }});
    let info: serde_json::Value = test::call_and_read_body_json(
        &app,
        test::TestRequest::post()
            .uri("/Items/1/PlaybackInfo")
            .insert_header(("X-Emby-Token", token))
            .insert_header(("User-Agent", "Jellyfin Android TV/0.19.9 (OkHttp/4.12.0)"))
            .set_json(profile)
            .to_request(),
    )
    .await;
    let psid = info["PlaySessionId"].as_str().unwrap().to_owned();
    let stream = test::TestRequest::get()
        .uri(&format!("/videos/1/stream?static=true&tag={psid}"))
        .to_request();
    test::call_service(&app, stream).await;
    let stop = test::TestRequest::post()
        .uri("/Sessions/Playing/Stopped")
        .insert_header(("X-Emby-Token", token))
        .set_json(serde_json::json!({ "PlaySessionId": psid, "ItemId": "1" }))
        .to_request();
    assert!(test::call_service(&app, stop).await.status().is_success());
    // A new playback on the same device supersedes the old session, and
    // that line names both.
    let superseded = "supersededplaysessionone";
    let superseding = "supersedingplaysessiontwo";
    for id in [superseded, superseding] {
        let start = test::TestRequest::post()
            .uri("/Sessions/Playing")
            .insert_header(("X-Emby-Token", token))
            .set_json(serde_json::json!({ "PlaySessionId": id, "ItemId": "1" }))
            .to_request();
        assert!(test::call_service(&app, start).await.status().is_success());
    }
    // The registry applies events in order, so a read answers after both.
    let list = test::TestRequest::get()
        .uri("/Sessions")
        .insert_header(("X-Emby-Token", token))
        .to_request();
    test::call_service(&app, list).await;

    let captured = String::from_utf8_lossy(&buf.lock().unwrap().clone()).into_owned();
    assert!(
        captured.contains(&format!("play_session_id={}", &psid[..8])),
        "no decision line joinable by the prefix:\n{captured}"
    );
    assert!(
        captured.contains("superseded by new playback"),
        "no supersede line:\n{captured}"
    );
    for id in [psid.as_str(), superseded, superseding] {
        assert!(
            !captured.contains(id),
            "PlaySessionId {id} in the log:\n{captured}"
        );
    }
}

/// B267: a live TV playlist URL carries the provider's credentials, so the
/// stream relay's log lines (and reqwest's, at every level) name the channel,
/// never the URL, on success and on each failure.
/// Disarm check: with `without_url()` dropped from the unreachable-upstream
/// line, the credentials appear in the log.
#[actix_web::test]
async fn live_tv_stream_never_logs_the_provider_url() {
    use actix_web::test;
    use pharos_core::TokenStore as _;
    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_writer(VecWriter(buf.clone()))
            .with_ansi(false)
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NEW),
    );
    let _guard = tracing::subscriber::set_default(subscriber);

    let provider = actix_test::start(|| {
        App::new()
            .route(
                "/live/ok",
                web::get().to(|| async {
                    actix_web::HttpResponse::Ok()
                        .insert_header(("content-type", "video/mp2t"))
                        .body("TS")
                }),
            )
            .route(
                "/live/gone",
                web::get().to(|| async { actix_web::HttpResponse::ServiceUnavailable().finish() }),
            )
    });
    let addr = provider.addr();
    // A bound-then-dropped listener leaves a port nothing answers on.
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let m3u = format!(
        "#EXTM3U\n\
         #EXTINF:-1 tvg-id=\"ok\",Ok\nhttp://prov-user:prov-pass@{addr}/live/ok?password=prov-pass\n\
         #EXTINF:-1 tvg-id=\"gone\",Gone\nhttp://prov-user:prov-pass@{addr}/live/gone?password=prov-pass\n\
         #EXTINF:-1 tvg-id=\"dead\",Dead\nhttp://prov-user:prov-pass@{closed}/live/dead?password=prov-pass\n"
    );
    let td = tempfile::TempDir::new().unwrap();
    let m3u_path = td.path().join("p.m3u");
    std::fs::write(&m3u_path, m3u).unwrap();
    let backend = pharos_discovery::live_tv::M3uXmltvBackend::new();
    backend.load_m3u(&m3u_path).await.unwrap();

    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "u".into(),
            password_hash: SecretString::new("x"),
            policy: UserPolicy::default(),
        })
        .await
        .unwrap();
    let token = stores.issue(uid, "t").await.unwrap();
    let token = token.0.expose().to_string();
    let state = web::Data::new(AppState::new(stores, "pharos-redact".into()).with_live_tv(backend));
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(tracing_actix_web::TracingLogger::<
                pharos_server::obs::StatusRootSpanBuilder,
            >::new())
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    for (channel, status) in [("ok", 200), ("gone", 502), ("dead", 502)] {
        let req = test::TestRequest::get()
            .uri(&format!("/LiveTv/Channels/{channel}/Stream"))
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status().as_u16(), status, "{channel}");
        test::read_body(resp).await;
    }

    let captured = String::from_utf8_lossy(&buf.lock().unwrap().clone()).into_owned();
    for line in [
        "live tv stream relayed",
        "live tv upstream refused the stream",
        "live tv upstream unreachable",
    ] {
        assert!(captured.contains(line), "no {line:?} line:\n{captured}");
    }
    assert!(
        !captured.contains("prov-"),
        "provider credentials in the log:\n{captured}"
    );
}
