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
use pharos_server::services::auth::RevokeCause;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::oneshot;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::common::{seed_rich, Fixture};

async fn start() -> (actix_test::TestServer, Fixture) {
    start_with(NonZeroU32::MIN).await
}

/// A server whose timer re-checks every `revalidate_secs`.
async fn start_with(revalidate_secs: NonZeroU32) -> (actix_test::TestServer, Fixture) {
    let fx = seed_rich().await;
    let state = fx.state.clone();
    let limits = rpc::limits::ConnectionLimits::default();
    let server = actix_test::start(move || {
        let cfg = NativeApiConfig {
            enabled: true,
            revalidate_secs,
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
    connect_with(server, None, None).await
}

/// [`connect`], carrying an `Origin` and a `pharos_session` cookie as a
/// browser would.
pub(crate) async fn connect_with(
    server: &actix_test::TestServer,
    origin: Option<&str>,
    cookie: Option<&str>,
) -> (bootstrap::Client, oneshot::Receiver<Option<String>>) {
    let url = server.url("/pharos/rpc").replacen("http", "ws", 1);
    let mut ws = awc::Client::new().ws(url);
    if let Some(origin) = origin {
        ws = ws.origin(origin);
    }
    if let Some(cookie) = cookie {
        ws = ws.cookie(actix_web::cookie::Cookie::new(rpc::SESSION_COOKIE, cookie));
    }
    let (_resp, framed) = ws.connect().await.expect("upgrade");
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

pub(crate) async fn me_works(session: &session::Client) -> bool {
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
    assert!(
        matches!(closed.try_recv(), Err(oneshot::error::TryRecvError::Empty)),
        "a live connection was closed"
    );
    assert!(me_works(&session).await);
}

/// An hour between timer re-checks: only an announced revocation can close
/// the socket inside these tests (T025).
fn hourly() -> NonZeroU32 {
    NonZeroU32::new(3_600).unwrap()
}

/// T025: logout and revoke drop the connection at once, not at the next
/// re-check. Disarm check: without the `revocations` branch in the socket's
/// select, this times out.
#[actix_web::test]
async fn an_announced_revocation_closes_the_connection_at_once() {
    let (server, fx) = start_with(hourly()).await;
    let (client, closed) = connect(&server).await;
    let _session = signed_in(&client, &fx.user_token).await;

    pharos_server::services::auth::revoke_token(
        &fx.state,
        fx.user_id,
        &fx.user_token,
        RevokeCause::Logout,
    )
    .await
    .unwrap();

    let reason = tokio::time::timeout(Duration::from_secs(2), closed)
        .await
        .expect("an announced revocation must close the connection at once")
        .expect("close frame");
    assert_eq!(reason.as_deref(), Some("revoked"));
}

/// An announcement only makes the user's connections re-check: another of
/// their tokens, or another user's, keeps its connection.
#[actix_web::test]
async fn an_announcement_closes_only_connections_whose_token_no_longer_resolves() {
    let (server, fx) = start_with(hourly()).await;
    let (revoked, revoked_closed) = connect(&server).await;
    let _revoked_session = signed_in(&revoked, &fx.user_token).await;
    let (admin, mut admin_closed) = connect(&server).await;
    let admin_session = signed_in(&admin, &fx.admin_token).await;
    let second = fx
        .state
        .stores
        .issue(fx.user_id, "other-device")
        .await
        .unwrap();
    let (user, mut user_closed) = connect(&server).await;
    let user_session = signed_in(&user, second.0.expose()).await;

    pharos_server::services::auth::revoke_token(
        &fx.state,
        fx.user_id,
        &fx.user_token,
        RevokeCause::Logout,
    )
    .await
    .unwrap();

    let reason = tokio::time::timeout(Duration::from_secs(2), revoked_closed)
        .await
        .expect("the revoked token's connection must close at once")
        .expect("close frame");
    assert_eq!(reason.as_deref(), Some("revoked"));
    for closed in [&mut admin_closed, &mut user_closed] {
        assert!(
            matches!(closed.try_recv(), Err(oneshot::error::TryRecvError::Empty)),
            "a connection with a live token was closed"
        );
    }
    assert!(me_works(&admin_session).await);
    assert!(me_works(&user_session).await);
}

/// T025: `Session.logout` answers on its own connection (which stays open
/// for a new sign-in) and closes every other connection signed in with the
/// token at once. Disarm check: without `end_token` in `logout`, the
/// announcement closes the caller's own socket and the reply may be lost.
#[actix_web::test]
async fn logout_answers_and_closes_the_tokens_other_connections() {
    let (server, fx) = start_with(hourly()).await;
    let (caller, mut caller_closed) = connect(&server).await;
    let session = signed_in(&caller, &fx.user_token).await;
    let (other, other_closed) = connect(&server).await;
    let _other_session = signed_in(&other, &fx.user_token).await;

    let mut logout = session.logout_request();
    logout.get().init_ctx();
    let reply = tokio::time::timeout(Duration::from_secs(2), logout.send().promise)
        .await
        .expect("the logout reply must arrive")
        .unwrap();
    assert!(matches!(
        reply.get().unwrap().get_result().unwrap().which().unwrap(),
        pharos_api::pharos_capnp::done::Ok(())
    ));

    let reason = tokio::time::timeout(Duration::from_secs(2), other_closed)
        .await
        .expect("another connection with the token must close at once")
        .expect("close frame");
    assert_eq!(reason.as_deref(), Some("revoked"));
    assert!(!me_works(&session).await, "the logged-out session is dead");
    assert!(
        matches!(
            caller_closed.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ),
        "the caller's connection stays open"
    );
}
