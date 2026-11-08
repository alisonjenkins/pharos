//! The Android TV app's I/O (spec 025): a native WebSocket carrying capnp RPC
//! (`pharos_client::connect_stream`), the QuickConnect sign-in loop, and the
//! device token in an app-private file. Every decision (backoff, what a poll
//! answer means) lives in `tv_auth`, which host tests cover; this module only
//! does the sockets and files. Nothing here logs a token or a QuickConnect
//! secret; the QuickConnect code is logged because the user must type it.

use crate::native::tv_auth::{self, Action, Polled, QuickConnect};
use futures_util::{SinkExt, StreamExt};
use pharos_api::pharos_capnp::{bootstrap, login_result, quick_connect_result, session};
use pharos_client::{CallInfo, ClientError};
use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

/// The connection's driver future.
pub type RpcSystem = std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>;

/// Turns true once the connection has ended; no call on it will be answered.
pub type Closed = Rc<Cell<bool>>;

/// Why a bounded call gave no answer.
#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("connection closed")]
    Closed,
    #[error("no answer in time")]
    TimedOut,
    #[error(transparent)]
    Client(#[from] ClientError),
}

/// How often a waiting call looks at the closed flag.
const CLOSED_CHECK: Duration = Duration::from_millis(200);

async fn wait_closed(closed: &Closed) {
    while !closed.get() {
        tokio::time::sleep(CLOSED_CHECK).await;
    }
}

/// Run one call with a deadline. A call on a closed socket hangs instead of
/// failing, so this returns at once when `closed` is set (before or during
/// the call) and gives up after `limit`.
async fn bounded<T>(
    closed: &Closed,
    limit: Duration,
    call: impl std::future::Future<Output = Result<T, ClientError>>,
) -> Result<T, NetError> {
    if closed.get() {
        return Err(NetError::Closed);
    }
    tokio::select! {
        r = call => r.map_err(NetError::Client),
        () = wait_closed(closed) => Err(NetError::Closed),
        () = tokio::time::sleep(limit) => Err(NetError::TimedOut),
    }
}

/// Open `wss://<host>/pharos/rpc`. The returned future drives the
/// connection and must be spawned on a `LocalSet`; the flag turns true once
/// the connection has ended.
pub async fn connect(base: &str) -> Result<(bootstrap::Client, RpcSystem, Closed), String> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let url = tv_auth::rpc_url(base)?;
    let (ws, _) = tokio::time::timeout(
        tv_auth::CALL_TIMEOUT,
        tokio_tungstenite::connect_async(url.as_str()),
    )
    .await
    .map_err(|_| format!("connect {url}: timed out"))?
    .map_err(|e| format!("connect {url}: {e}"))?;
    let (mut sink, stream) = ws.split();
    let chunks = stream
        .take_while(|m| {
            // Log the error that ends the stream, as `pump` logs a failed send.
            if let Err(e) = m {
                log::warn!("ws receive: {e}");
            }
            futures_util::future::ready(m.is_ok())
        })
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
    let closed: Closed = Rc::new(Cell::new(false));
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
fn files_dir() -> Result<PathBuf, String> {
    let cmdline = std::fs::read("/proc/self/cmdline").map_err(|e| format!("cmdline: {e}"))?;
    let raw =
        String::from_utf8_lossy(cmdline.split(|b| *b == 0).next().unwrap_or_default()).into_owned();
    let package = tv_auth::package_from_cmdline(&raw)?;
    Ok(PathBuf::from(format!("/data/data/{package}/files")))
}

fn token_path() -> Result<PathBuf, String> {
    static WARNED: std::sync::Once = std::sync::Once::new();
    files_dir()
        .map(|d| d.join("device-token"))
        .inspect_err(|e| {
            WARNED.call_once(|| log::warn!("token storage unavailable: {e}"));
        })
}

/// The device token as last loaded, stored or forgotten, kept in memory for
/// the server proxy, which sends it on every image request.
static TOKEN: std::sync::Mutex<Option<Option<String>>> = std::sync::Mutex::new(None);

fn set_cached(token: Option<String>) {
    if let Ok(mut t) = TOKEN.lock() {
        *t = Some(token);
    }
}

/// `load_token`, read from disk only the first time. Never log the value.
pub fn token() -> Option<String> {
    if let Ok(t) = TOKEN.lock() {
        if let Some(token) = &*t {
            return token.clone();
        }
    }
    load_token()
}

/// The stored device token, if any. Never log the value.
pub fn load_token() -> Option<String> {
    let path = token_path().ok()?;
    let token = std::fs::read_to_string(path)
        .ok()
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty());
    set_cached(token.clone());
    token
}

/// Write the token to a temp file in the same directory, fsync it, and
/// rename it over the target, so a crash never leaves a half-written token.
pub fn store_token(token: &str) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let path = token_path()?;
    let dir = path.parent().ok_or("token path has no directory")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = dir.join("device-token.tmp");
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(token.as_bytes())?;
        f.sync_all()?;
        if f.metadata()?.permissions().mode() & 0o077 != 0 {
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &path)
    };
    write()
        .inspect(|()| set_cached(Some(token.to_owned())))
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("{}: {e}", path.display())
        })
}

pub fn forget_token() {
    set_cached(None);
    let Ok(path) = token_path() else { return };
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => log::warn!("forget token: {e}"),
    }
}

/// Connect and spawn the driver, retrying with backoff until it works.
/// `base` must already have passed `tv_auth::rpc_url`: a bad base would
/// fail every attempt, so it is refused before this loop, not retried.
async fn connect_until_up(base: &str, qc: &mut QuickConnect) -> (bootstrap::Client, Closed) {
    loop {
        match connect(base).await {
            Ok((client, system, closed)) => {
                dioxus::core::spawn_forever(system);
                return (client, closed);
            }
            Err(e) => {
                let wait = qc.retry_delay();
                log::warn!("{e}; retrying in {wait:?}");
                tokio::time::sleep(wait).await;
            }
        }
    }
}

/// `Bootstrap.resume` with a stored device token.
pub async fn resume(
    bootstrap: &bootstrap::Client,
    closed: &Closed,
    info: &CallInfo,
    token: &str,
) -> Result<session::Client, NetError> {
    let mut req = bootstrap.resume_request();
    {
        let mut params = req.get();
        info.write(params.reborrow().init_ctx());
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
    bounded(closed, tv_auth::CALL_TIMEOUT, call).await
}

/// `Bootstrap.login` with a name and password; the device token to
/// `store_token`. Never log the password or the token.
pub async fn login(
    bootstrap: &bootstrap::Client,
    closed: &Closed,
    info: &CallInfo,
    name: &str,
    password: &str,
) -> Result<String, NetError> {
    let mut req = bootstrap.login_request();
    {
        let mut params = req.get();
        info.write(params.reborrow().init_ctx());
        params.set_name(name);
        params.set_password(password);
    }
    let call = async {
        let reply = req.send().promise.await?;
        match reply
            .get()?
            .get_result()?
            .which()
            .map_err(capnp::Error::from)?
        {
            login_result::Ok(ok) => text(ok?.get_device_token()),
            login_result::Error(e) => Err(ClientError::from_api(e?)),
        }
    };
    bounded(closed, tv_auth::CALL_TIMEOUT, call).await
}

/// The server address stored in the app's private `server` file, if any;
/// `tv_page::server_base` checks it.
pub fn load_server() -> Option<String> {
    let path = files_dir().ok()?.join("server");
    std::fs::read_to_string(path).ok()
}

/// A preference's file: `pref-` and the key, kept to safe file name bytes.
fn pref_path(key: &str) -> Result<PathBuf, String> {
    let name: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    Ok(files_dir()?.join(format!("pref-{name}")))
}

const GROUP_DEVICE_KEY: &str = "pharos.group.device";

/// This TV's device id for group calls, the twin of the web's per-tab id: the
/// server keys a member by user and device. Kept as a preference so a restart
/// is the same member; if it cannot be read or kept, this run's id still works.
/// A failed read never overwrites the stored id: that would make the TV a new
/// member after a transient error.
pub fn group_device() -> String {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| {
        let keep = match load_pref(GROUP_DEVICE_KEY) {
            Ok(Some(id)) if tv_auth::is_group_device(&id) => return id,
            Ok(_) => true,
            Err(e) => {
                log::warn!("group device id: {e}; using one for this run only");
                false
            }
        };
        let id = tv_auth::new_group_device(random_u64());
        if keep {
            if let Err(e) = store_pref(GROUP_DEVICE_KEY, &id) {
                log::warn!("group device id: {e}");
            }
        }
        id
    })
    .clone()
}

/// 64 bits that differ between installs: std's per-process random hash keys
/// mixed with the time. Not for secrets; only to tell two TVs apart.
fn random_u64() -> u64 {
    use std::hash::{BuildHasher as _, Hasher as _};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    if let Ok(d) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        h.write_u128(d.as_nanos());
    }
    h.finish()
}

/// `localStorage.getItem`'s twin.
pub fn load_pref(key: &str) -> Result<Option<String>, String> {
    match std::fs::read_to_string(pref_path(key)?) {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("pref {key}: {e}")),
    }
}

/// `localStorage.setItem`'s twin: written whole, then renamed into place.
pub fn store_pref(key: &str, value: &str) -> Result<(), String> {
    let path = pref_path(key)?;
    let dir = path.parent().ok_or("pref path has no directory")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, value)
        .and_then(|()| std::fs::rename(&tmp, &path))
        .map_err(|e| format!("pref {key}: {e}"))
}

fn text(t: capnp::Result<capnp::text::Reader<'_>>) -> Result<String, ClientError> {
    Ok(t?
        .to_string()
        .map_err(|e| capnp::Error::failed(e.to_string()))?)
}

/// The code to show and the secret to poll with.
pub async fn quick_connect_initiate(
    bootstrap: &bootstrap::Client,
    closed: &Closed,
    info: &CallInfo,
) -> Result<(String, String), NetError> {
    let mut req = bootstrap.quick_connect_initiate_request();
    info.write(req.get().init_ctx());
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
    bounded(closed, tv_auth::CALL_TIMEOUT, call).await
}

/// One QuickConnect poll, as a `Polled` for `tv_auth` to judge.
pub async fn quick_connect_poll(
    bootstrap: &bootstrap::Client,
    closed: &Closed,
    info: &CallInfo,
    secret: &str,
) -> Polled<(session::Client, String)> {
    let mut req = bootstrap.quick_connect_poll_request();
    {
        let mut params = req.get();
        info.write(params.reborrow().init_ctx());
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
    match bounded(closed, tv_auth::POLL_TIMEOUT, call).await {
        Ok(polled) => polled,
        Err(NetError::Closed) => Polled::Closed,
        Err(NetError::TimedOut) => Polled::TimedOut,
        Err(NetError::Client(e)) if e.is_disconnected() => Polled::Closed,
        Err(NetError::Client(e)) => Polled::Failed(e.to_string()),
    }
}

/// A signed-in connection from QuickConnect.
pub struct SignedIn {
    pub client: bootstrap::Client,
    pub closed: Closed,
    pub session: session::Client,
    /// The device token to `store_token`.
    pub token: String,
}

/// Sign in by QuickConnect: mint a code, hand it to `show_code`, and poll
/// until someone approves it. Runs until approved (drop the future to
/// cancel); a server address that is not `https://` is an error at once. The server closes a socket not signed in within 30 s, so this
/// reconnects, with backoff, only once the socket has closed; a code that
/// expires (`notFound`) is replaced by a new one. Must run on the Dioxus
/// runtime, which drives the connection.
pub async fn quick_connect_sign_in(
    base: &str,
    info: &CallInfo,
    mut show_code: impl FnMut(&str),
) -> Result<SignedIn, String> {
    // A bad server address is permanent: refuse it now instead of retrying
    // the socket forever.
    tv_auth::rpc_url(base)?;
    let mut qc = QuickConnect::new();
    let mut link: Option<(bootstrap::Client, Closed)> = None;
    loop {
        if link.as_ref().is_some_and(|(_, closed)| closed.get()) {
            link = None;
        }
        // The secret survives a reconnect: the server accepts it from a new
        // connection. Only `notFound` (see `tv_auth`) drops it.
        let (client, closed) = match &link {
            Some(l) => l.clone(),
            None => {
                let l = connect_until_up(base, &mut qc).await;
                link = Some(l.clone());
                l
            }
        };
        if qc.secret().is_none() {
            match quick_connect_initiate(&client, &closed, info).await {
                Ok((code, new_secret)) => {
                    log::info!("quickconnect code: {code}");
                    show_code(&code);
                    qc.on_minted(new_secret);
                }
                Err(e) => {
                    let wait = qc.retry_delay();
                    log::warn!("quickconnect initiate: {e}; retrying in {wait:?}");
                    tokio::time::sleep(wait).await;
                    continue;
                }
            }
        }
        let Some(current) = qc.secret().map(str::to_owned) else {
            continue;
        };
        let polled = quick_connect_poll(&client, &closed, info, &current).await;
        match qc.on_poll(&polled) {
            Action::Finish => {
                if let Polled::Approved((session, token)) = polled {
                    return Ok(SignedIn {
                        client,
                        closed,
                        session,
                        token,
                    });
                }
                // `on_poll` finishes only on `Approved`; if that ever changes,
                // say so and keep polling rather than spin.
                log::warn!("quickconnect: finished without an approval; polling again");
                tokio::time::sleep(tv_auth::POLL_INTERVAL).await;
            }
            Action::PollAfter(wait) => {
                if let Polled::Failed(e) = &polled {
                    log::warn!("quickconnect poll: {e}");
                }
                tokio::time::sleep(wait).await;
            }
            Action::RemintAfter(wait) => {
                log::info!("quickconnect code expired; new code in {wait:?}");
                tokio::time::sleep(wait).await;
            }
            Action::ReconnectAfter(wait) => {
                log::warn!("quickconnect socket closed; reconnecting in {wait:?}");
                link = None;
                tokio::time::sleep(wait).await;
            }
        }
    }
}
