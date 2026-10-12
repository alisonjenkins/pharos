//! Server side of the cast hand-off (spec 009): an actor that owns every active
//! cast seat, connects the TV, joins it to the owner's group and ends it again.
//!
//! One task owns all state; callers talk to it through [`CastServiceHandle`].
//! Each cast also has three small owned tasks (group messages to the driver,
//! driver events to the actor, group-closed watcher) that never touch state.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
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
use pharos_sync::group::{CastBinding, GroupHandle, GroupMsg, MemberKind};
use pharos_sync::hub::member_id_for_device;
use pharos_sync::messages::{GroupId, MemberId, ServerMsg};
use pharos_sync::{GroupRegistry, MemberSinks, SessionHub};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::cast::{mint_tv_token, revoke_tv_token, DEFAULT_TV_TOKEN_LIFETIME};
use crate::cast_adapter::{command_for, intent_for, playing_entry, ClockPair, GroupCommandRequest};
use crate::config::{CastConfig, ConfiguredTv};
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
}

impl CastRejectReason {
    pub const ALL: [CastRejectReason; 7] = [
        Self::NotInGroup,
        Self::TvBusy,
        Self::TvUnreachable,
        Self::UserHasSeat,
        Self::NotPermitted,
        Self::TokenFailed,
        Self::Unavailable,
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
    let actor = Actor {
        deps,
        cfg,
        factory,
        casts: HashMap::new(),
        by_user: HashMap::new(),
        next_id: 0,
        internal_tx,
    };
    tokio::spawn(actor.run(rx, internal_rx));
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
    owner_key: String,
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

struct Actor<F: TransportFactory> {
    deps: CastDeps,
    cfg: CastConfig,
    factory: F,
    casts: HashMap<String, ActiveCast>,
    by_user: HashMap<UserId, String>,
    next_id: u64,
    internal_tx: mpsc::Sender<Internal>,
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
    }
}

impl<F: TransportFactory> Actor<F> {
    async fn run(
        mut self,
        mut commands: mpsc::Receiver<Command>,
        mut internal: mpsc::Receiver<Internal>,
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
                _ = ping.tick() => self.ping_seats(),
            }
        }
        let users: Vec<UserId> = self.by_user.keys().copied().collect();
        for user in users {
            self.end(user, CastReason::UserStop, EndedBy::Service).await;
        }
    }

    async fn on_command(&mut self, cmd: Command) {
        match cmd {
            Command::Start { req, reply } => {
                let _ = reply.send(self.start(req).await);
            }
            Command::Stop { user, reason, done } => {
                self.end(user, reason, EndedBy::Service).await;
                let _ = done.send(());
            }
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

    async fn start(&mut self, req: StartCast) -> Result<(), CastReject> {
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

        if let Some(existing) = self.by_user.get(&req.user_id) {
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
        if self.casts.contains_key(&tv.name) {
            return Err(self.reject(
                &req,
                &group_label,
                CastRejectReason::TvBusy,
                &tv.name,
                format!("{:?} already has a cast seat", tv.name),
            ));
        }

        let opened = tokio::time::timeout(OPEN_TIMEOUT, self.factory.open(&tv)).await;
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
                receiver_name: tv.name.clone(),
                access_token: AccessToken::new(token.secret().expose()),
                latency_estimate: LATENCY_ESTIMATE,
                max_load_wait: MAX_LOAD_WAIT,
            },
            events_tx,
        ));

        self.next_id = self.next_id.wrapping_add(1);
        let id = self.next_id;
        let seat_member =
            member_id_for_device(&format!("{}:cast:{}", req.user_id.0.simple(), tv.name));
        let seat_rx =
            self.deps
                .sinks
                .attach_channel(seat_member, SEAT_SINK_GEN, SEAT_SINK_CAPACITY);
        let forwarder = tokio::spawn(forward(
            seat_rx,
            Arc::clone(&driver),
            group.epoch_unix_ms,
            tv.name.clone(),
        ));
        let pump = tokio::spawn(pump_events(
            events_rx,
            self.internal_tx.clone(),
            tv.name.clone(),
            id,
        ));
        let watcher = tokio::spawn(watch_group(
            group.tx.clone(),
            self.internal_tx.clone(),
            tv.name.clone(),
            id,
        ));
        let mut cast = ActiveCast {
            id,
            tv: tv.name.clone(),
            owner: req.user_id,
            owner_name: req.user_name.clone(),
            owner_key: req.owner_sync_key.clone(),
            sender_device: device_of(&req.owner_sync_key).to_owned(),
            group: group.clone(),
            owner_member,
            seat_member,
            token_id: token.id.clone(),
            driver,
            state: ConnState::Connecting,
            forwarder,
            pump,
            watcher,
        };

        let (join_tx, join_rx) = oneshot::channel();
        let joined = group
            .tx
            .send(GroupMsg::AddMember {
                member_id: seat_member,
                name: req.user_name.clone(),
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
            self.teardown(&mut cast, EndedBy::Service).await;
            return Err(CastReject {
                reason: CastRejectReason::Unavailable,
                message,
            });
        }

        let binding = CastBinding {
            tv: tv.name.clone(),
            owner: req.user_id.0.simple().to_string(),
        };
        for (member, kind) in [
            (owner_member, MemberKind::Controller),
            (seat_member, MemberKind::CastSeat),
        ] {
            let _ = group
                .tx
                .send(GroupMsg::SetMemberKind {
                    member,
                    kind,
                    cast: Some(binding.clone()),
                })
                .await;
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
        let response = crate::api::jellyfin::syncplay::dispatch(
            &self.deps.hub,
            &self.deps.registry,
            &self.deps.stores,
            Some(cast.owner_key.clone()),
            label,
            move |sender| match command {
                GroupCommandRequest::Pause => GroupMsg::PauseShared { sender },
                GroupCommandRequest::Unpause => GroupMsg::Unpause { sender },
                GroupCommandRequest::Seek { position_ms } => GroupMsg::SeekTo {
                    sender,
                    position_ms,
                },
                GroupCommandRequest::StopCast => GroupMsg::MemberPing { member_id: sender },
            },
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
            return;
        };
        let Some(mut cast) = self.casts.remove(&tv) else {
            return;
        };
        self.teardown(&mut cast, by).await;
        // The driver records its own `Ended`; and a Stop it is sent is recorded
        // by it as `user_stop` whatever the real cause, so record the real one.
        if by == EndedBy::Service && reason != CastReason::UserStop {
            obs::record_cast_event(
                CastEvent::Ended,
                reason,
                &cast.owner_name,
                &cast.sender_device,
                &cast.tv,
                &cast.group.group_id.to_string(),
                &format!("seat {} removed", cast.seat_member),
            );
        }
        tracing::info!(
            tv = %cast.tv, owner = %cast.owner_name, group = %cast.group.group_id,
            reason = reason.label(), "cast ended"
        );
    }

    /// Release everything a cast holds: tasks, driver, sink, group seat, token.
    async fn teardown(&self, cast: &mut ActiveCast, by: EndedBy) {
        cast.forwarder.abort();
        cast.pump.abort();
        cast.watcher.abort();
        let _ = (&mut cast.forwarder).await;

        if by == EndedBy::Service {
            let _ = cast.driver.send(GroupIntent::Stop).await;
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

/// Test support: hands out pre-built [`FakeReceiver`]s per TV name.
#[derive(Clone, Default)]
pub struct FakeTransportFactory {
    slots: Arc<Mutex<HashMap<String, FakeSlot>>>,
}

struct FakeSlot {
    receiver: Option<FakeReceiver>,
    occupancy: Occupancy,
    unreachable: bool,
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
