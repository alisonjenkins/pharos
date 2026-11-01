//! `/pharos/rpc`: the pharos-native Cap'n Proto API (spec 017, ADR-0028).
//!
//! [`bootstrap_client`] builds one connection's `Bootstrap` capability and
//! knows nothing of the transport; [`serve`] runs it over any byte stream.
//! Every capability is made through `pharos_api::server::<iface>::client`, so
//! every call is instrumented (V173).
//!
//! capnp-rpc is `!Send`: a connection's `RpcSystem` runs on the actix worker
//! that accepted it, and handlers reach shared state only through
//! [`AppState`] (research R2).

mod bootstrap;
mod library;
pub mod limits;
pub mod origin;
mod session;
pub mod socket;

use std::cell::RefCell;
use std::rc::Rc;

use actix_web::web;
use capnp::message::ReaderOptions;
use capnp_rpc::{rpc_twoparty_capnp::Side, twoparty, RpcSystem};
use pharos_api::{pharos_capnp, server, Connection};
use pharos_core::UserId;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::config::NativeApiConfig;
use crate::state::AppState;

pub use socket::{record_closed, record_refused, CloseCause, Recheck, Refused, SESSION_COOKIE};

/// Routes of the native API (`/pharos/rpc`, `/pharos/session`), or none at
/// all while `[server.native_api].enabled` is false: every push to main deploys
/// (ADR-0010), so the surface ships dark until it is complete. `limits` is
/// shared by every worker, so build it once per server.
pub fn configure(
    cfg: NativeApiConfig,
    limits: limits::ConnectionLimits,
) -> impl FnOnce(&mut web::ServiceConfig) {
    move |sc| {
        if !cfg.enabled {
            return;
        }
        sc.app_data(web::Data::new(cfg))
            .app_data(web::Data::new(limits))
            .route("/pharos/rpc", web::get().to(socket::upgrade))
            .service(
                web::resource("/pharos/session")
                    .app_data(web::JsonConfig::default().limit(super::session_cookie::MAX_BODY))
                    .route(web::post().to(super::session_cookie::sign_in))
                    .route(web::delete().to(super::session_cookie::sign_out)),
            );
    }
}

/// Logs and publishes whether the native API is served; call once at startup.
/// When it is, also starts every call's RED series at zero (so `rate()` works
/// from the first scrape) and parents call spans on the caller's
/// `traceparent`.
pub fn record_state(cfg: &NativeApiConfig) {
    metrics::gauge!("pharos_native_api_enabled").set(f64::from(u8::from(cfg.enabled)));
    if cfg.enabled {
        pharos_api::register_metrics();
        pharos_api::set_trace_parent_hook(crate::obs::rpc_trace_parent);
    }
    tracing::info!(
        enabled = cfg.enabled,
        allowed_origins = ?cfg.allowed_origins,
        max_connections_per_user = cfg.max_connections_per_user.get(),
        "native api (/pharos/rpc)"
    );
    for origin in origin::insecure(&cfg.allowed_origins) {
        tracing::warn!(
            origin,
            "native api: allowed origin is plain http; browsers drop the Secure \
             pharos_session cookie there, so sign-in from it cannot work. Serve the UI over https"
        );
    }
}

/// Words one incoming message may make a reader traverse (8 MiB). capnp's
/// default is 64 MiB; nothing in the schema comes near this.
pub const TRAVERSAL_LIMIT_WORDS: usize = 1 << 20;
/// Struct/list nesting one incoming message may have; the schema nests a
/// handful deep (capnp's default is 64).
pub const NESTING_LIMIT: i32 = 32;

/// Decode limits for every incoming message on a connection.
pub fn reader_options() -> ReaderOptions {
    let mut options = ReaderOptions::new();
    options
        .traversal_limit_in_words(Some(TRAVERSAL_LIMIT_WORDS))
        .nesting_limit(NESTING_LIMIT);
    options
}

/// What one connection brings to its `Bootstrap`.
pub struct ConnectionCtx {
    /// The `pharos_session` cookie the upgrade carried, used by `resume("")`.
    cookie_token: Option<String>,
    /// The client's address (`client_ip`, honouring `trusted_proxies`), for
    /// the login throttle's per-address limit (V174).
    client_ip: std::net::IpAddr,
    limits: limits::ConnectionLimits,
    max_per_user: u32,
    slot: RefCell<limits::Slot>,
    /// Every token a `resume` succeeded with, and its user, re-checked while
    /// the connection lives so revoking one ends it (T025a, T025).
    resumed: RefCell<Vec<(String, UserId)>>,
    /// Tokens `Session.logout` revoked on this connection: every session
    /// opened with one fails `revoked` from then on.
    ended: RefCell<Vec<String>>,
    /// Woken when a logout leaves the connection with no session, so the
    /// socket re-arms its auth deadline.
    pub(crate) signed_out: tokio::sync::Notify,
}

impl ConnectionCtx {
    /// A new connection's context, or `None` when the server already holds
    /// [`limits::MAX_UNAUTHENTICATED`] connections that have not signed in.
    pub fn admit(
        cookie_token: Option<String>,
        client_ip: std::net::IpAddr,
        limits: &limits::ConnectionLimits,
        max_per_user: u32,
    ) -> Option<Rc<Self>> {
        let slot = limits.admit()?;
        Some(Rc::new(Self {
            cookie_token,
            client_ip,
            limits: limits.clone(),
            max_per_user,
            slot: RefCell::new(slot),
            resumed: RefCell::new(Vec::new()),
            ended: RefCell::new(Vec::new()),
            signed_out: tokio::sync::Notify::new(),
        }))
    }

    fn remember_token(&self, token: String, user: UserId) {
        let mut resumed = self.resumed.borrow_mut();
        if !resumed.iter().any(|(t, _)| *t == token) {
            resumed.push((token, user));
        }
    }

    /// Ends `token` on this connection after `Session.logout` revoked it:
    /// its sessions here fail `revoked`, and it leaves the re-check, so the
    /// logout's own reply is not cut off by the close the revocation would
    /// cause. Other connections holding it still close (T025).
    /// When that leaves no session here, the connection is no longer signed
    /// in: its per-user slot is released (FR-010) and it has the auth
    /// deadline again to sign in or be closed.
    pub(crate) fn end_token(&self, token: &str) {
        let mut resumed = self.resumed.borrow_mut();
        resumed.retain(|(t, _)| t != token);
        let mut ended = self.ended.borrow_mut();
        if !ended.iter().any(|t| t == token) {
            ended.push(token.to_owned());
        }
        if resumed.is_empty() {
            self.limits.sign_out(&mut self.slot.borrow_mut());
            self.signed_out.notify_one();
        }
    }

    /// Puts the slot back in line with the sessions this connection holds,
    /// after a sign-in that failed once `admit` had moved it: not signed in
    /// when it holds none, else its last user's (already counted before).
    pub(crate) fn settle_slot(&self) {
        let last = self.resumed.borrow().last().map(|(_, user)| *user);
        let mut slot = self.slot.borrow_mut();
        match last {
            None => self.limits.sign_out(&mut slot),
            Some(user) => {
                // Back to a user it held a moment ago, so the cap can only
                // refuse if others filled it meanwhile; it then stays as is.
                let _ = self.limits.sign_in(&mut slot, user, u32::MAX);
            }
        }
    }

    /// Whether `Session.logout` ended `token` on this connection.
    pub(crate) fn has_ended(&self, token: &str) -> bool {
        self.ended.borrow().iter().any(|t| t == token)
    }

    /// The tokens this connection's sessions were resumed with: all of
    /// them, or only `user`'s.
    pub(crate) fn resumed_tokens(&self, user: Option<UserId>) -> Vec<String> {
        self.resumed
            .borrow()
            .iter()
            .filter(|(_, u)| user.map_or(true, |user| *u == user))
            .map(|(t, _)| t.clone())
            .collect()
    }

    /// Whether a session on this connection belongs to `user`.
    pub(crate) fn serves(&self, user: UserId) -> bool {
        self.resumed.borrow().iter().any(|(_, u)| *u == user)
    }

    /// Whether a `resume` has succeeded on this connection.
    pub fn is_signed_in(&self) -> bool {
        self.slot.borrow().is_signed_in()
    }
}

/// One connection's `Bootstrap` capability.
pub fn bootstrap_client(
    state: web::Data<AppState>,
    ctx: Rc<ConnectionCtx>,
    conn: &Rc<Connection>,
) -> pharos_capnp::bootstrap::Client {
    server::bootstrap::client(bootstrap::Bootstrap::new(state, ctx, Rc::clone(conn)), conn)
}

/// Serves `bootstrap` over one byte stream until the peer goes away.
pub async fn serve<R, W>(
    reader: R,
    writer: W,
    bootstrap: pharos_capnp::bootstrap::Client,
) -> Result<(), capnp::Error>
where
    R: AsyncRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
{
    let network = twoparty::VatNetwork::new(
        Counted::new(reader, Direction::In).compat(),
        Counted::new(writer, Direction::Out).compat_write(),
        Side::Server,
        reader_options(),
    );
    RpcSystem::new(Box::new(network), Some(bootstrap.client)).await
}

/// Which way bytes moved. The `direction` label of `pharos_rpc_bytes_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Read from the client.
    In,
    /// Written to the client.
    Out,
}

impl Direction {
    pub fn label(self) -> &'static str {
        match self {
            Direction::In => "in",
            Direction::Out => "out",
        }
    }
}

/// A reader or writer that counts the bytes through it under
/// `pharos_rpc_bytes_total{direction}` (017 §Observability).
struct Counted<T> {
    inner: T,
    bytes: metrics::Counter,
}

impl<T> Counted<T> {
    fn new(inner: T, direction: Direction) -> Self {
        Self {
            inner,
            bytes: metrics::counter!("pharos_rpc_bytes_total", "direction" => direction.label()),
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for Counted<T> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let polled = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        if let std::task::Poll::Ready(Ok(())) = polled {
            self.bytes.increment((buf.filled().len() - before) as u64);
        }
        polled
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Counted<T> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        data: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let polled = std::pin::Pin::new(&mut self.inner).poll_write(cx, data);
        if let std::task::Poll::Ready(Ok(n)) = polled {
            self.bytes.increment(n as u64);
        }
        polled
    }

    fn poll_write_vectored(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let polled = std::pin::Pin::new(&mut self.inner).poll_write_vectored(cx, bufs);
        if let std::task::Poll::Ready(Ok(n)) = polled {
            self.bytes.increment(n as u64);
        }
        polled
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};

    use super::*;

    #[test]
    fn direction_labels_are_distinct() {
        assert_ne!(Direction::In.label(), Direction::Out.label());
    }

    /// An operator asking "is the native API on in this pod?" gets the answer
    /// by query (`pharos_native_api_enabled`), not by reading config.
    #[test]
    fn startup_publishes_whether_the_native_api_is_served() {
        for enabled in [false, true] {
            let recorder = DebuggingRecorder::new();
            let snapshotter = pharos_test_metrics::snapshotter(&recorder);
            let cfg = NativeApiConfig {
                enabled,
                ..NativeApiConfig::default()
            };
            metrics::with_local_recorder(&recorder, || record_state(&cfg));
            let rows = snapshotter.snapshot().into_vec();
            // Disarm check: without `register_metrics` here, an enabled API
            // starts with no call series.
            assert_eq!(
                rows.iter()
                    .any(|(k, ..)| k.key().name() == "pharos_rpc_calls_total"),
                enabled,
                "call series at zero only when the API is served"
            );
            let value = rows
                .into_iter()
                .find(|(k, ..)| k.key().name() == "pharos_native_api_enabled")
                .map(|(.., v)| v);
            match value {
                Some(DebugValue::Gauge(v)) => {
                    assert_eq!(v.into_inner(), f64::from(u8::from(enabled)))
                }
                other => panic!("enabled={enabled}: gauge missing, got {other:?}"),
            }
        }
    }
}
