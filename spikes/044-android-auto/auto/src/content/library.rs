//! The blocking facade the JNI layer calls, and the worker thread behind it.
//!
//! JNI callbacks are synchronous and capnp-rpc is not `Send`, so one thread
//! per [`Library`] runs a current-thread runtime and owns the connection;
//! callers send it a command and wait with a deadline. A call that gets no
//! answer returns an error naming the call, never hangs the binder thread.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pharos_api::pharos_capnp::{bootstrap, session};
use pharos_client::api::MediaToken;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

use super::error::ContentError;
use super::id::NodeId;
use super::net::{self, Closed, NativeCatalogue};
use super::node::{Node, Urls};
use super::token;
use super::tree;
use super::tv_auth::{self, Action, Polled, QuickConnect};

/// Where sign-in stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthState {
    /// No usable device token and no code yet.
    SignedOut,
    /// A QuickConnect code is waiting for approval in the Pharos web UI.
    Pending {
        code: String,
    },
    SignedIn,
}

#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    /// Longest one browse, node or search call may take in all.
    pub command: Duration,
    /// Longest a browse waits for a QuickConnect code to exist.
    pub code: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            command: tv_auth::CALL_TIMEOUT,
            code: tv_auth::CALL_TIMEOUT,
        }
    }
}

/// Ask for a new media token when the current one has less than this left.
const TOKEN_REFRESH_MARGIN_MS: u64 = 5 * 60 * 1000;

/// Extra time the caller waits past the worker's own deadline, so the
/// worker's error (which names the call) wins over a bare channel timeout.
const REPLY_SLACK: Duration = Duration::from_secs(2);

type Hook = Arc<dyn Fn(&AuthState) + Send + Sync>;

struct Inner {
    auth: AuthState,
    /// The last connect or poll failure, named in "no code" errors.
    last_error: String,
    hook: Option<Hook>,
}

struct Shared {
    inner: Mutex<Inner>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set_auth(&self, auth: AuthState) {
        let hook = {
            let mut g = self.lock();
            if g.auth == auth {
                return;
            }
            g.auth = auth.clone();
            g.hook.clone()
        };
        self.changed.notify_all();
        log::info!("auth state: {}", describe(&auth));
        if let Some(h) = hook {
            h(&auth);
        }
    }

    fn set_error(&self, why: String) {
        self.lock().last_error = why;
    }
}

/// The state without the code's value in the generic log line is not needed:
/// the code is shown to the user on purpose.
fn describe(a: &AuthState) -> String {
    match a {
        AuthState::SignedOut => "signed out".into(),
        AuthState::Pending { code } => format!("waiting for approval of code {code}"),
        AuthState::SignedIn => "signed in".into(),
    }
}

enum Op {
    Children(NodeId),
    Node(NodeId),
    Search(String),
    SignOut,
}

impl Op {
    fn name(&self) -> String {
        match self {
            Op::Children(id) => format!("browse {id}"),
            Op::Node(id) => format!("node {id}"),
            Op::Search(q) => format!("search {q:?}"),
            Op::SignOut => "sign out".into(),
        }
    }
}

enum Out {
    Nodes(Vec<Node>),
    One(Node),
    Done,
}

struct Cmd {
    op: Op,
    reply: mpsc::Sender<Result<Out, ContentError>>,
}

/// A signed-in library of one Pharos server. Cheap calls, safe from any
/// thread; dropping it stops the worker.
pub struct Library {
    tx: UnboundedSender<Cmd>,
    shared: Arc<Shared>,
    timeouts: Timeouts,
    server: String,
}

impl Library {
    /// Start the worker for `server_url` (`https://host[:port]`) with the
    /// device token kept in `token_path`. No network I/O happens until the
    /// first call; a missing or revoked token starts QuickConnect.
    pub fn connect(server_url: &str, token_path: &Path) -> Result<Self, ContentError> {
        Self::connect_with(server_url, token_path, Timeouts::default())
    }

    pub fn connect_with(
        server_url: &str,
        token_path: &Path,
        timeouts: Timeouts,
    ) -> Result<Self, ContentError> {
        net::rpc_url(server_url)?;
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                auth: AuthState::SignedOut,
                last_error: "not tried yet".into(),
                hook: None,
            }),
            changed: Condvar::new(),
        });
        if token::load(token_path)?.is_some() {
            log::info!("device token found at {}", token_path.display());
        }
        let (tx, rx) = unbounded_channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let base = server_url.trim_end_matches('/').to_owned();
        let token_path = token_path.to_owned();
        let for_worker = shared.clone();
        let limit = timeouts.command;
        // The worker holds capnp capabilities, which are not `Send`, so it is
        // built on its own thread from parts that are.
        std::thread::Builder::new()
            .name("pharos-content".into())
            .spawn(move || {
                let worker = Worker {
                    base,
                    token_path,
                    shared: for_worker,
                    link: None,
                    qc_running: false,
                    limit,
                };
                worker.run_thread(rx, ready_tx)
            })
            .map_err(|e| ContentError::Rpc {
                op: "start the content worker".into(),
                why: e.to_string(),
            })?;
        ready_rx.recv().map_err(|_| ContentError::WorkerGone)??;
        Ok(Self {
            tx,
            shared,
            timeouts,
            server: server_url.to_owned(),
        })
    }

    pub fn auth_state(&self) -> AuthState {
        self.shared.lock().auth.clone()
    }

    /// Call `f` (from the worker thread) whenever sign-in changes, so the
    /// JNI layer can `notifyChildrenChanged` once a code is approved.
    pub fn on_auth_change(&self, f: impl Fn(&AuthState) + Send + Sync + 'static) {
        self.shared.lock().hook = Some(Arc::new(f));
    }

    /// What `media_id` holds. Signed out, every node holds the sign-in row
    /// carrying the QuickConnect code.
    pub fn children(&self, media_id: &str) -> Result<Vec<Node>, ContentError> {
        let id = NodeId::parse(media_id)?;
        self.nodes_or_sign_in(Op::Children(id))
    }

    /// One node, with a fresh stream URL for a track.
    pub fn node(&self, media_id: &str) -> Result<Node, ContentError> {
        let id = NodeId::parse(media_id)?;
        match self.call(Op::Node(id))? {
            Out::One(n) => Ok(n),
            _ => Err(ContentError::WorkerGone),
        }
    }

    /// Songs, albums and artists matching a spoken request.
    pub fn search(&self, query: &str) -> Result<Vec<Node>, ContentError> {
        self.nodes_or_sign_in(Op::Search(query.to_owned()))
    }

    /// Forget the device token and drop the connection.
    pub fn sign_out(&self) -> Result<(), ContentError> {
        self.call(Op::SignOut).map(|_| ())
    }

    fn nodes_or_sign_in(&self, op: Op) -> Result<Vec<Node>, ContentError> {
        let (tx, rx) = mpsc::channel();
        let name = op.name();
        self.tx
            .send(Cmd { op, reply: tx })
            .map_err(|_| ContentError::WorkerGone)?;
        match self.wait(&name, &rx) {
            Ok(Out::Nodes(n)) => Ok(n),
            Ok(_) => Err(ContentError::WorkerGone),
            Err(ContentError::NeedsSignIn) => self.sign_in_rows(),
            Err(other) => Err(other),
        }
    }

    fn call(&self, op: Op) -> Result<Out, ContentError> {
        let (tx, rx) = mpsc::channel();
        let name = op.name();
        self.tx
            .send(Cmd { op, reply: tx })
            .map_err(|_| ContentError::WorkerGone)?;
        self.wait(&name, &rx)
    }

    fn wait(
        &self,
        name: &str,
        rx: &mpsc::Receiver<Result<Out, ContentError>>,
    ) -> Result<Out, ContentError> {
        match rx.recv_timeout(self.timeouts.command + REPLY_SLACK) {
            Ok(r) => r,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(ContentError::Timeout {
                op: name.to_owned(),
                secs: (self.timeouts.command + REPLY_SLACK).as_secs(),
            }),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(ContentError::WorkerGone),
        }
    }

    /// The sign-in row, once a code exists.
    fn sign_in_rows(&self) -> Result<Vec<Node>, ContentError> {
        let g = self.shared.lock();
        let (g, _) = self
            .shared
            .changed
            .wait_timeout_while(g, self.timeouts.code, |i| i.auth == AuthState::SignedOut)
            .unwrap_or_else(PoisonError::into_inner);
        match &g.auth {
            AuthState::Pending { code } => Ok(vec![Node::sign_in(code)]),
            // Approved while we waited: the caller asks again.
            AuthState::SignedIn => Ok(Vec::new()),
            AuthState::SignedOut => Err(ContentError::NoCode {
                server: self.server.clone(),
                last: g.last_error.clone(),
            }),
        }
    }
}

/// A live connection and its media token.
#[derive(Clone)]
struct Link {
    cat: NativeCatalogue,
    media: MediaToken,
}

enum Event {
    Code(String),
    Failed(String),
    Approved {
        bootstrap: bootstrap::Client,
        closed: Closed,
        session: session::Client,
        token: String,
    },
}

struct Worker {
    base: String,
    token_path: PathBuf,
    shared: Arc<Shared>,
    link: Option<Link>,
    qc_running: bool,
    limit: Duration,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

impl Worker {
    fn run_thread(
        self,
        rx: tokio::sync::mpsc::UnboundedReceiver<Cmd>,
        ready: mpsc::Sender<Result<(), ContentError>>,
    ) {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = ready.send(Err(ContentError::Rpc {
                    op: "start the content runtime".into(),
                    why: e.to_string(),
                }));
                return;
            }
        };
        let _ = ready.send(Ok(()));
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, self.run(rx));
    }

    async fn run(mut self, mut rx: tokio::sync::mpsc::UnboundedReceiver<Cmd>) {
        let (events_tx, mut events) = unbounded_channel::<Event>();
        loop {
            tokio::select! {
                cmd = rx.recv() => {
                    let Some(Cmd { op, reply }) = cmd else { break };
                    let name = op.name();
                    let result = match tokio::time::timeout(self.limit, self.execute(&op, &events_tx)).await {
                        Ok(r) => r,
                        Err(_) => Err(ContentError::Timeout { op: name.clone(), secs: self.limit.as_secs() }),
                    };
                    if let Err(e) = &result {
                        log::warn!("{name}: {e}");
                    }
                    let _ = reply.send(result);
                }
                Some(ev) = events.recv() => self.on_event(ev).await,
            }
        }
    }

    async fn on_event(&mut self, ev: Event) {
        match ev {
            Event::Code(code) => self.shared.set_auth(AuthState::Pending { code }),
            Event::Failed(why) => self.shared.set_error(why),
            Event::Approved {
                bootstrap,
                closed,
                session,
                token,
            } => {
                self.qc_running = false;
                if let Err(e) = token::store(&self.token_path, &token) {
                    log::warn!("device token not kept, signing in again next start: {e}");
                }
                let cat = NativeCatalogue::new(bootstrap, closed, &session);
                match cat.media_token().await {
                    Ok(media) => {
                        self.link = Some(Link { cat, media });
                        self.shared.set_auth(AuthState::SignedIn);
                    }
                    Err(e) => {
                        log::warn!("signed in but no media token: {e}");
                        self.shared.set_error(e.to_string());
                    }
                }
            }
        }
    }

    /// Run `op`, reconnecting once if the socket had closed under it.
    async fn execute(
        &mut self,
        op: &Op,
        events: &UnboundedSender<Event>,
    ) -> Result<Out, ContentError> {
        if matches!(op, Op::SignOut) {
            self.link = None;
            token::forget(&self.token_path)?;
            self.shared.set_auth(AuthState::SignedOut);
            return Ok(Out::Done);
        }
        let mut last = None;
        for attempt in 0..2 {
            let link = self.link(events).await?;
            let urls = Urls::new(&self.base, &link.media.token);
            let r = match op {
                Op::Children(id) => tree::children(&link.cat, &urls, *id).await.map(Out::Nodes),
                Op::Node(id) => tree::node(&link.cat, &urls, *id).await.map(Out::One),
                Op::Search(q) => tree::search(&link.cat, &urls, q).await.map(Out::Nodes),
                Op::SignOut => Ok(Out::Done),
            };
            match r {
                Err(e) if e.is_revoked() => {
                    log::warn!("{}: device token revoked; starting QuickConnect", op.name());
                    self.link = None;
                    if let Err(e) = token::forget(&self.token_path) {
                        log::warn!("{e}");
                    }
                    self.shared.set_auth(AuthState::SignedOut);
                    self.start_sign_in(events);
                    return Err(ContentError::NeedsSignIn);
                }
                Err(e @ ContentError::Closed { .. }) => {
                    log::warn!("{} (attempt {}): {e}; reconnecting", op.name(), attempt + 1);
                    self.link = None;
                    last = Some(e);
                }
                other => return other,
            }
        }
        Err(last.unwrap_or(ContentError::WorkerGone))
    }

    /// A live link: the open one (media token refreshed if near expiry), or
    /// one resumed from the stored device token. No token starts QuickConnect.
    async fn link(&mut self, events: &UnboundedSender<Event>) -> Result<Link, ContentError> {
        if let Some(l) = self.link.as_mut() {
            if l.cat.is_closed() {
                log::info!("connection closed; reconnecting");
                self.link = None;
            } else {
                if l.media.expires_at_ms <= now_ms().saturating_add(TOKEN_REFRESH_MARGIN_MS) {
                    l.media = l.cat.media_token().await?;
                }
                return Ok(l.clone());
            }
        }
        let Some(device_token) = token::load(&self.token_path)? else {
            self.start_sign_in(events);
            return Err(ContentError::NeedsSignIn);
        };
        let (bootstrap, closed) = net::open(&self.base).await?;
        let session = match net::resume(&bootstrap, &closed, &device_token).await {
            Ok(s) => s,
            Err(e) if e.is_revoked() => {
                log::warn!("stored device token refused ({e}); starting QuickConnect");
                if let Err(e) = token::forget(&self.token_path) {
                    log::warn!("{e}");
                }
                self.shared.set_auth(AuthState::SignedOut);
                self.start_sign_in(events);
                return Err(ContentError::NeedsSignIn);
            }
            Err(e) => {
                self.shared.set_error(e.to_string());
                return Err(e);
            }
        };
        let cat = NativeCatalogue::new(bootstrap, closed, &session);
        let media = cat.media_token().await?;
        let link = Link { cat, media };
        self.link = Some(link.clone());
        self.shared.set_auth(AuthState::SignedIn);
        Ok(link)
    }

    fn start_sign_in(&mut self, events: &UnboundedSender<Event>) {
        if self.qc_running {
            return;
        }
        self.qc_running = true;
        tokio::task::spawn_local(quick_connect(self.base.clone(), events.clone()));
    }
}

/// Mint a code, report it, and poll until someone approves it. Reconnect and
/// re-mint decisions are `tv_auth`'s (see its module comment for why).
async fn quick_connect(base: String, events: UnboundedSender<Event>) {
    let mut qc = QuickConnect::new();
    let mut link: Option<(bootstrap::Client, Closed)> = None;
    loop {
        if link.as_ref().is_some_and(|(_, closed)| closed.get()) {
            link = None;
        }
        let (client, closed) = match &link {
            Some(l) => l.clone(),
            None => match net::open(&base).await {
                Ok(l) => {
                    link = Some(l.clone());
                    l
                }
                Err(e) => {
                    let wait = qc.retry_delay();
                    log::warn!("{e}; retrying in {wait:?}");
                    let _ = events.send(Event::Failed(e.to_string()));
                    tokio::time::sleep(wait).await;
                    continue;
                }
            },
        };
        if qc.secret().is_none() {
            match net::quick_connect_initiate(&client, &closed).await {
                Ok((code, secret)) => {
                    log::info!("quickconnect code: {code}");
                    let _ = events.send(Event::Code(code));
                    qc.on_minted(secret);
                }
                Err(e) => {
                    let wait = qc.retry_delay();
                    log::warn!("{e}; retrying in {wait:?}");
                    let _ = events.send(Event::Failed(e.to_string()));
                    tokio::time::sleep(wait).await;
                    continue;
                }
            }
        }
        let Some(current) = qc.secret().map(str::to_owned) else {
            continue;
        };
        let polled = net::quick_connect_poll(&client, &closed, &current).await;
        match qc.on_poll(&polled) {
            Action::Finish => {
                if let Polled::Approved((session, token)) = polled {
                    let _ = events.send(Event::Approved {
                        bootstrap: client,
                        closed,
                        session,
                        token,
                    });
                    return;
                }
                log::warn!("quickconnect: finished without an approval; polling again");
                tokio::time::sleep(tv_auth::POLL_INTERVAL).await;
            }
            Action::PollAfter(wait) => {
                if let Polled::Failed(e) = &polled {
                    log::warn!("quickconnect poll: {e}");
                    let _ = events.send(Event::Failed(e.clone()));
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn dead_server() -> String {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", l.local_addr().unwrap())
    }

    fn quick() -> Timeouts {
        Timeouts {
            command: Duration::from_secs(3),
            code: Duration::from_millis(800),
        }
    }

    /// With nothing listening, the browse says which URL it could not reach
    /// instead of hanging, and the car is not left without an answer.
    #[test]
    fn an_unreachable_server_is_named_in_the_error() {
        let base = dead_server();
        let path = std::env::temp_dir().join(format!("pharos-auto-lib-{}", std::process::id()));
        let lib = Library::connect_with(&base, &path, quick()).unwrap();
        let e = lib.children("root").expect_err("nothing is listening");
        let text = e.to_string();
        assert!(matches!(e, ContentError::NoCode { .. }), "{text}");
        assert!(text.contains(&base.replace("http", "ws")), "{text}");
        assert_eq!(lib.auth_state(), AuthState::SignedOut);
    }

    #[test]
    fn a_bad_media_id_and_a_bad_server_are_refused_before_any_call() {
        let path = std::env::temp_dir().join("pharos-auto-lib-bad");
        let e = Library::connect("http://pharos.example", &path)
            .err()
            .expect("cleartext");
        assert!(matches!(e, ContentError::Url(_)), "{e}");
        let lib = Library::connect_with(&dead_server(), &path, quick()).unwrap();
        let e = lib.children("track/x").expect_err("bad id");
        assert!(
            matches!(&e, ContentError::BadId(s) if s == "track/x"),
            "{e}"
        );
    }
}
