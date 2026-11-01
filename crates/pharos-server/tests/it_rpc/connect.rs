//! T021/T022: the `/pharos/rpc` WebSocket upgrade. A page on another origin
//! is refused before the upgrade (cross-site WebSocket hijacking); a native
//! client (no `Origin`) or this server's own origin connects, and capnp runs
//! over the socket's binary messages.

use actix_web::App;
use awc::ws;
use capnp_rpc::{rpc_twoparty_capnp::Side, twoparty, RpcSystem};
use futures_util::{SinkExt as _, StreamExt as _};
use pharos_api::pharos_capnp::{bootstrap, server_info_result};
use pharos_server::api::pharos::rpc;
use pharos_server::config::NativeApiConfig;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::common::seed_rich;

async fn start(allowed_origins: Vec<String>) -> actix_test::TestServer {
    let fx = seed_rich().await;
    let state = fx.state;
    let limits = rpc::limits::ConnectionLimits::default();
    actix_test::start(move || {
        let cfg = NativeApiConfig {
            enabled: true,
            allowed_origins: allowed_origins.clone(),
            ..NativeApiConfig::default()
        };
        App::new()
            .app_data(state.clone())
            .configure(rpc::configure(cfg, limits.clone()))
    })
}

fn ws_url(server: &actix_test::TestServer) -> String {
    server.url("/pharos/rpc").replacen("http", "ws", 1)
}

/// A capnp client over a WebSocket: binary messages in and out of a pipe.
async fn connect(server: &actix_test::TestServer, origin: Option<&str>) -> bootstrap::Client {
    let mut req = awc::Client::new().ws(ws_url(server));
    if let Some(o) = origin {
        req = req.origin(o);
    }
    let (_resp, framed) = req.connect().await.expect("upgrade");
    let (mut sink, mut stream) = framed.split();
    let (rpc_end, pump_end) = tokio::io::duplex(64 * 1024);
    let (mut from_rpc, mut to_rpc) = tokio::io::split(pump_end);
    actix_web::rt::spawn(async move {
        let mut buf = vec![0u8; 64 * 1024];
        while let Ok(n) = from_rpc.read(&mut buf).await {
            if n == 0
                || sink
                    .send(ws::Message::Binary(buf[..n].to_vec().into()))
                    .await
                    .is_err()
            {
                break;
            }
        }
    });
    actix_web::rt::spawn(async move {
        while let Some(Ok(frame)) = stream.next().await {
            if let ws::Frame::Binary(bytes) = frame {
                if to_rpc.write_all(&bytes).await.is_err() {
                    break;
                }
            }
        }
    });
    let (r, w) = tokio::io::split(rpc_end);
    let network = twoparty::VatNetwork::new(
        r.compat(),
        w.compat_write(),
        Side::Client,
        Default::default(),
    );
    let mut system = RpcSystem::new(Box::new(network), None);
    let client: bootstrap::Client = system.bootstrap(Side::Server);
    actix_web::rt::spawn(async move {
        let _ = system.await;
    });
    client
}

async fn server_info(client: &bootstrap::Client) -> u32 {
    let mut req = client.server_info_request();
    req.get().init_ctx();
    let reply = req
        .send()
        .promise
        .await
        .expect("serverInfo over the socket");
    match reply.get().unwrap().get_result().unwrap().which().unwrap() {
        server_info_result::Info(info) => info.unwrap().get_schema_version(),
        server_info_result::Error(_) => panic!("serverInfo failed"),
    }
}

#[actix_web::test]
async fn a_native_client_without_origin_connects_and_calls() {
    let server = start(Vec::new()).await;
    let client = connect(&server, None).await;
    assert_eq!(
        server_info(&client).await,
        pharos_api::pharos_capnp::SCHEMA_VERSION
    );
}

/// DNS rebinding: a page whose Origin names the very host it reached is not
/// trusted for that; only `allowed_origins` admits a browser.
#[actix_web::test]
async fn an_origin_matching_the_host_is_refused_unless_listed() {
    let server = start(Vec::new()).await;
    let own = server.url("").trim_end_matches('/').to_owned();
    let refused = awc::Client::new()
        .ws(ws_url(&server))
        .origin(own.as_str())
        .connect()
        .await;
    assert!(
        matches!(refused, Err(awc::error::WsClientError::InvalidResponseStatus(s)) if s == 403),
        "an unlisted origin must be refused even when it matches Host"
    );
    let server = start(vec![own.clone()]).await;
    let client = connect(&server, Some(&own)).await;
    assert_eq!(
        server_info(&client).await,
        pharos_api::pharos_capnp::SCHEMA_VERSION
    );
}

#[actix_web::test]
async fn a_listed_origin_connects() {
    let server = start(vec!["https://ui.example.org".into()]).await;
    let client = connect(&server, Some("https://ui.example.org")).await;
    assert_eq!(
        server_info(&client).await,
        pharos_api::pharos_capnp::SCHEMA_VERSION
    );
}

#[actix_web::test]
async fn another_sites_page_is_refused_before_the_upgrade() {
    let server = start(Vec::new()).await;
    let refused = awc::Client::new()
        .ws(ws_url(&server))
        .origin("https://evil.example")
        .connect()
        .await;
    match refused {
        Err(awc::error::WsClientError::InvalidResponseStatus(status)) => assert_eq!(status, 403),
        Err(e) => panic!("expected a 403, got {e}"),
        Ok(_) => panic!("a cross-site upgrade must be refused"),
    }
}

/// The endpoint speaks binary capnp only; a text frame closes the socket
/// with `Unsupported` rather than being ignored.
#[actix_web::test]
async fn a_text_frame_closes_the_connection() {
    let server = start(Vec::new()).await;
    let (_resp, mut framed) = awc::Client::new()
        .ws(ws_url(&server))
        .connect()
        .await
        .expect("upgrade");
    framed
        .send(ws::Message::Text("hello".into()))
        .await
        .unwrap();
    let mut closed = None;
    while let Some(Ok(frame)) = framed.next().await {
        if let ws::Frame::Close(reason) = frame {
            closed = Some(reason);
            break;
        }
    }
    let reason = closed.expect("server closes").expect("with a reason");
    assert_eq!(reason.code, ws::CloseCode::Unsupported);
    assert_eq!(reason.description.as_deref(), Some("text_frame"));
}
