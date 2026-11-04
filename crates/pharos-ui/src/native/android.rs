//! 025 slice 0 spike: the Android TV app's link to the server. A native
//! WebSocket carries capnp RPC (`pharos_client::connect_stream`), sign-in
//! goes through QuickConnect, and the device token lives in an app-private
//! file. Throwaway: slice 1 folds this into a full platform module.

use futures_util::{SinkExt, StreamExt};
use pharos_api::pharos_capnp::{bootstrap, login_result, quick_connect_result, session};
use pharos_client::{CallInfo, ClientError};
use std::path::PathBuf;

/// The connection's driver future.
pub type RpcSystem = std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>;

/// Open `wss://<host>/pharos/rpc`. The returned future drives the
/// connection and must be spawned on a `LocalSet`.
/// The flag turns true once the connection has ended.
pub async fn connect(
    base: &str,
) -> Result<
    (
        bootstrap::Client,
        RpcSystem,
        std::rc::Rc<std::cell::Cell<bool>>,
    ),
    String,
> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let url = format!(
        "{}/pharos/rpc",
        base.replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1)
    );
    let (ws, _) = tokio_tungstenite::connect_async(url.as_str())
        .await
        .map_err(|e| format!("connect {url}: {e}"))?;
    let (mut sink, stream) = ws.split();
    let chunks = stream
        .take_while(|m| futures_util::future::ready(m.is_ok()))
        .filter_map(|m| {
            futures_util::future::ready(match m {
                Ok(tokio_tungstenite::tungstenite::Message::Binary(b)) => Some(b.to_vec()),
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
            if let Err(e) = sink
                .send(tokio_tungstenite::tungstenite::Message::Binary(
                    bytes.into(),
                ))
                .await
            {
                log::warn!("ws send: {e}");
                break;
            }
        }
    };
    let closed = std::rc::Rc::new(std::cell::Cell::new(false));
    let flag = closed.clone();
    let driven: RpcSystem = Box::pin(async move {
        tokio::select! {
            r = system => if let Err(e) = r { log::warn!("rpc ended: {e}") },
            () = pump => {}
        }
        flag.set(true);
    });
    Ok((client, driven, closed))
}

/// This app's private files directory, from its package name.
fn files_dir() -> PathBuf {
    let cmdline = std::fs::read("/proc/self/cmdline").unwrap_or_default();
    let package =
        String::from_utf8_lossy(cmdline.split(|b| *b == 0).next().unwrap_or_default()).into_owned();
    PathBuf::from(format!("/data/data/{package}/files"))
}

fn token_path() -> PathBuf {
    files_dir().join("device-token")
}

pub fn load_token() -> Option<String> {
    std::fs::read_to_string(token_path())
        .ok()
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
}

pub fn store_token(token: &str) -> Result<(), String> {
    let path = token_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(&path, token).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn forget_token() {
    let _ = std::fs::remove_file(token_path());
}

/// `Bootstrap.resume` with a stored device token.
pub async fn resume(
    bootstrap: &bootstrap::Client,
    info: &CallInfo,
    token: &str,
) -> Result<session::Client, ClientError> {
    let mut req = bootstrap.resume_request();
    {
        let mut params = req.get();
        info.write(params.reborrow().init_ctx());
        params.set_device_token(token);
    }
    let reply = req.send().promise.await?;
    let result = reply.get()?.get_result()?;
    match result.get_status()?.which().map_err(capnp::Error::from)? {
        pharos_api::pharos_capnp::done::Ok(()) => Ok(result.get_session()?),
        pharos_api::pharos_capnp::done::Error(e) => Err(ClientError::from_api(e?)),
    }
}

/// The code to show and the secret to poll with.
pub async fn quick_connect_initiate(
    bootstrap: &bootstrap::Client,
    info: &CallInfo,
) -> Result<(String, String), ClientError> {
    let mut req = bootstrap.quick_connect_initiate_request();
    info.write(req.get().init_ctx());
    let reply = req.send().promise.await?;
    match reply
        .get()?
        .get_result()?
        .which()
        .map_err(capnp::Error::from)?
    {
        quick_connect_result::Pending(p) => {
            let p = p?;
            let text = |t: capnp::Result<capnp::text::Reader<'_>>| -> Result<String, ClientError> {
                Ok(t?
                    .to_string()
                    .map_err(|e| capnp::Error::failed(e.to_string()))?)
            };
            Ok((text(p.get_code())?, text(p.get_secret())?))
        }
        quick_connect_result::Error(e) => Err(ClientError::from_api(e?)),
    }
}

/// `Ok(None)` while the code is not yet approved.
pub async fn quick_connect_poll(
    bootstrap: &bootstrap::Client,
    info: &CallInfo,
    secret: &str,
) -> Result<Option<(session::Client, String)>, ClientError> {
    let mut req = bootstrap.quick_connect_poll_request();
    {
        let mut params = req.get();
        info.write(params.reborrow().init_ctx());
        params.set_secret(secret);
    }
    let reply = req.send().promise.await?;
    match reply
        .get()?
        .get_result()?
        .which()
        .map_err(capnp::Error::from)?
    {
        login_result::Ok(ok) => {
            let ok = ok?;
            let token = ok
                .get_device_token()?
                .to_string()
                .map_err(|e| capnp::Error::failed(e.to_string()))?;
            Ok(Some((ok.get_session()?, token)))
        }
        login_result::Error(e) => {
            let err = ClientError::from_api(e?);
            match err {
                ClientError::Api {
                    kind: "notPermitted",
                    ..
                } => Ok(None),
                other => Err(other),
            }
        }
    }
}
