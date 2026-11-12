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

pub(crate) async fn start() -> (actix_test::TestServer, Fixture) {
    start_with(NonZeroU32::MIN).await
}

/// A server that holds a released playback grant for `grace` before ending it.
pub(crate) async fn start_with_release_grace(
    grace: std::time::Duration,
) -> (actix_test::TestServer, Fixture) {
    let fx = crate::common::seed_rich_with(|s| s.with_release_grace(grace)).await;
    serve(fx, NonZeroU32::MIN)
}

/// A server whose timer re-checks every `revalidate_secs`.
async fn start_with(revalidate_secs: NonZeroU32) -> (actix_test::TestServer, Fixture) {
    serve(seed_rich().await, revalidate_secs)
}

/// A server holding `memo` as the integrity check's verdicts.
pub(crate) async fn start_with_integrity_memo(
    memo: pharos_server::integrity_memo::IntegrityMemo,
) -> (actix_test::TestServer, Fixture) {
    let memo = std::sync::Arc::new(memo);
    let fx = crate::common::seed_rich_with(|s| s.with_integrity_memo(memo)).await;
    serve(fx, NonZeroU32::MIN)
}

/// A server that enforces `VideoRangeType` conditions (T065).
pub(crate) async fn start_enforcing_video_range() -> (actix_test::TestServer, Fixture) {
    let fx = crate::common::seed_rich_with(|s| s.with_enforce_video_range(true)).await;
    serve(fx, NonZeroU32::MIN)
}

/// A server whose state also extracts image subtitles (spec 019). Its pool
/// never runs: nothing here asks for the images, only whether they can be.
#[cfg(feature = "ffmpeg-lib")]
pub(crate) async fn start_drawing_images(
    cache_root: &std::path::Path,
) -> (actix_test::TestServer, Fixture) {
    let cache = pharos_cache::subtitle_cache::SubtitleCache::new(1 << 20, 16)
        .with_disk(cache_root)
        .with_pool(pharos_transcode::worker::LibavWorkerPool::new(
            "/nonexistent/transcode-worker",
            1,
        ));
    let fx = crate::common::seed_rich_with(|s| s.with_subtitle_cache(cache)).await;
    serve(fx, NonZeroU32::MIN)
}

/// A server over an already-configured `fx`.
pub(crate) fn serve_fixture(fx: Fixture) -> (actix_test::TestServer, Fixture) {
    serve(fx, NonZeroU32::MIN)
}

fn serve(fx: Fixture, revalidate_secs: NonZeroU32) -> (actix_test::TestServer, Fixture) {
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
pub(crate) async fn connect(
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
    connect_tapped(server, origin, cookie, None).await
}

/// The bytes of each WebSocket frame in the order they crossed the socket,
/// `true` for client to server.
pub(crate) type Tap = std::rc::Rc<std::cell::RefCell<Vec<(bool, Vec<u8>)>>>;

/// [`connect_with`], recording every frame into `tap` when one is given.
pub(crate) async fn connect_tapped(
    server: &actix_test::TestServer,
    origin: Option<&str>,
    cookie: Option<&str>,
    tap: Option<Tap>,
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
    let sent = tap.clone();
    actix_web::rt::spawn(async move {
        let mut buf = vec![0u8; 64 * 1024];
        while let Ok(n) = from_rpc.read(&mut buf).await {
            if let Some(tap) = &sent {
                tap.borrow_mut().push((true, buf[..n].to_vec()));
            }
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
                    if let Some(tap) = &tap {
                        tap.borrow_mut().push((false, bytes.to_vec()));
                    }
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

pub(crate) async fn signed_in(client: &bootstrap::Client, token: &str) -> session::Client {
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
