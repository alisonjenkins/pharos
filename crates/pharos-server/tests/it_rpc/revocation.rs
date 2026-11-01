//! T025a: an open connection notices its token being revoked. Every
//! `revalidate_secs` it re-checks the tokens it resumed with; once one no
//! longer resolves the server closes the socket with cause `revoked`, so a
//! revoked device stops seeing the library without reconnecting.
//! Disarm check: with the re-check tick returning early, the revoked socket
//! stays open and `a_revoked_token_closes_its_open_connection` times out.

use std::num::NonZeroU32;
use std::time::Duration;

use actix_web::App;
use awc::ws;
use capnp_rpc::{rpc_twoparty_capnp::Side, twoparty, RpcSystem};
use futures_util::{SinkExt as _, StreamExt as _};
use pharos_api::pharos_capnp::{bootstrap, me_result, session};
use pharos_core::TokenStore;
use pharos_server::api::pharos::rpc;
use pharos_server::config::NativeApiConfig;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::oneshot;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::common::{seed_rich, Fixture};

async fn start() -> (actix_test::TestServer, Fixture) {
    let fx = seed_rich().await;
    let state = fx.state.clone();
    let limits = rpc::limits::ConnectionLimits::default();
    let server = actix_test::start(move || {
        let cfg = NativeApiConfig {
            enabled: true,
            revalidate_secs: NonZeroU32::MIN,
            ..NativeApiConfig::default()
        };
        App::new()
            .app_data(state.clone())
            .configure(rpc::configure(cfg, limits.clone()))
    });
    (server, fx)
}

/// A capnp client over the socket, and the description of the close frame
/// the server sends, if it sends one.
async fn connect(
    server: &actix_test::TestServer,
) -> (bootstrap::Client, oneshot::Receiver<Option<String>>) {
    let url = server.url("/pharos/rpc").replacen("http", "ws", 1);
    let (_resp, framed) = awc::Client::new().ws(url).connect().await.expect("upgrade");
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
    let (closed_tx, closed_rx) = oneshot::channel();
    actix_web::rt::spawn(async move {
        while let Some(Ok(frame)) = stream.next().await {
            match frame {
                ws::Frame::Binary(bytes) => {
                    if to_rpc.write_all(&bytes).await.is_err() {
                        break;
                    }
                }
                ws::Frame::Close(reason) => {
                    let _ = closed_tx.send(reason.and_then(|r| r.description));
                    return;
                }
                _ => {}
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
    (client, closed_rx)
}

async fn signed_in(client: &bootstrap::Client, token: &str) -> session::Client {
    let mut req = client.resume_request();
    req.get().init_ctx();
    req.get().set_device_token(token);
    let session = req.send().pipeline.get_result().get_session();
    assert!(me_works(&session).await, "resume with a live token");
    session
}

async fn me_works(session: &session::Client) -> bool {
    let mut me = session.me_request();
    me.get().init_ctx();
    match me.send().promise.await {
        Ok(reply) => matches!(
            reply.get().unwrap().get_result().unwrap().which().unwrap(),
            me_result::Me(_)
        ),
        Err(_) => false,
    }
}

#[actix_web::test]
async fn a_revoked_token_closes_its_open_connection() {
    let (server, fx) = start().await;
    let (client, closed) = connect(&server).await;
    let _session = signed_in(&client, &fx.user_token).await;

    fx.state.stores.revoke(&fx.user_token).await.unwrap();

    let reason = tokio::time::timeout(Duration::from_secs(5), closed)
        .await
        .expect("the server must close a revoked connection within a few re-checks")
        .expect("close frame");
    assert_eq!(reason.as_deref(), Some("revoked"));
}

#[actix_web::test]
async fn a_live_token_survives_its_re_checks() {
    let (server, fx) = start().await;
    let (client, mut closed) = connect(&server).await;
    let session = signed_in(&client, &fx.user_token).await;

    tokio::time::sleep(Duration::from_millis(2_500)).await;
    assert!(closed.try_recv().is_err(), "a live connection was closed");
    assert!(me_works(&session).await);
}
