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
pub mod limits;
pub mod origin;
mod session;
mod socket;

use std::cell::RefCell;
use std::rc::Rc;

use actix_web::web;
use capnp::message::ReaderOptions;
use capnp_rpc::{rpc_twoparty_capnp::Side, twoparty, RpcSystem};
use pharos_api::{pharos_capnp, server, Connection};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::config::NativeApiConfig;
use crate::state::AppState;

pub use socket::{record_closed, record_refused, CloseCause, Refused, SESSION_COOKIE};

/// Routes of the native API, or none at all while
/// `[server.native_api].enabled` is false: every push to main deploys
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
            .route("/pharos/rpc", web::get().to(socket::upgrade));
    }
}

/// Logs and publishes whether the native API is served; call once at startup.
pub fn record_state(cfg: &NativeApiConfig) {
    metrics::gauge!("pharos_native_api_enabled").set(f64::from(u8::from(cfg.enabled)));
    tracing::info!(
        enabled = cfg.enabled,
        allowed_origins = ?cfg.allowed_origins,
        max_connections_per_user = cfg.max_connections_per_user.get(),
        "native api (/pharos/rpc)"
    );
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
    limits: limits::ConnectionLimits,
    max_per_user: u32,
    slot: RefCell<limits::Slot>,
}

impl ConnectionCtx {
    /// A new connection's context, or `None` when the server already holds
    /// [`limits::MAX_UNAUTHENTICATED`] connections that have not signed in.
    pub fn admit(
        cookie_token: Option<String>,
        limits: &limits::ConnectionLimits,
        max_per_user: u32,
    ) -> Option<Rc<Self>> {
        let slot = limits.admit()?;
        Some(Rc::new(Self {
            cookie_token,
            limits: limits.clone(),
            max_per_user,
            slot: RefCell::new(slot),
        }))
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
        reader.compat(),
        writer.compat_write(),
        Side::Server,
        reader_options(),
    );
    RpcSystem::new(Box::new(network), Some(bootstrap.client)).await
}

#[cfg(test)]
mod tests {
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};

    use super::*;

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
            let value = snapshotter
                .snapshot()
                .into_vec()
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
