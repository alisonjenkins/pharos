//! The socket and the native API calls. The websocket-to-capnp pump and the
//! QuickConnect calls follow `crates/pharos-ui/src/native/android.rs`, which
//! cannot be depended on (it lives in the Dioxus app); the decisions
//! (backoff, what a poll answer means) are that app's own `tv_auth`, included
//! as is. Nothing here logs a token or a QuickConnect secret.

use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use pharos_api::pharos_capnp::{bootstrap, login_result, quick_connect_result, session};
use pharos_client::api::{Item, ItemKind, ItemRef, Listing, MediaToken, NativeApi, PharosApi};
use pharos_client::mix::MixSpec;
use pharos_client::{CallInfo, ClientError};
use tokio_tungstenite::tungstenite::Message;

use super::error::ContentError;
use super::tree::Catalogue;
use super::tv_auth::{self, Polled};

/// Turns true once the connection has ended; a call on it would hang.
pub type Closed = Rc<Cell<bool>>;

pub fn call_info() -> CallInfo {
    CallInfo {
        traceparent: String::new(),
        ui_route: "android-auto".into(),
        ui_action: String::new(),
        client_version: env!("CARGO_PKG_VERSION").into(),
        device: "android-auto".into(),
    }
}

/// The socket URL. `https://` goes through the TV app's own check (the
/// socket carries the QuickConnect secret and the device token). A loopback
/// `http://` base is also taken, for tests against a server in this process;
/// it never leaves the machine.
pub fn rpc_url(base: &str) -> Result<String, ContentError> {
    if let Some(rest) = base.strip_prefix("http://") {
        let host = rest.trim_end_matches('/');
        let name = host.rsplit_once(':').map_or(host, |(h, _)| h);
        if matches!(name, "127.0.0.1" | "localhost" | "[::1]") {
            return Ok(format!("ws://{host}/pharos/rpc"));
        }
    }
    tv_auth::rpc_url(base).map_err(ContentError::Url)
}

/// Open the socket and drive it on the current `LocalSet`.
pub async fn open(base: &str) -> Result<(bootstrap::Client, Closed), ContentError> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let url = rpc_url(base)?;
    let connect_err = |why: String| ContentError::Connect {
        url: url.clone(),
        why,
    };
    let (ws, _) = tokio::time::timeout(
        tv_auth::CALL_TIMEOUT,
        tokio_tungstenite::connect_async(url.as_str()),
    )
    .await
    .map_err(|_| connect_err("timed out".into()))?
    .map_err(|e| connect_err(e.to_string()))?;
    let (mut sink, stream) = ws.split();
    let chunks = stream
        .take_while(|m| {
            if let Err(e) = m {
                log::warn!("ws receive: {e}");
            }
            futures::future::ready(m.is_ok())
        })
        .filter_map(|m| {
            futures::future::ready(match m {
                Ok(Message::Binary(b)) => Some(b.to_vec()),
                _ => None,
            })
        })
        .boxed_local();
    let (tx, mut rx) = futures::channel::mpsc::unbounded::<Vec<u8>>();
    let writer = pharos_client::byte_stream::ChunkWriter::new(move |bytes: &[u8]| {
        tx.unbounded_send(bytes.to_vec())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::BrokenPipe, e.to_string()))
    });
    let reader = pharos_client::byte_stream::ChunkReader::new(chunks);
    let (client, system) = pharos_client::connect_stream(reader, writer);
    let pump = async move {
        while let Some(bytes) = rx.next().await {
            if let Err(e) = sink.send(Message::Binary(bytes.into())).await {
                log::warn!("ws send: {e}");
                break;
            }
        }
    };
    let closed: Closed = Rc::new(Cell::new(false));
    let flag = closed.clone();
    let driven: Pin<Box<dyn Future<Output = ()>>> = Box::pin(async move {
        tokio::select! {
            r = system => if let Err(e) = r { log::warn!("rpc ended: {e}") },
            () = pump => {}
        }
        flag.set(true);
    });
    tokio::task::spawn_local(driven);
    Ok((client, closed))
}

/// How often a waiting call looks at the closed flag.
const CLOSED_CHECK: Duration = Duration::from_millis(200);

/// Run one call with a deadline. A call on a closed socket hangs instead of
/// failing, so this returns at once when `closed` is set.
pub async fn bounded<T>(
    op: &str,
    closed: &Closed,
    call: impl Future<Output = Result<T, ClientError>>,
) -> Result<T, ContentError> {
    if closed.get() {
        return Err(ContentError::Closed { op: op.into() });
    }
    let watch = async {
        while !closed.get() {
            tokio::time::sleep(CLOSED_CHECK).await;
        }
    };
    tokio::select! {
        r = call => r.map_err(|e| ContentError::from_client(op, e)),
        () = watch => Err(ContentError::Closed { op: op.into() }),
        () = tokio::time::sleep(tv_auth::CALL_TIMEOUT) => Err(ContentError::Timeout {
            op: op.into(),
            secs: tv_auth::CALL_TIMEOUT.as_secs(),
        }),
    }
}

fn text(t: capnp::Result<capnp::text::Reader<'_>>) -> Result<String, ClientError> {
    Ok(t?
        .to_string()
        .map_err(|e| capnp::Error::failed(e.to_string()))?)
}

/// `Bootstrap.resume` with the stored device token.
pub async fn resume(
    bootstrap: &bootstrap::Client,
    closed: &Closed,
    token: &str,
) -> Result<session::Client, ContentError> {
    let mut req = bootstrap.resume_request();
    {
        let mut params = req.get();
        call_info().write(params.reborrow().init_ctx());
        params.set_device_token(token);
    }
    let call = async {
        let reply = req.send().promise.await?;
        let result = reply.get()?.get_result()?;
        match result.get_status()?.which().map_err(capnp::Error::from)? {
            pharos_api::pharos_capnp::done::Ok(()) => Ok(result.get_session()?),
            pharos_api::pharos_capnp::done::Error(e) => Err(ClientError::from_api(e?)),
        }
    };
    bounded("resume the stored device token", closed, call).await
}

/// The code to show and the secret to poll with.
pub async fn quick_connect_initiate(
    bootstrap: &bootstrap::Client,
    closed: &Closed,
) -> Result<(String, String), ContentError> {
    let mut req = bootstrap.quick_connect_initiate_request();
    call_info().write(req.get().init_ctx());
    let call = async {
        let reply = req.send().promise.await?;
        match reply
            .get()?
            .get_result()?
            .which()
            .map_err(capnp::Error::from)?
        {
            quick_connect_result::Pending(p) => {
                let p = p?;
                Ok((text(p.get_code())?, text(p.get_secret())?))
            }
            quick_connect_result::Error(e) => Err(ClientError::from_api(e?)),
        }
    };
    bounded("quickconnect initiate", closed, call).await
}

/// One QuickConnect poll, as a `Polled` for `tv_auth` to judge.
pub async fn quick_connect_poll(
    bootstrap: &bootstrap::Client,
    closed: &Closed,
    secret: &str,
) -> Polled<(session::Client, String)> {
    let mut req = bootstrap.quick_connect_poll_request();
    {
        let mut params = req.get();
        call_info().write(params.reborrow().init_ctx());
        params.set_secret(secret);
    }
    let call = async {
        let reply = req.send().promise.await?;
        match reply
            .get()?
            .get_result()?
            .which()
            .map_err(capnp::Error::from)?
        {
            login_result::Ok(ok) => {
                let ok = ok?;
                Ok(Polled::Approved((
                    ok.get_session()?,
                    text(ok.get_device_token())?,
                )))
            }
            login_result::Error(e) => match ClientError::from_api(e?) {
                ClientError::Api { kind, reason, .. } => Ok(Polled::from_api_error(kind, &reason)),
                other => Err(other),
            },
        }
    };
    match bounded("quickconnect poll", closed, call).await {
        Ok(polled) => polled,
        Err(ContentError::Closed { .. }) => Polled::Closed,
        Err(ContentError::Timeout { .. }) => Polled::TimedOut,
        Err(e) => Polled::Failed(e.to_string()),
    }
}

/// A signed-in connection and the calls the browse tree needs.
#[derive(Clone)]
pub struct NativeCatalogue {
    api: NativeApi,
    closed: Closed,
    // Keeps the connection's bootstrap capability alive with the session.
    _bootstrap: bootstrap::Client,
    info: CallInfo,
}

impl NativeCatalogue {
    pub fn new(bootstrap: bootstrap::Client, closed: Closed, session: &session::Client) -> Self {
        Self {
            api: NativeApi::new(session),
            closed,
            _bootstrap: bootstrap,
            info: call_info(),
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed.get()
    }

    pub async fn media_token(&self) -> Result<MediaToken, ContentError> {
        bounded(
            "media token",
            &self.closed,
            self.api.media_token(&self.info),
        )
        .await
    }
}

impl Catalogue for NativeCatalogue {
    async fn views(&self) -> Result<Vec<Item>, ContentError> {
        let page = bounded("list libraries", &self.closed, self.api.views(&self.info)).await?;
        Ok(page.items)
    }

    async fn recent_music(&self) -> Result<Vec<Item>, ContentError> {
        let home = bounded("recently played", &self.closed, self.api.home(&self.info)).await?;
        Ok(home.listening)
    }

    async fn list(
        &self,
        parent: ItemRef,
        group: Option<ItemKind>,
        sort_by: &str,
        limit: u32,
    ) -> Result<Vec<Item>, ContentError> {
        let op = format!("list {parent} ({group:?}, {sort_by})");
        let listing = Listing::of(parent, group, sort_by);
        let page = bounded(
            &op,
            &self.closed,
            self.api.query(&self.info, &listing, 0, limit),
        )
        .await?;
        Ok(page.items)
    }

    async fn search(&self, text: &str, limit: u32) -> Result<Vec<Item>, ContentError> {
        let op = format!("search {text:?}");
        let page = bounded(&op, &self.closed, self.api.search(&self.info, text, limit)).await?;
        Ok(page.items)
    }

    async fn mix(&self, spec: &MixSpec, count: u32) -> Result<Vec<Item>, ContentError> {
        let batch = bounded(
            "mix",
            &self.closed,
            self.api.mix(&self.info, spec, &[], count),
        )
        .await?;
        Ok(batch.picks.into_iter().map(|p| p.item).collect())
    }

    async fn item(&self, id: ItemRef) -> Result<Item, ContentError> {
        let op = format!("detail {id}");
        Ok(bounded(&op, &self.closed, self.api.detail(&self.info, id))
            .await?
            .item)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn https_is_taken_and_cleartext_remote_is_refused_naming_the_value() {
        assert_eq!(
            rpc_url("https://h:8443/").unwrap(),
            "wss://h:8443/pharos/rpc"
        );
        let e = rpc_url("http://pharos.example").expect_err("cleartext");
        assert!(e.to_string().contains("http://pharos.example"), "{e}");
    }

    #[test]
    fn only_loopback_may_be_cleartext() {
        assert_eq!(
            rpc_url("http://127.0.0.1:9/").unwrap(),
            "ws://127.0.0.1:9/pharos/rpc"
        );
        assert_eq!(
            rpc_url("http://localhost:9").unwrap(),
            "ws://localhost:9/pharos/rpc"
        );
        assert!(rpc_url("http://127.0.0.1.evil.example:9").is_err());
    }
}
