//! Client core for the pharos native API (spec 017, phase 6).
//!
//! [`connect_stream`] runs capnp RPC over any byte stream and hands back the
//! `Bootstrap` capability and the RPC future the caller must drive. The
//! browser transport ([`browser::connect`], wasm32 only) builds that stream
//! from a WebSocket to `/pharos/rpc`. [`resume`] turns the connection into a
//! signed-in `Session` from the cookie `POST /pharos/session` set.

pub mod api;
#[cfg(target_arch = "wasm32")]
pub mod browser;
pub mod byte_stream;
pub mod group;
pub mod mix;

use capnp_rpc::{rpc_twoparty_capnp::Side, twoparty, RpcSystem};
use futures::io::{AsyncRead, AsyncWrite};
use pharos_api::pharos_capnp::{bootstrap, call_context, done, error, session};

/// What the client stamps on every call (`CallContext`).
#[derive(Debug, Clone, Default)]
pub struct CallInfo {
    /// W3C `traceparent` for this call; empty starts a new trace server-side.
    pub traceparent: String,
    pub ui_route: String,
    pub ui_action: String,
    pub client_version: String,
    pub device: String,
}

impl CallInfo {
    pub fn write(&self, mut ctx: call_context::Builder<'_>) {
        ctx.set_traceparent(&self.traceparent);
        ctx.set_ui_route(&self.ui_route);
        ctx.set_ui_action(&self.ui_action);
        ctx.set_client_version(&self.client_version);
        ctx.reborrow().init_device().set_value(&self.device);
        ctx.set_schema_version(pharos_api::pharos_capnp::SCHEMA_VERSION);
    }
}

/// A failed call: the transport broke, or the server answered an `Error`.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("rpc: {0}")]
    Rpc(#[from] capnp::Error),
    #[error("{kind}: {reason}")]
    Api {
        /// The `Error.kind` variant, e.g. `revoked`, `notFound`.
        kind: &'static str,
        reason: String,
        trace_id: String,
    },
}

impl ClientError {
    /// Read a server `Error` struct.
    pub fn from_api(e: error::Reader<'_>) -> Self {
        use error::kind::Which;
        let text = |t: capnp::Result<capnp::text::Reader<'_>>| {
            t.ok()
                .and_then(|t| t.to_str().ok())
                .unwrap_or_default()
                .to_owned()
        };
        let kind = match e.get_kind().which() {
            Ok(Which::NotFound(_)) => "notFound",
            Ok(Which::NotPermitted(_)) => "notPermitted",
            Ok(Which::Invalid(_)) => "invalid",
            Ok(Which::Limit(_)) => "limit",
            Ok(Which::Revoked(())) => "revoked",
            Ok(Which::Unavailable(_)) => "unavailable",
            Ok(Which::Other(())) | Err(_) => "other",
        };
        ClientError::Api {
            kind,
            reason: text(e.get_reason()),
            trace_id: text(e.get_trace_id()),
        }
    }

    /// Whether the server refused because no session was signed in or it
    /// was revoked, so the UI should show sign-in. Every sign-in refusal is
    /// `revoked`; `notPermitted` also covers signed-in refusals such as an
    /// unplayable item, so it does not count.
    pub fn needs_sign_in(&self) -> bool {
        matches!(
            self,
            ClientError::Api {
                kind: "revoked",
                ..
            }
        )
    }

    /// Whether the call failed because its connection is gone, as a call on
    /// a capability from a connection that has since dropped does.
    pub fn is_disconnected(&self) -> bool {
        matches!(self, ClientError::Rpc(e) if e.kind == capnp::ErrorKind::Disconnected)
    }
}

/// A `Done` result: ok, or the server's `Error`.
pub(crate) fn read_done(result: done::Reader<'_>) -> Result<(), ClientError> {
    match result.which().map_err(capnp::Error::from)? {
        done::Ok(()) => Ok(()),
        done::Error(e) => Err(ClientError::from_api(e?)),
    }
}

/// Run capnp RPC over a byte stream. The returned future must be driven
/// (spawned) for the connection to make progress; it finishes when the
/// stream closes.
pub fn connect_stream<R, W>(reader: R, writer: W) -> (bootstrap::Client, RpcSystem<Side>)
where
    R: AsyncRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
{
    let network = twoparty::VatNetwork::new(reader, writer, Side::Client, Default::default());
    let mut system = RpcSystem::new(Box::new(network), None);
    let client = system.bootstrap(Side::Server);
    (client, system)
}

/// `Bootstrap.resume` with an empty device token: the server signs the
/// connection in from the session cookie the upgrade carried.
pub async fn resume(
    bootstrap: &bootstrap::Client,
    info: &CallInfo,
) -> Result<session::Client, ClientError> {
    let mut req = bootstrap.resume_request();
    {
        let mut params = req.get();
        info.write(params.reborrow().init_ctx());
        params.set_device_token("");
    }
    let reply = req.send().promise.await?;
    let result = reply.get()?.get_result()?;
    read_done(result.get_status()?)?;
    Ok(result.get_session()?)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    fn api(kind: &'static str) -> ClientError {
        ClientError::Api {
            kind,
            reason: String::new(),
            trace_id: String::new(),
        }
    }

    /// A `Done` that carries an `Error` is that error, not success, so a
    /// refused report or stop surfaces.
    #[test]
    fn a_done_error_is_an_api_error() {
        let mut message = capnp::message::Builder::new_default();
        let mut out = message.init_root::<done::Builder<'_>>();
        out.reborrow().init_error().init_kind().set_revoked(());
        let err = read_done(out.into_reader()).expect_err("an error result");
        assert!(
            matches!(
                err,
                ClientError::Api {
                    kind: "revoked",
                    ..
                }
            ),
            "{err:?}"
        );

        let mut message = capnp::message::Builder::new_default();
        let mut out = message.init_root::<done::Builder<'_>>();
        out.set_ok(());
        assert!(read_done(out.into_reader()).is_ok());
    }

    /// The server answers `notPermitted` to signed-in users too (an
    /// unplayable item, a pending quick-connect), so only `revoked` sends
    /// the UI to sign-in.
    #[test]
    fn only_a_revoked_session_needs_sign_in() {
        assert!(api("revoked").needs_sign_in());
        for kind in [
            "notPermitted",
            "notFound",
            "invalid",
            "limit",
            "unavailable",
            "other",
        ] {
            assert!(!api(kind).needs_sign_in(), "{kind}");
        }
    }

    /// Only a lost connection is a disconnect: the server's own refusals and
    /// other transport failures still warn.
    #[test]
    fn only_a_lost_connection_is_disconnected() {
        assert!(
            ClientError::from(capnp::Error::disconnected("Peer disconnected".into()))
                .is_disconnected()
        );
        assert!(!ClientError::from(capnp::Error::failed("boom".into())).is_disconnected());
        assert!(!api("unavailable").is_disconnected());
    }
}
