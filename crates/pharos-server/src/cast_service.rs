//! Server side of the cast hand-off (spec 009): an actor that owns every active
//! cast seat, connects the TV, joins it to the owner's group and ends it again.
//!
//! One task owns all state; callers talk to it through [`CastServiceHandle`].
//! Each cast also has three small owned tasks (group messages to the driver,
//! driver events to the actor, group-closed watcher) that never touch state.

mod hydrate;

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use pharos_cast::castv2::{CastV2Transport, Occupancy, TlsPolicy};
use pharos_cast::drift::PlannerConfig;
use pharos_cast::driver::{self, CastDriverHandle, DriverConfig, DriverEvent, GroupIntent};
use pharos_cast::events::{CastEvent, CastReason};
use pharos_cast::fake::{FakeReceiver, FakeReceiverConfig, FakeReceiverHandle};
use pharos_cast::messages::AccessToken;
use pharos_cast::obs::{self, ConnState};
use pharos_cast::transport::{CastError, CastTransport};
use pharos_core::{TokenId, UserId};
use pharos_sync::group::{CastBinding, CastSeatInfo, GroupHandle, GroupMsg, MemberKind};
use pharos_sync::hub::member_id_for_device;
use pharos_sync::messages::{GroupId, MemberId, ServerMsg};
use pharos_sync::{GroupRegistry, MemberSinks, SessionHub};
use tokio::sync::{mpsc, oneshot, watch, Notify};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::cast::{mint_tv_token, revoke_tv_token, TvToken, DEFAULT_TV_TOKEN_LIFETIME};
use crate::cast_adapter::{command_for, intent_for, playing_entry, ClockPair, GroupCommandRequest};
use crate::config::{CastAddress, CastConfig, ConfiguredTv};
use crate::state::Stores;

/// The Jellyfin cast receiver's app id; asked about before launching so a
/// second sender never steals a TV that is playing something else.
const RECEIVER_APP_ID: &str = "F007D354";
/// Generation the seat's sink is registered under. A seat has exactly one
/// sink for its whole life, so there is no reconnect to fence against.
const SEAT_SINK_GEN: u64 = 1;
/// Group messages buffered for a TV's forwarder; the group drops (and counts)
/// beyond this, as for any slow member.
const SEAT_SINK_CAPACITY: usize = 64;
/// Bound on opening and probing a TV, so one dead TV cannot stall the actor
/// for longer than the connect timeout of the transport plus slack.
const OPEN_TIMEOUT: Duration = Duration::from_secs(12);
/// Bound on the group acknowledging the seat's join.
const JOIN_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a TV gets to load an item before the driver gives up; matches the
/// group's own anti-wedge bound so a stuck TV never outlasts it.
const MAX_LOAD_WAIT: Duration = Duration::from_secs(30);
/// Start-up latency assumed for a command to reach the TV, until measured (T048).
const LATENCY_ESTIMATE: Duration = Duration::from_millis(200);
/// How long ending a cast waits for the driver to tell the TV to stop.
const DRIVER_STOP_WAIT: Duration = Duration::from_secs(2);
/// A seat that goes silent for the group's member TTL (150 s) is pruned as a
/// ghost, so it is pinged well inside that.
const SEAT_PING_INTERVAL: Duration = Duration::from_secs(30);
const COMMAND_QUEUE: usize = 32;
/// Namespace of the lease keys; a v5 id under it is never a group's id.
const TV_LEASE_NS: uuid::Uuid = uuid::Uuid::from_u128(0x5c0d_91a4_3e7b_4f12_b6a8_20d4_7e19_c3b5);

/// The cross-replica lease that gives one replica a TV's Cast connection.
pub fn tv_lease_key(tv_name: &str) -> GroupId {
    GroupId(uuid::Uuid::new_v5(&TV_LEASE_NS, tv_name.as_bytes()))
}

/// What a user asks for when they pick a TV.
#[derive(Debug, Clone)]
pub struct StartCast {
    pub user_id: UserId,
    pub user_name: String,
    /// The session-hub key of the owner's player, `"{user}:{device}"`.
    pub owner_sync_key: String,
    pub tv_name: String,
}

/// Why a start was refused. Each variant is a stable metric label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastRejectReason {
    NotInGroup,
    TvBusy,
    TvUnreachable,
    UserHasSeat,
    NotPermitted,
    TokenFailed,
    /// The group could not take the seat, or the service is not running.
    Unavailable,
    /// The group runs on another replica, which alone may hold its TV.
    NotGroupOwner,
}

impl CastRejectReason {
    pub const ALL: [CastRejectReason; 8] = [
        Self::NotInGroup,
        Self::TvBusy,
        Self::TvUnreachable,
        Self::UserHasSeat,
        Self::NotPermitted,
        Self::TokenFailed,
        Self::Unavailable,
        Self::NotGroupOwner,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::NotInGroup => "not_in_group",
            Self::TvBusy => "tv_busy",
            Self::TvUnreachable => "tv_unreachable",
            Self::UserHasSeat => "user_has_seat",
            Self::NotPermitted => "not_permitted",
            Self::TokenFailed => "token_failed",
            Self::Unavailable => "unavailable",
            Self::NotGroupOwner => "not_group_owner",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("cast refused ({}): {message}", .reason.label())]
pub struct CastReject {
    pub reason: CastRejectReason,
    /// Readable, names the offending value (TV, app id, ...).
    pub message: String,
}

/// A point-in-time view of one cast, for tests and diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CastInfo {
    pub tv: String,
    pub owner: UserId,
    pub group: GroupId,
    pub seat_member: MemberId,
    pub owner_member: MemberId,
    pub state: ConnState,
}

/// Opens the connection to a TV and says what it is running.
pub trait TransportFactory: Send + Sync + 'static {
    type Transport: CastTransport + 'static;

    fn open(
        &self,
        tv: &ConfiguredTv,
    ) -> impl Future<Output = Result<(Self::Transport, Occupancy), CastError>> + Send;
}

/// The real factory: a CASTV2 connection over TLS to the configured address.
#[derive(Debug, Clone, Copy, Default)]
pub struct CastV2Factory;

impl TransportFactory for CastV2Factory {
    type Transport = CastV2Transport;

    async fn open(&self, tv: &ConfiguredTv) -> Result<(CastV2Transport, Occupancy), CastError> {
        let mut transport = CastV2Transport::new(
            tv.address.host.clone(),
            tv.address.port,
            TlsPolicy::AcceptAny,
        );
        transport.connect().await?;
        let occupancy = transport.busy_with_foreign_app(RECEIVER_APP_ID).await;
        // The driver connects again itself; leave this one closed.
        transport.close().await;
        Ok((transport, occupancy?))
    }
}

/// Everything the service needs from the running server.
#[derive(Clone)]
pub struct CastDeps {
    pub hub: SessionHub,
    pub registry: GroupRegistry,
    pub sinks: MemberSinks,
    pub stores: Stores,
}

enum Command {
    Start {
        req: StartCast,
        reply: oneshot::Sender<Result<(), CastReject>>,
    },
    Stop {
        user: UserId,
        reason: CastReason,
        done: oneshot::Sender<()>,
    },
    Snapshot {
        reply: oneshot::Sender<Vec<CastInfo>>,
    },
    /// A persisted seat found in a group this replica runs.
    Resume {
        group: GroupHandle,
        seat: CastSeatInfo,
    },
}

/// Cheap to clone; every method is a message to the actor.
#[derive(Clone)]
pub struct CastServiceHandle {
    tx: mpsc::Sender<Command>,
}

impl CastServiceHandle {
    /// Hand the user's seat in their group over to `tv_name`.
    pub async fn start(&self, req: StartCast) -> Result<(), CastReject> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Command::Start { req, reply })
            .await
            .map_err(|_| unavailable())?;
        rx.await.map_err(|_| unavailable())?
    }

    /// End `user`'s cast, if any. Idempotent. Returns once the group, the token
    /// and the owner are restored.
    pub async fn stop(&self, user: UserId, reason: CastReason) {
        let (done, rx) = oneshot::channel();
        if self
            .tx
            .send(Command::Stop { user, reason, done })
            .await
            .is_ok()
        {
            let _ = rx.await;
        }
    }

    pub async fn snapshot(&self) -> Vec<CastInfo> {
        let (reply, rx) = oneshot::channel();
        if self.tx.send(Command::Snapshot { reply }).await.is_err() {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }
}

fn unavailable() -> CastReject {
    CastReject {
        reason: CastRejectReason::Unavailable,
        message: "the cast service is not running".into(),
    }
}

/// Start the actor. It lives until every handle is dropped.
pub fn spawn<F: TransportFactory>(
    deps: CastDeps,
    cfg: CastConfig,
    factory: F,
) -> CastServiceHandle {
    let (tx, rx) = mpsc::channel(COMMAND_QUEUE);
    let (internal_tx, internal_rx) = mpsc::channel(COMMAND_QUEUE);
    let (opened_tx, opened_rx) = mpsc::channel(COMMAND_QUEUE);
    let (resumed_tx, resumed_rx) = mpsc::channel(COMMAND_QUEUE);
    hydrate::spawn_listener(deps.registry.clone(), tx.downgrade());
    let actor = Actor {
        deps,
        cfg,
        factory: Arc::new(factory),
        casts: HashMap::new(),
        by_user: HashMap::new(),
        pending: HashMap::new(),
        resuming: HashMap::new(),
        next_id: 0,
        internal_tx,
        opened_tx,
        resumed_tx,
    };
    tokio::spawn(actor.run(rx, internal_rx, opened_rx, resumed_rx));
    CastServiceHandle { tx }
}

enum Internal {
    Driver {
        tv: String,
        id: u64,
        event: DriverEvent,
    },
    GroupGone {
        tv: String,
        id: u64,
    },
}

/// Who finished the cast, which decides who has already recorded it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EndedBy {
    /// The driver ended itself and recorded `Ended` with its own reason.
    Driver,
    /// The service ends it; the TV still has to be told to stop.
    Service,
}

struct ActiveCast {
    id: u64,
    tv: String,
    owner: UserId,
    owner_name: String,
    /// `None` for a resumed cast, whose owner's device is not known.
    owner_key: Option<String>,
    sender_device: String,
    group: GroupHandle,
    owner_member: MemberId,
    seat_member: MemberId,
    token_id: TokenId,
    driver: Arc<CastDriverHandle>,
    state: ConnState,
    forwarder: JoinHandle<()>,
    pump: JoinHandle<()>,
    watcher: JoinHandle<()>,
}

/// What `TransportFactory::open` produced within `OPEN_TIMEOUT`.
type OpenOutcome<T> = Result<Result<(T, Occupancy), CastError>, tokio::time::error::Elapsed>;

/// A connect task reporting back to the actor.
struct Opened<T> {
    tv: String,
    outcome: OpenOutcome<T>,
}

/// Everything a start needs once the TV has answered.
struct StartParams {
    req: StartCast,
    tv: ConfiguredTv,
    owner_member: MemberId,
    group: GroupHandle,
    group_label: String,
}

/// A start whose TV connect is in flight. Holding it reserves the TV and the
/// user, so a concurrent start is refused instead of racing.
struct PendingStart {
    params: StartParams,
    reply: oneshot::Sender<Result<(), CastReject>>,
    /// The task opening the TV; aborted when the start is cancelled.
    connect: JoinHandle<()>,
}

struct Actor<F: TransportFactory> {
    deps: CastDeps,
    cfg: CastConfig,
    factory: Arc<F>,
    casts: HashMap<String, ActiveCast>,
    by_user: HashMap<UserId, String>,
    pending: HashMap<String, PendingStart>,
    /// Persisted seats whose TV is connecting, by TV name.
    resuming: HashMap<String, hydrate::ResumeParams>,
    next_id: u64,
    internal_tx: mpsc::Sender<Internal>,
    opened_tx: mpsc::Sender<Opened<F::Transport>>,
    resumed_tx: mpsc::Sender<hydrate::Resumed<F::Transport>>,
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn permitted(tv: &ConfiguredTv, user: UserId) -> bool {
    let wanted = user.0.simple().to_string();
    tv.allowed_users.is_empty()
        || tv
            .allowed_users
            .iter()
            .any(|u| u.replace('-', "").to_lowercase() == wanted)
}

fn reason_of(reject: CastRejectReason) -> CastReason {
    match reject {
        CastRejectReason::NotInGroup => CastReason::NotInGroup,
        CastRejectReason::TvBusy => CastReason::TvBusy,
        CastRejectReason::TvUnreachable => CastReason::TvUnreachable,
        CastRejectReason::UserHasSeat => CastReason::UserHasSeat,
        CastRejectReason::NotPermitted => CastReason::NotPermitted,
        CastRejectReason::TokenFailed => CastReason::TokenFailed,
        CastRejectReason::Unavailable => CastReason::GroupGone,
        CastRejectReason::NotGroupOwner => CastReason::NotGroupOwner,
    }
}

impl<F: TransportFactory> Actor<F> {
    async fn run(
        mut self,
        mut commands: mpsc::Receiver<Command>,
        mut internal: mpsc::Receiver<Internal>,
        mut opened: mpsc::Receiver<Opened<F::Transport>>,
        mut resumed: mpsc::Receiver<hydrate::Resumed<F::Transport>>,
    ) {
        let mut ping = tokio::time::interval(SEAT_PING_INTERVAL);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ping.tick().await;
        loop {
            tokio::select! {
                cmd = commands.recv() => match cmd {
                    Some(cmd) => self.on_command(cmd).await,
                    None => break,
                },
                Some(msg) = internal.recv() => self.on_internal(msg).await,
                Some(done) = opened.recv() => self.on_opened(done).await,
                Some(done) = resumed.recv() => self.on_resumed(done).await,
                _ = ping.tick() => self.ping_seats(),
            }
        }
        self.release_all().await;
    }

    /// The service is going away with the process. The group keeps its seats so
    /// the next process (or replica) can resume them from the snapshot, and the
    /// tokens are revoked by that resume: only the tasks and the sinks go.
    async fn release_all(&mut self) {
        for (_, cast) in self.casts.drain() {
            cast.forwarder.abort();
            cast.pump.abort();
            cast.watcher.abort();
            cast.driver.abort();
            self.deps.sinks.remove(cast.seat_member, SEAT_SINK_GEN);
            if let Err(e) = self
                .deps
                .registry
                .release_lease(tv_lease_key(&cast.tv))
                .await
            {
                tracing::warn!(tv = %cast.tv, error = %e, "cast: the TV lease could not be released");
            }
        }
        self.by_user.clear();
    }

    async fn on_command(&mut self, cmd: Command) {
        match cmd {
            Command::Start { req, reply } => self.begin_start(req, reply).await,
            Command::Stop { user, reason, done } => {
                self.end(user, reason, EndedBy::Service).await;
                let _ = done.send(());
            }
            Command::Resume { group, seat } => self.begin_resume(group, seat).await,
            Command::Snapshot { reply } => {
                let mut all: Vec<CastInfo> = self
                    .casts
                    .values()
                    .map(|c| CastInfo {
                        tv: c.tv.clone(),
                        owner: c.owner,
                        group: c.group.group_id,
                        seat_member: c.seat_member,
                        owner_member: c.owner_member,
                        state: c.state,
                    })
                    .collect();
                all.extend(self.pending.values().map(|p| {
                    let StartParams {
                        req,
                        tv,
                        owner_member,
                        group,
                        ..
                    } = &p.params;
                    CastInfo {
                        tv: tv.name.clone(),
                        owner: req.user_id,
                        group: group.group_id,
                        seat_member: member_id_for_device(&format!(
                            "{}:cast:{}",
                            req.user_id.0.simple(),
                            tv.name
                        )),
                        owner_member: *owner_member,
                        state: ConnState::Connecting,
                    }
                }));
                all.sort_by(|a, b| a.tv.cmp(&b.tv));
                let _ = reply.send(all);
            }
        }
    }

    fn reject(
        &self,
        req: &StartCast,
        group: &str,
        reason: CastRejectReason,
        tv: &str,
        message: String,
    ) -> CastReject {
        obs::record_cast_event(
            CastEvent::Rejected,
            reason_of(reason),
            &req.user_name,
            device_of(&req.owner_sync_key),
            tv,
            group,
            &message,
        );
        CastReject { reason, message }
    }

    /// Validate the request and reserve its TV and user; the connect itself
    /// runs in its own task so the actor stays responsive.
    async fn begin_start(
        &mut self,
        req: StartCast,
        reply: oneshot::Sender<Result<(), CastReject>>,
    ) {
        let params = match self.reserve(req) {
            Ok(params) => params,
            Err(reject) => {
                let _ = reply.send(Err(reject));
                return;
            }
        };
        if let Err(reject) = self.claim(&params).await {
            let _ = reply.send(Err(reject));
            return;
        }
        let tv = params.tv.clone();
        let factory = Arc::clone(&self.factory);
        let opened_tx = self.opened_tx.clone();
        let tv_name = tv.name.clone();
        let connect = tokio::spawn(async move {
            let outcome = tokio::time::timeout(OPEN_TIMEOUT, factory.open(&tv)).await;
            let _ = opened_tx
                .send(Opened {
                    tv: tv.name,
                    outcome,
                })
                .await;
        });
        self.pending.insert(
            tv_name,
            PendingStart {
                params,
                reply,
                connect,
            },
        );
    }

    /// Cancel the start `user` has in flight, if any: stop connecting, give the
    /// TV lease back and answer the waiting request. Nothing has touched the
    /// group yet, so there is nothing else to undo.
    async fn cancel_pending(&mut self, user: UserId, reason: CastReason) {
        let Some(tv) = self
            .pending
            .iter()
            .find(|(_, p)| p.params.req.user_id == user)
            .map(|(tv, _)| tv.clone())
        else {
            return;
        };
        let Some(PendingStart {
            params,
            reply,
            connect,
        }) = self.pending.remove(&tv)
        else {
            return;
        };
        connect.abort();
        self.release_tv(&tv).await;
        let message = format!(
            "the start for {:?} was cancelled ({}) while the TV was connecting",
            tv,
            reason.label()
        );
        obs::record_cast_event(
            CastEvent::Ended,
            reason,
            &params.req.user_name,
            device_of(&params.req.owner_sync_key),
            &tv,
            &params.group_label,
            &message,
        );
        let rejected = match reason {
            CastReason::OwnerLeft => CastRejectReason::NotInGroup,
            _ => CastRejectReason::Unavailable,
        };
        let _ = reply.send(Err(CastReject {
            reason: rejected,
            message,
        }));
    }

    async fn on_opened(&mut self, done: Opened<F::Transport>) {
        let Some(PendingStart { params, reply, .. }) = self.pending.remove(&done.tv) else {
            return;
        };
        let result = self.complete_start(params, done.outcome).await;
        if result.is_err() {
            self.release_tv(&done.tv).await;
        }
        let _ = reply.send(result);
    }

    /// Only the replica that runs the group may hold its TV, and only one
    /// replica may hold a TV at all; both are settled before any connection.
    async fn claim(&self, params: &StartParams) -> Result<(), CastReject> {
        let StartParams {
            req,
            tv,
            group,
            group_label,
            ..
        } = params;
        match self.deps.registry.is_owned(group.group_id).await {
            Ok(true) => {}
            Ok(false) => {
                return Err(self.reject(
                    req,
                    group_label,
                    CastRejectReason::NotGroupOwner,
                    &tv.name,
                    format!(
                        "group {group_label} runs on another replica, which alone may connect {:?}; \
                         retry the request",
                        tv.name
                    ),
                ));
            }
            Err(e) => {
                return Err(self.reject(
                    req,
                    group_label,
                    CastRejectReason::Unavailable,
                    &tv.name,
                    format!("cannot tell whether this replica runs group {group_label}: {e}"),
                ));
            }
        }
        match self.deps.registry.try_lease(tv_lease_key(&tv.name)).await {
            Ok(true) => Ok(()),
            Ok(false) => Err(self.reject(
                req,
                group_label,
                CastRejectReason::TvBusy,
                &tv.name,
                format!("{:?} is connected from another replica", tv.name),
            )),
            Err(e) => Err(self.reject(
                req,
                group_label,
                CastRejectReason::Unavailable,
                &tv.name,
                format!("cannot lease {:?}: {e}", tv.name),
            )),
        }
    }

    /// Give the TV's cross-replica lease back.
    async fn release_tv(&self, tv: &str) {
        if let Err(e) = self.deps.registry.release_lease(tv_lease_key(tv)).await {
            tracing::warn!(tv, error = %e, "cast: the TV lease could not be released");
        }
    }

    fn reserve(&self, req: StartCast) -> Result<StartParams, CastReject> {
        let Some(tv) = self.cfg.tv(&req.tv_name).cloned() else {
            return Err(self.reject(
                &req,
                "",
                CastRejectReason::NotPermitted,
                &req.tv_name,
                format!("no TV named {:?} is configured", req.tv_name),
            ));
        };
        if !permitted(&tv, req.user_id) {
            return Err(self.reject(
                &req,
                "",
                CastRejectReason::NotPermitted,
                &tv.name,
                format!("user {:?} may not cast to {:?}", req.user_name, tv.name),
            ));
        }

        let session = self.deps.hub.resolve(&req.owner_sync_key);
        let Some((owner_member, group)) = session.and_then(|s| {
            let group = s.group.filter(|g| !g.tx.is_closed())?;
            Some((s.member_id, group))
        }) else {
            return Err(self.reject(
                &req,
                "",
                CastRejectReason::NotInGroup,
                &tv.name,
                format!(
                    "user {:?} is not in a SyncPlay group (session {:?}); join one before casting",
                    req.user_name, req.owner_sync_key
                ),
            ));
        };
        let group_label = group.group_id.to_string();

        let in_flight = self
            .pending
            .values()
            .find(|p| p.params.req.user_id == req.user_id);
        if let Some(existing) = self
            .by_user
            .get(&req.user_id)
            .or_else(|| in_flight.map(|p| &p.params.tv.name))
        {
            return Err(self.reject(
                &req,
                &group_label,
                CastRejectReason::UserHasSeat,
                &tv.name,
                format!(
                    "user {:?} already has a seat on {existing:?}; stop that cast first",
                    req.user_name
                ),
            ));
        }
        if self.casts.contains_key(&tv.name)
            || self.pending.contains_key(&tv.name)
            || self.resuming.contains_key(&tv.name)
        {
            return Err(self.reject(
                &req,
                &group_label,
                CastRejectReason::TvBusy,
                &tv.name,
                format!("{:?} already has a cast seat", tv.name),
            ));
        }

        Ok(StartParams {
            req,
            tv,
            owner_member,
            group,
            group_label,
        })
    }

    fn owner_still_in_group(
        &self,
        req: &StartCast,
        owner_member: MemberId,
        group: &GroupHandle,
    ) -> bool {
        self.deps.hub.resolve(&req.owner_sync_key).is_some_and(|s| {
            s.member_id == owner_member
                && s.group
                    .is_some_and(|g| g.group_id == group.group_id && !g.tx.is_closed())
        })
    }

    async fn complete_start(
        &mut self,
        params: StartParams,
        opened: OpenOutcome<F::Transport>,
    ) -> Result<(), CastReject> {
        let StartParams {
            req,
            tv,
            owner_member,
            group,
            group_label,
        } = params;
        let (transport, occupancy) = match opened {
            Ok(Ok(opened)) => opened,
            Ok(Err(e)) => {
                return Err(self.reject(
                    &req,
                    &group_label,
                    CastRejectReason::TvUnreachable,
                    &tv.name,
                    format!("cannot reach {:?} at {}: {e}", tv.name, tv.address),
                ));
            }
            Err(_) => {
                return Err(self.reject(
                    &req,
                    &group_label,
                    CastRejectReason::TvUnreachable,
                    &tv.name,
                    format!(
                        "{:?} at {} did not answer within {OPEN_TIMEOUT:?}",
                        tv.name, tv.address
                    ),
                ));
            }
        };
        if let Occupancy::ForeignApp {
            app_id,
            display_name,
        } = occupancy
        {
            return Err(self.reject(
                &req,
                &group_label,
                CastRejectReason::TvBusy,
                &tv.name,
                format!(
                    "{:?} is running another app: {app_id} ({})",
                    tv.name,
                    display_name.as_deref().unwrap_or("unnamed")
                ),
            ));
        }

        let token = match mint_tv_token(
            &self.deps.stores,
            req.user_id,
            &tv.name,
            DEFAULT_TV_TOKEN_LIFETIME,
        )
        .await
        {
            Ok(t) => t,
            Err(e) => {
                return Err(self.reject(
                    &req,
                    &group_label,
                    CastRejectReason::TokenFailed,
                    &tv.name,
                    format!("cannot mint a TV token: {e}"),
                ));
            }
        };

        // The connect and the mint both awaited; the owner may have left since.
        if !self.owner_still_in_group(&req, owner_member, &group) {
            if let Err(e) = revoke_tv_token(&self.deps.stores, &token.id).await {
                tracing::error!(
                    tv = %tv.name, token = %token.id.0, error = %e,
                    "cast: the TV token of an abandoned start could not be revoked; it expires on its own"
                );
            }
            return Err(self.reject(
                &req,
                &group_label,
                CastRejectReason::NotInGroup,
                &tv.name,
                format!(
                    "user {:?} left group {group_label} while {:?} was connecting; no seat was added",
                    req.user_name, tv.name
                ),
            ));
        }

        let mut cast = self.assemble(
            transport,
            &token,
            Assembly {
                tv: &tv,
                owner: req.user_id,
                owner_name: &req.user_name,
                owner_key: Some(req.owner_sync_key.clone()),
                sender_device: device_of(&req.owner_sync_key).to_owned(),
                group: &group,
                owner_member,
            },
        );
        let seat_member = cast.seat_member;

        let binding = CastBinding {
            tv: tv.name.clone(),
            owner: req.user_id.0.simple().to_string(),
        };
        // The owner becomes the Controller first so the seat is paired, and so
        // hidden, from the moment it joins.
        let _ = group
            .tx
            .send(GroupMsg::SetMemberKind {
                member: owner_member,
                kind: MemberKind::Controller,
                cast: Some(binding.clone()),
            })
            .await;
        let (join_tx, join_rx) = oneshot::channel();
        let joined = group
            .tx
            .send(GroupMsg::AddCastSeat {
                member_id: seat_member,
                name: req.user_name.clone(),
                cast: binding,
                reply: join_tx,
            })
            .await
            .is_ok()
            && matches!(tokio::time::timeout(JOIN_TIMEOUT, join_rx).await, Ok(Ok(_)));
        if !joined {
            let message = format!(
                "group {group_label} did not accept the seat for {:?} within {JOIN_TIMEOUT:?}",
                tv.name
            );
            obs::record_cast_event(
                CastEvent::HandoffFailed,
                CastReason::GroupGone,
                &req.user_name,
                &cast.sender_device,
                &tv.name,
                &group_label,
                &message,
            );
            self.teardown(&mut cast, EndedBy::Service, CastReason::GroupGone)
                .await;
            return Err(CastReject {
                reason: CastRejectReason::Unavailable,
                message,
            });
        }

        obs::record_cast_event(
            CastEvent::Started,
            CastReason::UserStop,
            &req.user_name,
            &cast.sender_device,
            &tv.name,
            &group_label,
            &format!("seat {seat_member} on {}", tv.address),
        );
        self.by_user.insert(req.user_id, tv.name.clone());
        self.casts.insert(tv.name.clone(), cast);
        Ok(())
    }

    async fn on_internal(&mut self, msg: Internal) {
        match msg {
            Internal::GroupGone { tv, id } => {
                if let Some(owner) = self.owner_of(&tv, id) {
                    self.end(owner, CastReason::GroupGone, EndedBy::Service)
                        .await;
                }
            }
            Internal::Driver { tv, id, event } => {
                let Some(owner) = self.owner_of(&tv, id) else {
                    return;
                };
                match event {
                    DriverEvent::State(state) => self.on_state(&tv, state).await,
                    DriverEvent::Ended { reason } => {
                        self.end(owner, reason, EndedBy::Driver).await;
                    }
                    other => match command_for(&other) {
                        Some(GroupCommandRequest::StopCast) => {
                            self.end(owner, CastReason::UserStop, EndedBy::Service)
                                .await;
                        }
                        Some(command) => self.dispatch_owner_command(&tv, command).await,
                        None => {}
                    },
                }
            }
        }
    }

    fn owner_of(&self, tv: &str, id: u64) -> Option<UserId> {
        self.casts.get(tv).filter(|c| c.id == id).map(|c| c.owner)
    }

    async fn on_state(&mut self, tv: &str, state: ConnState) {
        let Some(cast) = self.casts.get_mut(tv) else {
            return;
        };
        let previous = std::mem::replace(&mut cast.state, state);
        let seat = cast.seat_member;
        let group = cast.group.tx.clone();
        match (previous, state) {
            (_, ConnState::Lost) => {
                let _ = group
                    .send(GroupMsg::MemberSocketLost { member_id: seat })
                    .await;
            }
            (ConnState::Lost, ConnState::Following) => {
                let _ = group.send(GroupMsg::ResyncMember { member_id: seat }).await;
            }
            _ => {}
        }
    }

    /// Apply what someone did on the TV's own remote to the group, through the
    /// path the HTTP SyncPlay handlers use, as the owner.
    async fn dispatch_owner_command(&self, tv: &str, command: GroupCommandRequest) {
        let Some(cast) = self.casts.get(tv) else {
            return;
        };
        let label = command.label();
        let Some(owner_key) = cast.owner_key.clone() else {
            // A resumed cast does not know the owner's device, but it knows the
            // owner's member: speak to the group as that member.
            let msg = command_msg(command, cast.owner_member);
            if cast.group.tx.send(msg).await.is_err() {
                tracing::warn!(
                    tv, owner = %cast.owner_name, command = label,
                    "cast: a command from the TV's remote found its group gone"
                );
            }
            return;
        };
        let response = crate::api::jellyfin::syncplay::dispatch(
            &self.deps.hub,
            &self.deps.registry,
            &self.deps.stores,
            Some(owner_key),
            label,
            move |sender| command_msg(command, sender),
        )
        .await;
        if !response.status().is_success() {
            tracing::warn!(
                tv, owner = %cast.owner_name, command = label, status = %response.status(),
                "cast: a command from the TV's remote was not applied to the group"
            );
        }
    }

    fn ping_seats(&self) {
        for cast in self.casts.values() {
            let _ = cast.group.tx.try_send(GroupMsg::MemberPing {
                member_id: cast.seat_member,
            });
        }
    }

    /// End `user`'s cast. Idempotent.
    async fn end(&mut self, user: UserId, reason: CastReason, by: EndedBy) {
        let Some(tv) = self.by_user.remove(&user) else {
            self.cancel_pending(user, reason).await;
            return;
        };
        let Some(mut cast) = self.casts.remove(&tv) else {
            return;
        };
        self.teardown(&mut cast, by, reason).await;
        // The owner's local player was stopped at hand-off; the late-joiner
        // catch-up restarts it at the group's position. A leaving owner is
        // about to be removed, so catching it up would only contradict GroupLeft.
        if reason != CastReason::OwnerLeft {
            let _ = cast
                .group
                .tx
                .send(GroupMsg::ResyncMember {
                    member_id: cast.owner_member,
                })
                .await;
        }
        tracing::info!(
            tv = %cast.tv, owner = %cast.owner_name, group = %cast.group.group_id,
            reason = reason.label(), "cast ended"
        );
    }

    /// Release everything a cast holds: tasks, driver, sink, group seat, token.
    async fn teardown(&self, cast: &mut ActiveCast, by: EndedBy, reason: CastReason) {
        cast.forwarder.abort();
        cast.pump.abort();
        cast.watcher.abort();
        let _ = (&mut cast.forwarder).await;

        if by == EndedBy::Service {
            let _ = cast.driver.send(GroupIntent::Stop { reason }).await;
        }
        self.finish_driver(cast, by).await;

        self.deps.sinks.remove(cast.seat_member, SEAT_SINK_GEN);
        let _ = cast
            .group
            .tx
            .send(GroupMsg::RemoveMember {
                member_id: cast.seat_member,
            })
            .await;
        let _ = cast
            .group
            .tx
            .send(GroupMsg::SetMemberKind {
                member: cast.owner_member,
                kind: MemberKind::Player,
                cast: None,
            })
            .await;
        self.release_tv(&cast.tv).await;
        if let Err(e) = revoke_tv_token(&self.deps.stores, &cast.token_id).await {
            tracing::error!(
                tv = %cast.tv, token = %cast.token_id.0, error = %e,
                "cast: the TV token could not be revoked; it expires on its own"
            );
        }
    }

    async fn finish_driver(&self, cast: &ActiveCast, by: EndedBy) {
        if by == EndedBy::Service {
            let deadline = Instant::now() + DRIVER_STOP_WAIT;
            while !cast.driver.is_finished() && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        if !cast.driver.is_finished() {
            cast.driver.abort();
        }
    }
}

/// What [`Actor::assemble`] needs to wire a cast together, whether it was just
/// started or is being resumed.
struct Assembly<'a> {
    tv: &'a ConfiguredTv,
    owner: UserId,
    owner_name: &'a str,
    owner_key: Option<String>,
    sender_device: String,
    group: &'a GroupHandle,
    owner_member: MemberId,
}

impl<F: TransportFactory> Actor<F> {
    /// Spawn the driver, attach the seat's sink and start the three owned
    /// tasks. The group is not touched: the caller decides how the seat joins.
    fn assemble(
        &mut self,
        transport: F::Transport,
        token: &TvToken,
        a: Assembly<'_>,
    ) -> ActiveCast {
        let (events_tx, events_rx) = mpsc::channel(64);
        let driver = Arc::new(driver::spawn(
            transport,
            DriverConfig {
                planner: PlannerConfig {
                    threshold_ms: self.cfg.drift_threshold_ms,
                    cooldown_ms: self.cfg.correction_cooldown_ms,
                    ..PlannerConfig::default()
                },
                reconnect_grace: Duration::from_secs(self.cfg.reconnect_grace_secs),
                server_url: self.cfg.server_url.clone().unwrap_or_default(),
                receiver_name: a.tv.name.clone(),
                access_token: AccessToken::new(token.secret().expose()),
                latency_estimate: LATENCY_ESTIMATE,
                max_load_wait: MAX_LOAD_WAIT,
            },
            events_tx,
        ));

        self.next_id = self.next_id.wrapping_add(1);
        let id = self.next_id;
        let seat_member =
            member_id_for_device(&format!("{}:cast:{}", a.owner.0.simple(), a.tv.name));
        let seat_rx =
            self.deps
                .sinks
                .attach_channel(seat_member, SEAT_SINK_GEN, SEAT_SINK_CAPACITY);
        let forwarder = tokio::spawn(forward(
            seat_rx,
            Arc::clone(&driver),
            a.group.epoch_unix_ms,
            a.tv.name.clone(),
        ));
        let pump = tokio::spawn(pump_events(
            events_rx,
            self.internal_tx.clone(),
            a.tv.name.clone(),
            id,
        ));
        let watcher = tokio::spawn(watch_group(
            a.group.tx.clone(),
            self.internal_tx.clone(),
            a.tv.name.clone(),
            id,
        ));
        ActiveCast {
            id,
            tv: a.tv.name.clone(),
            owner: a.owner,
            owner_name: a.owner_name.to_owned(),
            owner_key: a.owner_key,
            sender_device: a.sender_device,
            group: a.group.clone(),
            owner_member: a.owner_member,
            seat_member,
            token_id: token.id.clone(),
            driver,
            state: ConnState::Connecting,
            forwarder,
            pump,
            watcher,
        }
    }
}

fn command_msg(command: GroupCommandRequest, sender: MemberId) -> GroupMsg {
    match command {
        GroupCommandRequest::Pause => GroupMsg::PauseShared { sender },
        GroupCommandRequest::Unpause => GroupMsg::Unpause { sender },
        GroupCommandRequest::Seek { position_ms } => GroupMsg::SeekTo {
            sender,
            position_ms,
        },
        GroupCommandRequest::StopCast => GroupMsg::MemberPing { member_id: sender },
    }
}

fn device_of(sync_key: &str) -> &str {
    sync_key.split_once(':').map_or(sync_key, |(_, d)| d)
}

/// Group messages for the seat, turned into driver intents.
async fn forward(
    mut rx: mpsc::Receiver<ServerMsg>,
    driver: Arc<CastDriverHandle>,
    epoch_unix_ms: u64,
    tv: String,
) {
    let mut last_entry: Option<String> = None;
    while let Some(msg) = rx.recv().await {
        if let Some(entry) = playing_entry(&msg) {
            if last_entry.as_deref() == Some(entry) {
                tracing::debug!(tv, entry, "cast: queue re-sent for the item already loaded");
                continue;
            }
            last_entry = Some(entry.to_owned());
        }
        let clock = ClockPair {
            now_server_ms: unix_now_ms().saturating_sub(epoch_unix_ms),
            now: Instant::now(),
        };
        let Some(intent) = intent_for(&msg, clock) else {
            continue;
        };
        if driver.send(intent).await.is_err() {
            break;
        }
    }
}

async fn pump_events(
    mut events: mpsc::Receiver<DriverEvent>,
    to_actor: mpsc::Sender<Internal>,
    tv: String,
    id: u64,
) {
    while let Some(event) = events.recv().await {
        let msg = Internal::Driver {
            tv: tv.clone(),
            id,
            event,
        };
        if to_actor.send(msg).await.is_err() {
            break;
        }
    }
}

/// Resolves when the group's actor has terminated.
async fn watch_group(
    group: mpsc::Sender<GroupMsg>,
    to_actor: mpsc::Sender<Internal>,
    tv: String,
    id: u64,
) {
    group.closed().await;
    let _ = to_actor.send(Internal::GroupGone { tv, id }).await;
}

/// TV name -> whether it accepted a TCP connection on its last probe.
pub type Reachability = HashMap<String, bool>;

const PROBE_INTERVAL: Duration = Duration::from_secs(30);
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

async fn probe(address: &CastAddress) -> bool {
    matches!(
        tokio::time::timeout(
            PROBE_TIMEOUT,
            tokio::net::TcpStream::connect((address.host.as_str(), address.port)),
        )
        .await,
        Ok(Ok(_))
    )
}

/// Probe every configured TV every 30 s and publish who answered. With no TVs
/// configured nothing is spawned and the receiver holds an empty map.
pub fn spawn_reachability_prober(tvs: &[ConfiguredTv]) -> watch::Receiver<Reachability> {
    let (tx, rx) = watch::channel(Reachability::new());
    if tvs.is_empty() {
        return rx;
    }
    let tvs: Vec<(String, CastAddress)> = tvs
        .iter()
        .map(|t| (t.name.clone(), t.address.clone()))
        .collect();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(PROBE_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let results = futures_util::future::join_all(
                tvs.iter()
                    .map(|(name, addr)| async move { (name.clone(), probe(addr).await) }),
            )
            .await;
            let map: Reachability = results.into_iter().collect();
            for (name, up) in &map {
                tracing::debug!(tv = %name, reachable = up, "cast: TV reachability probed");
            }
            if tx.send(map).is_err() {
                break;
            }
        }
    });
    rx
}

/// What `AppState` holds for casting: the reachability of each configured TV
/// and, once the group machinery exists, the service handle. Inert (no task, no
/// service) when no TV is configured.
#[derive(Clone)]
pub struct CastRuntime {
    service: Arc<OnceLock<CastServiceHandle>>,
    reachable: watch::Receiver<Reachability>,
}

impl CastRuntime {
    /// No TVs, no service, an empty reachability map.
    pub fn inert() -> Self {
        Self::start(&[])
    }

    /// Begin probing `tvs`. Needs a running tokio runtime when `tvs` is non-empty.
    pub fn start(tvs: &[ConfiguredTv]) -> Self {
        Self {
            service: Arc::new(OnceLock::new()),
            reachable: spawn_reachability_prober(tvs),
        }
    }

    /// Attach the running service; the first install wins.
    pub fn install(&self, handle: CastServiceHandle) {
        let _ = self.service.set(handle);
    }

    /// The service, if casting is configured and started.
    pub fn service(&self) -> Option<CastServiceHandle> {
        self.service.get().cloned()
    }

    /// TV name -> reachable at the last probe. A TV not yet probed is absent.
    pub fn reachable_tvs(&self) -> Reachability {
        self.reachable.borrow().clone()
    }
}

/// Start the cast service for `cfg` and install it into `runtime`. A no-op with
/// no `[[cast.tvs]]`, so an unconfigured server pays nothing.
pub fn start_service(runtime: &CastRuntime, deps: CastDeps, cfg: &CastConfig) {
    if cfg.tvs.is_empty() {
        return;
    }
    runtime.install(spawn(deps, cfg.clone(), CastV2Factory));
    tracing::info!(tvs = cfg.tvs.len(), "cast: service started");
}

/// Test support: hands out pre-built [`FakeReceiver`]s per TV name.
#[derive(Clone, Default)]
pub struct FakeTransportFactory {
    slots: Arc<Mutex<HashMap<String, FakeSlot>>>,
}

struct FakeSlot {
    receiver: Option<FakeReceiver>,
    occupancy: Occupancy,
    unreachable: bool,
    /// While set, `open` waits for one notification before answering.
    gate: Option<Arc<Notify>>,
}

impl FakeTransportFactory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a TV and get the handle that drives and inspects its emulator.
    /// Registering the same name again replaces its receiver.
    pub fn add_tv(&self, name: &str) -> FakeReceiverHandle {
        let (receiver, handle) = FakeReceiver::new(FakeReceiverConfig::default());
        self.slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                name.to_owned(),
                FakeSlot {
                    receiver: Some(receiver),
                    occupancy: Occupancy::Idle,
                    unreachable: false,
                    gate: None,
                },
            );
        handle
    }

    pub fn set_occupancy(&self, name: &str, occupancy: Occupancy) {
        if let Some(s) = self
            .slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(name)
        {
            s.occupancy = occupancy;
        }
    }

    /// Make `open` for this TV hang until the returned handle is notified
    /// (`notify_one`), to stand in for a TV that is slow to answer.
    pub fn hold_open(&self, name: &str) -> Arc<Notify> {
        let gate = Arc::new(Notify::new());
        if let Some(s) = self
            .slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(name)
        {
            s.gate = Some(Arc::clone(&gate));
        }
        gate
    }

    pub fn set_unreachable(&self, name: &str, unreachable: bool) {
        if let Some(s) = self
            .slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(name)
        {
            s.unreachable = unreachable;
        }
    }
}

impl TransportFactory for FakeTransportFactory {
    type Transport = FakeReceiver;

    async fn open(&self, tv: &ConfiguredTv) -> Result<(FakeReceiver, Occupancy), CastError> {
        let gate = self
            .slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&tv.name)
            .and_then(|s| s.gate.clone());
        if let Some(gate) = gate {
            gate.notified().await;
        }
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        let refused = |why: &str| CastError::Connect {
            addr: tv.address.to_string(),
            source: std::io::Error::new(std::io::ErrorKind::ConnectionRefused, why.to_owned()),
        };
        let slot = slots
            .get_mut(&tv.name)
            .ok_or_else(|| refused("no such fake TV"))?;
        if slot.unreachable {
            return Err(refused("fake TV marked unreachable"));
        }
        let receiver = slot
            .receiver
            .take()
            .ok_or_else(|| refused("fake receiver already handed out"))?;
        Ok((receiver, slot.occupancy.clone()))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn tv(name: &str, addr: std::net::SocketAddr) -> ConfiguredTv {
        ConfiguredTv {
            name: name.into(),
            address: CastAddress {
                host: addr.ip().to_string(),
                port: addr.port(),
            },
            allowed_users: Vec::new(),
        }
    }

    async fn closed_port() -> std::net::SocketAddr {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    }

    #[tokio::test]
    async fn probe_tells_a_listening_port_from_a_closed_one() {
        let open = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let open_addr = open.local_addr().unwrap();
        assert!(probe(&tv("a", open_addr).address).await);
        let gone = closed_port().await;
        assert!(!probe(&tv("b", gone).address).await);
    }

    #[tokio::test]
    async fn the_prober_publishes_who_answered() {
        let open = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tvs = [
            tv("Up", open.local_addr().unwrap()),
            tv("Down", closed_port().await),
        ];
        let mut rx = spawn_reachability_prober(&tvs);
        tokio::time::timeout(Duration::from_secs(5), rx.changed())
            .await
            .expect("a first probe result")
            .unwrap();
        let seen = rx.borrow().clone();
        assert_eq!(seen.get("Up"), Some(&true), "{seen:?}");
        assert_eq!(seen.get("Down"), Some(&false), "{seen:?}");
    }

    #[tokio::test]
    async fn with_no_tvs_the_runtime_is_inert() {
        let runtime = CastRuntime::start(&[]);
        assert!(runtime.service().is_none());
        assert!(runtime.reachable_tvs().is_empty());
    }

    #[tokio::test]
    async fn the_runtime_exposes_reachability_and_the_installed_service() {
        let open = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let runtime = CastRuntime::start(&[tv("Up", open.local_addr().unwrap())]);
        for _ in 0..250 {
            if runtime.reachable_tvs().get("Up") == Some(&true) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(runtime.reachable_tvs().get("Up"), Some(&true));
        assert!(runtime.service().is_none(), "no service until installed");
        let (tx, _rx) = mpsc::channel(1);
        runtime.install(CastServiceHandle { tx });
        assert!(runtime.service().is_some());
    }
}
