//! The blocking facade the JNI layer calls, and the worker thread behind it.
//!
//! JNI callbacks are synchronous and capnp-rpc is not `Send`, so one thread
//! per [`Library`] runs a current-thread runtime and owns the connection;
//! callers send it a command and wait with a deadline. A call that gets no
//! answer returns an error naming the call, never hangs the binder thread.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pharos_api::pharos_capnp::{bootstrap, session};
use pharos_client::api::MediaToken;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

use super::cache::Cache;
use super::error::ContentError;
use super::id::NodeId;
use super::net::{self, Closed, NativeCatalogue};
use super::node::{Node, Urls};
use super::prefetch::{Downloader, ResolveError, Resolver, Tuning};
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
    Ranked(Vec<Node>, tree::SearchReport),
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
    /// Set by [`Library::enable_cache`]; without it nothing is kept offline.
    offline: OnceLock<Offline>,
    queues: Mutex<Queues>,
    online: AtomicBool,
    /// What the platform's network callback last said.
    network_up: AtomicBool,
}

/// The audio cache and the thread that fills it.
struct Offline {
    cache: Arc<Cache>,
    downloader: Downloader,
}

/// Which album or mix each track was last listed in, in play order.
#[derive(Default)]
struct Queues {
    lists: HashMap<String, Vec<String>>,
    container_of: HashMap<String, String>,
}

impl Queues {
    fn note(&mut self, container: &str, nodes: impl IntoIterator<Item = (String, bool)>) {
        let tracks: Vec<String> = nodes
            .into_iter()
            .filter_map(|(id, playable)| playable.then_some(id))
            .collect();
        for t in &tracks {
            self.container_of.insert(t.clone(), container.to_owned());
        }
        self.lists.insert(container.to_owned(), tracks);
    }

    fn after(&self, track: &str) -> Vec<String> {
        let Some(list) = self.container_of.get(track).and_then(|c| self.lists.get(c)) else {
            return Vec::new();
        };
        match list.iter().position(|t| t == track) {
            Some(i) => list.iter().skip(i.saturating_add(1)).cloned().collect(),
            None => Vec::new(),
        }
    }
}

/// Containers whose children are the tracks to play on from.
fn is_track_list(id: NodeId) -> bool {
    matches!(id, NodeId::Album(_) | NodeId::Mix(_))
}

/// The server could not be reached or did not answer, as opposed to
/// answering "no".
fn is_unreachable(e: &ContentError) -> bool {
    matches!(
        e,
        ContentError::Timeout { .. }
            | ContentError::Closed { .. }
            | ContentError::Connect { .. }
            | ContentError::Rpc { .. }
            | ContentError::NoCode { .. }
    )
}

fn wait_reply(
    timeouts: &Timeouts,
    name: &str,
    rx: &mpsc::Receiver<Result<Out, ContentError>>,
) -> Result<Out, ContentError> {
    match rx.recv_timeout(timeouts.command + REPLY_SLACK) {
        Ok(r) => r,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(ContentError::Timeout {
            op: name.to_owned(),
            secs: (timeouts.command + REPLY_SLACK).as_secs(),
        }),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(ContentError::WorkerGone),
    }
}

/// A fresh stream URL for the downloader, which runs on its own thread.
fn resolve_stream(
    tx: &UnboundedSender<Cmd>,
    timeouts: &Timeouts,
    id: &str,
) -> Result<String, ResolveError> {
    let nid = NodeId::parse(id).map_err(|e| ResolveError::Permanent(e.to_string()))?;
    let op = Op::Node(nid);
    let name = op.name();
    let (rtx, rrx) = mpsc::channel();
    tx.send(Cmd { op, reply: rtx })
        .map_err(|_| ResolveError::Permanent(ContentError::WorkerGone.to_string()))?;
    match wait_reply(timeouts, &name, &rrx) {
        Ok(Out::One(n)) => n
            .stream_url
            .ok_or_else(|| ResolveError::Permanent(format!("{id} has no stream url"))),
        Ok(_) => Err(ResolveError::Permanent("unexpected reply".into())),
        Err(e) if is_unreachable(&e) => Err(ResolveError::Transient(e.to_string())),
        Err(e) => Err(ResolveError::Permanent(e.to_string())),
    }
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
            offline: OnceLock::new(),
            queues: Mutex::new(Queues::default()),
            online: AtomicBool::new(true),
            network_up: AtomicBool::new(true),
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
    ///
    /// With the cache enabled, a listing that cannot be fetched because the
    /// server is unreachable (or [`Library::set_online`] said so) is answered
    /// from the last one seen; an album or mix then shows only the tracks
    /// that are on the phone.
    pub fn children(&self, media_id: &str) -> Result<Vec<Node>, ContentError> {
        let id = NodeId::parse(media_id)?;
        if !self.online.load(Ordering::SeqCst) {
            if let Some(nodes) = self.offline_children(media_id, id) {
                return Ok(nodes);
            }
        }
        match self.nodes_or_sign_in(Op::Children(id)) {
            Ok(nodes) => {
                self.note_listing(media_id, id, &nodes);
                Ok(nodes)
            }
            Err(e) if is_unreachable(&e) => self.offline_children(media_id, id).ok_or(e),
            Err(e) => Err(e),
        }
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

    /// [`Library::search`] with how it ranked, for deciding whether to play
    /// the top result unasked. Signed out, the report is `None` and the
    /// nodes are the sign-in rows.
    pub fn search_ranked(
        &self,
        query: &str,
    ) -> Result<(Vec<Node>, Option<tree::SearchReport>), ContentError> {
        match self.call(Op::Search(query.to_owned())) {
            Ok(Out::Ranked(n, r)) => Ok((n, Some(r))),
            Ok(_) => Err(ContentError::WorkerGone),
            Err(ContentError::NeedsSignIn) => Ok((self.sign_in_rows()?, None)),
            Err(other) => Err(other),
        }
    }

    /// Forget the device token and drop the connection.
    pub fn sign_out(&self) -> Result<(), ContentError> {
        self.call(Op::SignOut).map(|_| ())
    }

    /// Keep the listing for offline browse and remember the play order.
    fn note_listing(&self, media_id: &str, id: NodeId, nodes: &[Node]) {
        let signed_out = nodes.iter().any(|n| n.id == NodeId::SignIn.to_string());
        if signed_out || nodes.is_empty() {
            return;
        }
        if is_track_list(id) {
            self.queues
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .note(media_id, nodes.iter().map(|n| (n.id.clone(), n.playable)));
        }
        if let Some(o) = self.offline.get() {
            o.cache.save_listing(media_id, nodes);
        }
    }

    fn offline_children(&self, media_id: &str, id: NodeId) -> Option<Vec<Node>> {
        let cache = &self.offline.get()?.cache;
        let rows = cache.load_listing(media_id)?;
        let total = rows.len();
        let nodes: Vec<Node> = rows
            .iter()
            .filter(|r| !(is_track_list(id) && r.playable && !cache.is_complete(&r.id)))
            .map(|r| r.to_node())
            .collect();
        log::info!(
            "browse offline id={media_id} snapshot_rows={total} shown={}",
            nodes.len()
        );
        Some(nodes)
    }

    /// Start keeping tracks in `dir` (at most `cap_bytes`; see
    /// [`super::cache::DEFAULT_CAP_BYTES`]) and downloading ahead. Call once
    /// after [`Library::connect`]; calling again is a no-op.
    pub fn enable_cache(&self, dir: &Path, cap_bytes: u64) -> Result<(), ContentError> {
        if self.offline.get().is_some() {
            return Ok(());
        }
        let cache = Arc::new(Cache::open(dir, cap_bytes)?);
        let (tx, timeouts) = (self.tx.clone(), self.timeouts);
        let resolve: Resolver = Arc::new(move |id| resolve_stream(&tx, &timeouts, id));
        let downloader = Downloader::start(cache.clone(), resolve, Tuning::default())?;
        {
            let mut q = self.queues.lock().unwrap_or_else(PoisonError::into_inner);
            for (container, rows) in cache.all_listings() {
                if NodeId::parse(&container).is_ok_and(is_track_list) {
                    q.note(&container, rows.into_iter().map(|r| (r.id, r.playable)));
                }
            }
        }
        let _ = self.offline.set(Offline { cache, downloader });
        Ok(())
    }

    /// The finished cached file for a track id (`track/..`), or `None` when it
    /// is not whole on the phone. Counts as a play for eviction.
    pub fn local_path(&self, track_id: &str) -> Option<PathBuf> {
        self.offline.get()?.cache.local_path(track_id)
    }

    /// The tracks after `track_id` in the album or mix it was last listed in,
    /// in play order. Empty when unknown (e.g. it came from a search).
    pub fn play_queue(&self, track_id: &str) -> Vec<String> {
        self.queues
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .after(track_id)
    }

    /// Download `ids` in order in the background (the first should be the
    /// track being played). Replaces the previous request. Never blocks.
    /// Returns how many are not yet cached and will be fetched.
    pub fn prefetch(&self, ids: &[String]) -> usize {
        match self.offline.get() {
            Some(o) => o.downloader.prefetch(ids),
            None => 0,
        }
    }

    /// [`Library::prefetch`] of `track_id` followed by its [`Library::play_queue`].
    pub fn prefetch_from(&self, track_id: &str) -> usize {
        let mut ids = vec![track_id.to_owned()];
        ids.extend(self.play_queue(track_id));
        self.prefetch(&ids)
    }

    /// Keep every track of an album or mix on the phone, never evicted, and
    /// download the missing ones. Returns the number of tracks pinned. Needs
    /// the listing (from the server, or the saved one when offline).
    pub fn pin(&self, container_id: &str) -> Result<usize, ContentError> {
        let id = NodeId::parse(container_id)?;
        if !is_track_list(id) {
            return Err(ContentError::Cache {
                op: format!("pin {container_id}"),
                why: "only an album or a mix can be pinned".into(),
            });
        }
        let o = self.offline.get().ok_or_else(|| ContentError::Cache {
            op: format!("pin {container_id}"),
            why: "the cache is not enabled".into(),
        })?;
        let fetched = if self.online.load(Ordering::SeqCst) {
            self.nodes_or_sign_in(Op::Children(id))
        } else {
            Err(ContentError::Cache {
                op: format!("pin {container_id}"),
                why: "offline".into(),
            })
        };
        let playable = |nodes: Vec<Node>| -> Vec<String> {
            nodes
                .into_iter()
                .filter(|n| n.playable)
                .map(|n| n.id)
                .collect()
        };
        let tracks = match fetched {
            Ok(nodes) => {
                self.note_listing(container_id, id, &nodes);
                playable(nodes)
            }
            Err(e) => match o.cache.load_listing(container_id) {
                Some(rows) => rows
                    .into_iter()
                    .filter(|r| r.playable)
                    .map(|r| r.id)
                    .collect(),
                None => return Err(e),
            },
        };
        o.cache.pin(container_id, &tracks);
        o.downloader.enqueue(&tracks);
        Ok(tracks.len())
    }

    /// Release a pin; its tracks become ordinary, evictable cache entries.
    pub fn unpin(&self, container_id: &str) {
        if let Some(o) = self.offline.get() {
            o.cache.unpin(container_id);
        }
    }

    /// Whether the phone has a network, from the platform's callback. Offline
    /// pauses downloads and makes browsing answer from saved listings at once
    /// instead of waiting for a timeout.
    pub fn set_online(&self, online: bool) {
        self.network_up.store(online, Ordering::SeqCst);
        self.apply_online();
    }

    /// Debug: refuse every server connection (websocket, listings,
    /// downloads) as if the server were unreachable, and go offline. Off
    /// restores what the network callback last said.
    pub fn debug_offline(&self, on: bool) {
        super::fault::set_unreachable(on);
        self.apply_online();
        log::info!("debug offline {}", if on { "on" } else { "off" });
    }

    fn apply_online(&self) {
        let online = self.network_up.load(Ordering::SeqCst) && !super::fault::unreachable();
        self.online.store(online, Ordering::SeqCst);
        if let Some(o) = self.offline.get() {
            o.downloader.set_online(online);
        }
    }

    fn nodes_or_sign_in(&self, op: Op) -> Result<Vec<Node>, ContentError> {
        let (tx, rx) = mpsc::channel();
        let name = op.name();
        self.tx
            .send(Cmd { op, reply: tx })
            .map_err(|_| ContentError::WorkerGone)?;
        match self.wait(&name, &rx) {
            Ok(Out::Nodes(n) | Out::Ranked(n, _)) => Ok(n),
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
        wait_reply(&self.timeouts, name, rx)
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
                Op::Search(q) => tree::search(&link.cat, &urls, q)
                    .await
                    .map(|(n, r)| Out::Ranked(n, r)),
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
        if super::fault::unreachable() {
            self.link = None;
            return Err(ContentError::Connect {
                url: self.base.clone(),
                why: super::fault::WHY.into(),
            });
        }
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

    fn cache_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "pharos-auto-libcache-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn offline_lib(name: &str) -> (Library, PathBuf) {
        let base = dead_server();
        let tok =
            std::env::temp_dir().join(format!("pharos-auto-lib-{name}-{}", std::process::id()));
        let lib = Library::connect_with(&base, &tok, quick()).unwrap();
        let dir = cache_dir(name);
        lib.enable_cache(&dir, 1 << 20).unwrap();
        (lib, dir)
    }

    fn track(n: u32) -> Node {
        Node {
            id: format!("track/{n}"),
            title: format!("T{n}"),
            subtitle: "A".into(),
            browsable: false,
            playable: true,
            art_url: None,
            stream_url: Some("https://h/x?api_key=SECRET".into()),
            duration_ms: 1000,
        }
    }

    fn keep(lib: &Library, id: &str) {
        let c = &lib.offline.get().unwrap().cache;
        std::fs::write(c.part_path(id), b"audio").unwrap();
        c.commit(id, Some(5)).unwrap();
    }

    #[test]
    fn play_queue_is_the_rest_of_the_album_in_order() {
        let (lib, _) = offline_lib("queue");
        let album = NodeId::parse("album/7").unwrap();
        lib.note_listing("album/7", album, &[track(1), track(2), track(3)]);
        assert_eq!(lib.play_queue("track/1"), ["track/2", "track/3"]);
        assert_eq!(lib.play_queue("track/3"), Vec::<String>::new());
        assert_eq!(lib.play_queue("track/99"), Vec::<String>::new());
        // Recent is not a play queue: playing from it queues nothing.
        let recent = NodeId::parse("music/recent").unwrap();
        lib.note_listing("music/recent", recent, &[track(8), track(9)]);
        assert!(lib.play_queue("track/8").is_empty());
    }

    #[test]
    fn local_path_is_only_for_whole_cached_files() {
        let (lib, _) = offline_lib("local");
        assert_eq!(lib.local_path("track/1"), None);
        keep(&lib, "track/1");
        let p = lib.local_path("track/1").unwrap();
        assert_eq!(std::fs::read(p).unwrap(), b"audio");
    }

    #[test]
    fn without_the_cache_nothing_is_local_and_prefetch_is_a_no_op() {
        let lib = Library::connect_with(
            &dead_server(),
            &std::env::temp_dir().join("pharos-auto-lib-nocache"),
            quick(),
        )
        .unwrap();
        assert_eq!(lib.local_path("track/1"), None);
        assert_eq!(lib.prefetch(&["track/1".to_owned()]), 0);
        assert!(lib.pin("album/1").is_err());
    }

    /// Browsing an album with no signal lists the tracks that are on the
    /// phone, and those rows carry no URL (and so no token).
    #[test]
    fn an_unreachable_server_still_lists_the_cached_tracks_of_an_album() {
        let (lib, _) = offline_lib("browse");
        let album = NodeId::parse("album/7").unwrap();
        lib.note_listing("album/7", album, &[track(1), track(2), track(3)]);
        keep(&lib, "track/2");
        let nodes = lib.children("album/7").unwrap();
        let ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, ["track/2"]);
        assert_eq!(nodes[0].stream_url, None);
        assert_eq!(nodes[0].title, "T2");
        // Folders are served as they were last seen.
        lib.note_listing(
            "music",
            NodeId::Music,
            &[Node::folder(NodeId::Albums, "Albums")],
        );
        assert_eq!(lib.children("music").unwrap()[0].id, "music/albums");
        // No snapshot: the original error stands.
        assert!(lib.children("album/8").is_err());
    }

    #[test]
    fn set_offline_answers_from_the_snapshot_without_waiting_for_the_network() {
        let (lib, _) = offline_lib("flag");
        let album = NodeId::parse("album/7").unwrap();
        lib.note_listing("album/7", album, &[track(1)]);
        keep(&lib, "track/1");
        lib.set_online(false);
        let t = std::time::Instant::now();
        assert_eq!(lib.children("album/7").unwrap().len(), 1);
        assert!(t.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn the_play_order_and_snapshots_survive_a_restart() {
        let (lib, dir) = offline_lib("restart");
        let album = NodeId::parse("album/7").unwrap();
        lib.note_listing("album/7", album, &[track(1), track(2)]);
        keep(&lib, "track/1");
        drop(lib);
        let lib2 = Library::connect_with(
            &dead_server(),
            &std::env::temp_dir().join("pharos-auto-lib-restart2"),
            quick(),
        )
        .unwrap();
        lib2.enable_cache(&dir, 1 << 20).unwrap();
        assert_eq!(lib2.play_queue("track/1"), ["track/2"]);
        assert!(lib2.local_path("track/1").is_some());
    }

    #[test]
    fn only_albums_and_mixes_pin_and_a_pin_survives_for_the_tracks_it_names() {
        let (lib, dir) = offline_lib("pin");
        assert!(lib.pin("music/albums").is_err());
        let album = NodeId::parse("album/7").unwrap();
        lib.note_listing("album/7", album, &[track(1), track(2)]);
        lib.set_online(false);
        assert_eq!(lib.pin("album/7").unwrap(), 2);
        let c = &lib.offline.get().unwrap().cache;
        assert_eq!(c.pinned_tracks("album/7").unwrap(), ["track/1", "track/2"]);
        lib.unpin("album/7");
        assert!(!c.is_pinned("album/7"));
        drop(lib);
        let _ = dir;
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
