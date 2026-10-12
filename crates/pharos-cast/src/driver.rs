//! Actor that drives ONE cast TV from group intents. It owns every piece of
//! state; nothing is shared and nothing is locked.

use std::collections::VecDeque;
use std::future::pending;
use std::time::Duration;

use thiserror::Error;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::drift::{self, Action, Observed, PlannerConfig, PlannerState, Target};
use crate::events::{CastEvent, CastReason};
use crate::messages::{
    AccessToken, CommandBody, PlayItem, PlayNowOptions, ReceiverCommand, ReceiverReport,
    SeekOptions,
};
use crate::obs::{self, ConnState, CorrectionKind};
use crate::transport::{CastError, CastTransport, TransportEvent};

/// What the group wants the TV to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupIntent {
    Load {
        item_id: String,
        item_type: String,
        position_ms: i64,
        playing: bool,
    },
    Play {
        position_ms: i64,
        apply_at: Instant,
    },
    Pause {
        position_ms: i64,
        apply_at: Instant,
    },
    Seek {
        position_ms: i64,
        playing: bool,
        apply_at: Instant,
    },
    Stop,
}

/// What the server must hear.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverEvent {
    State(ConnState),
    UserPaused { position_ms: i64 },
    UserResumed { position_ms: i64 },
    UserSeeked { position_ms: i64 },
    UserStopped,
    Ended { reason: CastReason },
}

#[derive(Debug, Clone)]
pub struct DriverConfig {
    pub planner: PlannerConfig,
    pub reconnect_grace: Duration,
    pub server_url: String,
    pub receiver_name: String,
    pub access_token: AccessToken,
    pub latency_estimate: Duration,
    pub max_load_wait: Duration,
}

#[derive(Debug, Error)]
#[error("cast driver for the TV has ended; dropped intent {0:?}")]
pub struct DriverGone(pub GroupIntent);

#[derive(Debug)]
pub struct CastDriverHandle {
    intents: mpsc::Sender<GroupIntent>,
    task: JoinHandle<()>,
}

impl CastDriverHandle {
    pub async fn send(&self, intent: GroupIntent) -> Result<(), DriverGone> {
        self.intents.send(intent).await.map_err(|e| DriverGone(e.0))
    }

    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    pub fn abort(&self) {
        self.task.abort();
    }

    pub async fn join(self) -> Result<(), tokio::task::JoinError> {
        drop(self.intents);
        self.task.await
    }
}

const INTENT_QUEUE: usize = 32;
const RECEIVER_APP_ID: &str = "F007D354";
const TICKS_PER_MS: i64 = 10_000;
/// Receivers echo our own command back as reports for ~0.5 s and a progress
/// report can predate the command's effect, so the planner stays quiet this long.
const SETTLE: Duration = Duration::from_secs(2);
const RECONNECT_BACKOFF_START: Duration = Duration::from_millis(250);
/// Short enough that a TV coming back is noticed well inside the default 20 s grace.
const RECONNECT_BACKOFF_CAP: Duration = Duration::from_secs(4);

pub fn spawn<T: CastTransport + 'static>(
    transport: T,
    cfg: DriverConfig,
    events: mpsc::Sender<DriverEvent>,
) -> CastDriverHandle {
    let (intents, rx) = mpsc::channel(INTENT_QUEUE);
    let driver = Driver {
        transport,
        cfg,
        events,
        intents: rx,
        epoch: Instant::now(),
        state: ConnState::Connecting,
        clock: None,
        loaded: None,
        last_report: None,
        planner: PlannerState::new(),
        last_command_at: None,
        expected_paused: false,
        held: VecDeque::new(),
        load_deadline: None,
        sent: Vec::new(),
        user_pending_until: None,
        loss_since: None,
        last_cause: None,
    };
    let task = tokio::spawn(driver.run());
    CastDriverHandle { intents, task }
}

fn millis(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

/// The group's position, anchored at the instant it was true.
#[derive(Debug, Clone, Copy)]
struct GroupClock {
    position_ms: i64,
    at: Instant,
    playing: bool,
}

impl GroupClock {
    fn target_at(&self, now: Instant) -> Target {
        let elapsed = if self.playing {
            millis(now.saturating_duration_since(self.at))
        } else {
            0
        };
        Target {
            position_ms: self.position_ms.saturating_add(elapsed),
            playing: self.playing,
        }
    }
}

#[derive(Debug, Clone)]
struct LoadedItem {
    item_id: String,
    item_type: String,
}

/// The last receiver report: `position_ms` was true at `at`.
#[derive(Debug, Clone, Copy)]
struct Snapshot {
    position_ms: i64,
    paused: bool,
    at: Instant,
}

impl Snapshot {
    fn position_at(&self, now: Instant) -> i64 {
        if self.paused {
            self.position_ms
        } else {
            self.position_ms
                .saturating_add(millis(now.saturating_duration_since(self.at)))
        }
    }
}

/// A timed group intent waiting for `send_at` (its apply time less the cast latency).
#[derive(Debug, Clone, Copy)]
struct Held {
    send_at: Instant,
    apply_at: Instant,
    position_ms: i64,
    playing: bool,
    force_seek: bool,
}

async fn sleep_until_opt(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => pending().await,
    }
}

/// What a command we sent should make the receiver report back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Effect {
    Load,
    Stop,
    Pause,
    Unpause,
    Seek { to_ms: i64 },
}

impl Effect {
    fn of(body: &CommandBody) -> Option<Self> {
        match body {
            CommandBody::PlayNow(_) => Some(Self::Load),
            CommandBody::Stop => Some(Self::Stop),
            CommandBody::Pause => Some(Self::Pause),
            CommandBody::Unpause => Some(Self::Unpause),
            CommandBody::Seek(o) => Some(Self::Seek {
                to_ms: (o.position * 1000.0).round() as i64,
            }),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Sent {
    at: Instant,
    effect: Effect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Exit {
    Ended(CastReason),
    Failed(CastReason),
}

/// Why a command sequence stopped early.
#[derive(Debug, Clone, Copy)]
enum Halt {
    Exit(Exit),
    /// The connection was lost and re-established; the reload carries the group state.
    Recovered,
}

fn settle(result: Result<(), Halt>) -> Option<Exit> {
    match result {
        Ok(()) | Err(Halt::Recovered) => None,
        Err(Halt::Exit(exit)) => Some(exit),
    }
}

struct Driver<T> {
    transport: T,
    cfg: DriverConfig,
    events: mpsc::Sender<DriverEvent>,
    intents: mpsc::Receiver<GroupIntent>,
    epoch: Instant,
    state: ConnState,
    clock: Option<GroupClock>,
    loaded: Option<LoadedItem>,
    last_report: Option<Snapshot>,
    planner: PlannerState,
    last_command_at: Option<Instant>,
    expected_paused: bool,
    held: VecDeque<Held>,
    load_deadline: Option<Instant>,
    sent: Vec<Sent>,
    user_pending_until: Option<Instant>,
    loss_since: Option<Instant>,
    last_cause: Option<String>,
}

impl<T: CastTransport> Driver<T> {
    async fn run(mut self) {
        let exit = self.drive().await;
        self.finish(exit).await;
    }

    async fn drive(&mut self) -> Exit {
        self.enter(ConnState::Connecting).await;
        if let Err(e) = self.transport.connect().await {
            return self.failed("connect", &e);
        }
        if let Err(e) = self.transport.launch(RECEIVER_APP_ID).await {
            return self.failed("launch", &e);
        }
        loop {
            let due = self.next_due();
            let load_deadline = self.load_deadline;
            let exit = tokio::select! {
                intent = self.intents.recv() => match intent {
                    Some(intent) => self.on_intent(intent).await,
                    None => Some(Exit::Ended(CastReason::UserStop)),
                },
                event = self.transport.recv() => self.on_transport(event).await,
                () = sleep_until_opt(due) => self.flush_due(Instant::now()).await,
                () = sleep_until_opt(load_deadline) => Some(self.load_timed_out()),
            };
            if let Some(exit) = exit {
                return exit;
            }
        }
    }

    fn failed(&self, op: &str, cause: &CastError) -> Exit {
        self.record(
            CastEvent::Rejected,
            CastReason::TvUnreachable,
            &format!("{op}: {cause}"),
        );
        Exit::Failed(CastReason::TvUnreachable)
    }

    async fn finish(&mut self, exit: Exit) {
        self.transport.close().await;
        match exit {
            Exit::Ended(reason) => {
                let detail = self.last_cause.take().unwrap_or_default();
                self.enter(ConnState::Ended).await;
                self.record(CastEvent::Ended, reason, &detail);
                self.emit(DriverEvent::Ended { reason }).await;
            }
            Exit::Failed(reason) => {
                self.enter(ConnState::Failed).await;
                self.emit(DriverEvent::Ended { reason }).await;
            }
        }
    }

    fn record(&self, event: CastEvent, reason: CastReason, detail: &str) {
        obs::record_cast_event(event, reason, "", "", &self.cfg.receiver_name, "", detail);
    }

    async fn emit(&mut self, event: DriverEvent) {
        if let Err(e) = self.events.send(event).await {
            tracing::debug!(tv = %self.cfg.receiver_name, dropped = ?e.0, "cast driver event receiver is gone");
        }
    }

    async fn enter(&mut self, next: ConnState) {
        obs::set_connection(self.state, 0.0);
        obs::set_connection(next, 1.0);
        self.state = next;
        self.emit(DriverEvent::State(next)).await;
    }

    fn envelope(&self, body: CommandBody) -> ReceiverCommand {
        ReceiverCommand {
            body,
            server_address: self.cfg.server_url.clone(),
            access_token: self.cfg.access_token.clone(),
            receiver_name: Some(self.cfg.receiver_name.clone()),
            user_id: None,
            device_id: None,
            server_id: None,
            server_version: None,
            max_bitrate: None,
        }
    }

    async fn send(&mut self, body: CommandBody) -> Result<(), CastError> {
        let name = body.name();
        let effect = Effect::of(&body);
        let command = self.envelope(body);
        self.transport.send(&command).await?;
        let now = Instant::now();
        self.last_command_at = Some(now);
        self.sent
            .retain(|s| now.saturating_duration_since(s.at) <= SETTLE);
        if let Some(effect) = effect {
            self.sent.push(Sent { at: now, effect });
        }
        tracing::debug!(tv = %self.cfg.receiver_name, command = name, "cast command sent");
        Ok(())
    }

    /// Send a command; a transport failure runs the loss path and halts the caller's sequence.
    async fn send_cmd(&mut self, body: CommandBody) -> Result<(), Halt> {
        let name = body.name();
        match self.send(body).await {
            Ok(()) => Ok(()),
            Err(e) => self.loss_halt(&format!("sending {name}: {e}")).await,
        }
    }

    async fn loss_halt(&mut self, detail: &str) -> Result<(), Halt> {
        match self.on_loss(detail).await {
            Some(exit) => Err(Halt::Exit(exit)),
            None => Err(Halt::Recovered),
        }
    }

    fn load_timed_out(&mut self) -> Exit {
        self.load_deadline = None;
        let item = self.loaded.as_ref().map_or("none", |l| l.item_id.as_str());
        self.record(
            CastEvent::Lost,
            CastReason::TvUnreachable,
            &format!(
                "no report for item {item} within {:?} of PlayNow",
                self.cfg.max_load_wait
            ),
        );
        Exit::Failed(CastReason::TvUnreachable)
    }

    /// The connection is gone: retry with backoff until `reconnect_grace` runs out,
    /// then reload the item at the group's current position. `None` means recovered
    /// (the driver is Loading again); `Some` is the end of the cast.
    async fn on_loss(&mut self, detail: &str) -> Option<Exit> {
        let since = *self.loss_since.get_or_insert_with(Instant::now);
        self.record(CastEvent::Lost, CastReason::TvUnreachable, detail);
        tracing::warn!(tv = %self.cfg.receiver_name, detail, "cast connection lost");
        self.load_deadline = None;
        self.transport.close().await;
        self.enter(ConnState::Lost).await;

        let deadline = since + self.cfg.reconnect_grace;
        let mut backoff = RECONNECT_BACKOFF_START;
        let mut attempt = 0u32;
        loop {
            attempt = attempt.saturating_add(1);
            let cause = match tokio::time::timeout_at(deadline, self.reopen()).await {
                Ok(Ok(())) => {
                    tracing::info!(tv = %self.cfg.receiver_name, attempt, "cast reconnect attempt succeeded");
                    return None;
                }
                Ok(Err(e)) => e.to_string(),
                Err(_) => format!(
                    "attempt {attempt} still running when the {:?} grace ran out",
                    self.cfg.reconnect_grace
                ),
            };
            let now = Instant::now();
            tracing::info!(
                tv = %self.cfg.receiver_name,
                attempt,
                cause = %cause,
                backoff_ms = millis(backoff),
                "cast reconnect attempt failed"
            );
            self.last_cause = Some(cause);
            if now >= deadline {
                return Some(Exit::Ended(CastReason::GraceExpired));
            }
            let wake = (now + backoff).min(deadline);
            if let Some(exit) = self.wait_while_lost(wake).await {
                return Some(exit);
            }
            backoff = backoff.saturating_mul(2).min(RECONNECT_BACKOFF_CAP);
        }
    }

    /// Connect, relaunch and reload the current item at the group's current position.
    async fn reopen(&mut self) -> Result<(), CastError> {
        self.transport.connect().await?;
        self.transport.launch(RECEIVER_APP_ID).await?;
        let Some(item) = self.loaded.clone() else {
            self.enter(ConnState::Connecting).await;
            return Ok(());
        };
        let target = self
            .clock
            .map(|c| c.target_at(Instant::now()))
            .unwrap_or(Target {
                position_ms: 0,
                playing: true,
            });
        self.start_item(item, target.position_ms, target.playing)
            .await
    }

    async fn wait_while_lost(&mut self, wake: Instant) -> Option<Exit> {
        loop {
            tokio::select! {
                () = tokio::time::sleep_until(wake) => return None,
                intent = self.intents.recv() => match intent {
                    None => return Some(Exit::Ended(CastReason::UserStop)),
                    Some(intent) => {
                        if let Some(exit) = self.on_intent_while_lost(intent) {
                            return Some(exit);
                        }
                    }
                },
            }
        }
    }

    /// Nothing can be sent while lost: remember what the group wants for the reload.
    fn on_intent_while_lost(&mut self, intent: GroupIntent) -> Option<Exit> {
        self.user_pending_until = None;
        match intent {
            GroupIntent::Stop => {
                self.held.clear();
                Some(Exit::Ended(CastReason::UserStop))
            }
            GroupIntent::Load {
                item_id,
                item_type,
                position_ms,
                playing,
            } => {
                self.held.clear();
                self.set_desired(LoadedItem { item_id, item_type }, position_ms, playing);
                None
            }
            GroupIntent::Play {
                position_ms,
                apply_at,
            } => {
                self.enqueue(position_ms, true, false, apply_at);
                None
            }
            GroupIntent::Pause {
                position_ms,
                apply_at,
            } => {
                self.enqueue(position_ms, false, false, apply_at);
                None
            }
            GroupIntent::Seek {
                position_ms,
                playing,
                apply_at,
            } => {
                self.enqueue(position_ms, playing, true, apply_at);
                None
            }
        }
    }

    async fn on_transport(&mut self, event: TransportEvent) -> Option<Exit> {
        match event {
            TransportEvent::Report(report) => self.on_report(report).await,
            TransportEvent::HeartbeatLost => self.on_loss("heartbeat lost").await,
            TransportEvent::Closed { reason } => self.on_loss(&format!("closed: {reason}")).await,
        }
    }

    async fn on_intent(&mut self, intent: GroupIntent) -> Option<Exit> {
        self.user_pending_until = None;
        match intent {
            GroupIntent::Load {
                item_id,
                item_type,
                position_ms,
                playing,
            } => {
                self.held.clear();
                settle(
                    self.load(LoadedItem { item_id, item_type }, position_ms, playing)
                        .await,
                )
            }
            GroupIntent::Stop => {
                self.held.clear();
                if let Err(e) = self.send(CommandBody::Stop).await {
                    tracing::warn!(tv = %self.cfg.receiver_name, error = %e, "cast Stop command failed while ending");
                }
                Some(Exit::Ended(CastReason::UserStop))
            }
            GroupIntent::Play {
                position_ms,
                apply_at,
            } => self.hold(position_ms, true, false, apply_at).await,
            GroupIntent::Pause {
                position_ms,
                apply_at,
            } => self.hold(position_ms, false, false, apply_at).await,
            GroupIntent::Seek {
                position_ms,
                playing,
                apply_at,
            } => self.hold(position_ms, playing, true, apply_at).await,
        }
    }

    /// Queue a timed intent in apply order and send whatever is already due.
    async fn hold(
        &mut self,
        position_ms: i64,
        playing: bool,
        force_seek: bool,
        apply_at: Instant,
    ) -> Option<Exit> {
        self.enqueue(position_ms, playing, force_seek, apply_at);
        self.flush_due(Instant::now()).await
    }

    fn enqueue(&mut self, position_ms: i64, playing: bool, force_seek: bool, apply_at: Instant) {
        let send_at = apply_at
            .checked_sub(self.cfg.latency_estimate)
            .unwrap_or(apply_at);
        let idx = self
            .held
            .iter()
            .position(|h| h.send_at > send_at)
            .unwrap_or(self.held.len());
        self.held.insert(
            idx,
            Held {
                send_at,
                apply_at,
                position_ms,
                playing,
                force_seek,
            },
        );
    }

    /// Timed intents wait for the receiver to be Following: commands sent
    /// mid-load would land on the previous item.
    fn next_due(&self) -> Option<Instant> {
        if self.state != ConnState::Following {
            return None;
        }
        self.held.front().map(|h| h.send_at)
    }

    async fn flush_due(&mut self, now: Instant) -> Option<Exit> {
        while self.next_due().is_some_and(|at| at <= now) {
            let h = self.held.pop_front()?;
            let applied = self
                .apply(h.position_ms, h.playing, h.force_seek, h.apply_at)
                .await;
            if let Err(halt) = applied {
                return settle(Err(halt));
            }
        }
        None
    }

    /// Record what the group wants on the TV, before anything is sent.
    fn set_desired(&mut self, item: LoadedItem, position_ms: i64, playing: bool) {
        self.clock = Some(GroupClock {
            position_ms,
            at: Instant::now(),
            playing,
        });
        self.loaded = Some(item);
        self.last_report = None;
        self.planner = PlannerState::new();
        self.expected_paused = false;
    }

    async fn start_item(
        &mut self,
        item: LoadedItem,
        position_ms: i64,
        playing: bool,
    ) -> Result<(), CastError> {
        let body = CommandBody::PlayNow(PlayNowOptions {
            items: vec![PlayItem {
                id: item.item_id.clone(),
                kind: item.item_type.clone(),
            }],
            start_index: 0,
            start_position_ticks: position_ms.saturating_mul(TICKS_PER_MS),
            media_source_id: None,
            audio_stream_index: None,
            subtitle_stream_index: None,
        });
        self.set_desired(item, position_ms, playing);
        self.send(body).await?;
        self.load_deadline = Some(Instant::now() + self.cfg.max_load_wait);
        self.enter(ConnState::Loading).await;
        Ok(())
    }

    async fn load(
        &mut self,
        item: LoadedItem,
        position_ms: i64,
        playing: bool,
    ) -> Result<(), Halt> {
        match self.start_item(item, position_ms, playing).await {
            Ok(()) => Ok(()),
            Err(e) => self.loss_halt(&format!("sending PlayNow: {e}")).await,
        }
    }

    /// Bring the receiver to the group's new position and play state.
    async fn apply(
        &mut self,
        position_ms: i64,
        playing: bool,
        force_seek: bool,
        at: Instant,
    ) -> Result<(), Halt> {
        if self.loaded.is_none() {
            tracing::debug!(tv = %self.cfg.receiver_name, position_ms, "cast intent before any Load ignored");
            return Ok(());
        }
        let threshold = i64::try_from(self.cfg.planner.threshold_ms).unwrap_or(i64::MAX);
        let off = match &self.last_report {
            Some(s) => s.position_at(at).saturating_sub(position_ms).abs() > threshold,
            None => true,
        };
        self.clock = Some(GroupClock {
            position_ms,
            at,
            playing,
        });
        if playing && self.expected_paused {
            self.expected_paused = false;
            self.send_cmd(CommandBody::Unpause).await?;
        }
        if !playing && !self.expected_paused {
            self.expected_paused = true;
            self.send_cmd(CommandBody::Pause).await?;
        }
        if force_seek || off {
            let body = CommandBody::Seek(SeekOptions {
                position: position_ms as f64 / 1000.0,
            });
            self.send_cmd(body).await?;
        }
        Ok(())
    }

    async fn on_report(&mut self, report: ReceiverReport) -> Option<Exit> {
        let now = Instant::now();
        if let ReceiverReport::Error(e)
        | ReceiverReport::PlaybackError(e)
        | ReceiverReport::ConnectionError(e) = &report
        {
            tracing::warn!(tv = %self.cfg.receiver_name, message = ?e.message, "cast receiver reported an error");
            return None;
        }
        let ps = report.play_state()?;
        let loaded = self.loaded.as_ref()?;
        if let Some(id) = ps.item_id.as_deref() {
            if id != loaded.item_id {
                tracing::debug!(tv = %self.cfg.receiver_name, reported = id, loaded = %loaded.item_id, "cast report for another item ignored");
                return None;
            }
        }
        if matches!(report, ReceiverReport::PlaybackStop(_)) {
            if self.state == ConnState::Following {
                if self.echoes(now, |e| matches!(e, Effect::Stop)) {
                    tracing::debug!(tv = %self.cfg.receiver_name, "cast playbackstop is the echo of our own command");
                } else {
                    self.user_acted(now, "stop", None);
                    self.emit(DriverEvent::UserStopped).await;
                }
            }
            return None;
        }
        let Some(ticks) = ps.position_ticks else {
            tracing::debug!(tv = %self.cfg.receiver_name, "cast report without a position ignored");
            return None;
        };
        let snapshot = Snapshot {
            position_ms: ticks / TICKS_PER_MS,
            paused: ps.is_paused.unwrap_or(false),
            at: now,
        };
        let previous = self.last_report.replace(snapshot);
        if self.state == ConnState::Following {
            if let Some(previous) = previous {
                let changed = matches!(report, ReceiverReport::PlayStateChange(_));
                self.classify(now, previous, snapshot, changed).await;
            }
        }
        match self.state {
            ConnState::Loading => {
                self.load_deadline = None;
                self.enter(ConnState::Following).await;
                if let Some(since) = self.loss_since.take() {
                    self.record(
                        CastEvent::Reconnected,
                        CastReason::TvUnreachable,
                        &format!(
                            "following again {:?} after the connection was lost",
                            now.saturating_duration_since(since)
                        ),
                    );
                }
                let playing = self.clock.map_or(true, |c| c.playing);
                if !playing {
                    self.expected_paused = true;
                    if let Err(halt) = self.send_cmd(CommandBody::Pause).await {
                        return settle(Err(halt));
                    }
                }
                self.flush_due(now).await
            }
            ConnState::Following if matches!(report, ReceiverReport::PlaybackProgress(_)) => {
                settle(self.correct(now, snapshot).await)
            }
            _ => None,
        }
    }

    fn echoes(&self, now: Instant, matches: impl Fn(&Effect) -> bool) -> bool {
        self.sent
            .iter()
            .any(|s| now.saturating_duration_since(s.at) <= SETTLE && matches(&s.effect))
    }

    fn user_acted(&mut self, now: Instant, what: &str, position_ms: Option<i64>) {
        let cooldown = Duration::from_millis(self.cfg.planner.cooldown_ms);
        self.user_pending_until = Some(now + cooldown);
        tracing::info!(
            tv = %self.cfg.receiver_name,
            what,
            position_ms,
            hold_planner_ms = self.cfg.planner.cooldown_ms,
            "cast receiver-originated change, not caused by a command of ours"
        );
    }

    /// Tell a change the TV made itself from the echo of a command we sent.
    async fn classify(
        &mut self,
        now: Instant,
        previous: Snapshot,
        current: Snapshot,
        is_state_change: bool,
    ) {
        if previous.paused != current.paused {
            let paused = current.paused;
            let ours = self.echoes(now, |e| {
                matches!(
                    (e, paused),
                    (Effect::Pause, true) | (Effect::Unpause, false)
                )
            });
            if ours {
                tracing::debug!(tv = %self.cfg.receiver_name, paused, "cast play state change is the echo of our own command");
            } else {
                self.expected_paused = paused;
                self.user_acted(
                    now,
                    if paused { "pause" } else { "resume" },
                    Some(current.position_ms),
                );
                let position_ms = current.position_ms;
                self.emit(if paused {
                    DriverEvent::UserPaused { position_ms }
                } else {
                    DriverEvent::UserResumed { position_ms }
                })
                .await;
            }
        }
        if !is_state_change {
            return;
        }
        let threshold = i64::try_from(self.cfg.planner.threshold_ms).unwrap_or(i64::MAX);
        let jump = current
            .position_ms
            .saturating_sub(previous.position_at(now))
            .abs();
        if jump <= threshold {
            return;
        }
        let before = previous.position_at(now);
        // A seek lands with receiver error, so match its direction, not its landing point.
        let moved = current.position_ms.saturating_sub(before).signum();
        let ours = self.sent.iter().any(|s| match s.effect {
            Effect::Seek { to_ms } => {
                now.saturating_duration_since(s.at) <= SETTLE
                    && to_ms.saturating_sub(before).signum() == moved
            }
            _ => false,
        });
        if ours {
            tracing::debug!(tv = %self.cfg.receiver_name, jump_ms = jump, "cast position jump is the echo of our own seek");
        } else {
            self.user_acted(now, "seek", Some(current.position_ms));
            self.emit(DriverEvent::UserSeeked {
                position_ms: current.position_ms,
            })
            .await;
        }
    }

    async fn correct(&mut self, now: Instant, snapshot: Snapshot) -> Result<(), Halt> {
        let Some(clock) = self.clock else {
            return Ok(());
        };
        if self.user_pending_until.is_some_and(|until| now < until) {
            tracing::debug!(tv = %self.cfg.receiver_name, "cast planner held while a receiver-originated change awaits the group");
            return Ok(());
        }
        if let Some(sent) = self.last_command_at {
            if now.saturating_duration_since(sent) < SETTLE {
                return Ok(());
            }
        }
        let target = clock.target_at(now);
        let age_ms = u64::try_from(millis(now.saturating_duration_since(snapshot.at))).unwrap_or(0);
        let observed = Observed {
            position_ms: snapshot.position_ms,
            is_paused: snapshot.paused,
            age_ms,
        };
        let interpolated = snapshot.position_at(now);
        let drift_ms = interpolated.saturating_sub(target.position_ms);
        obs::record_drift(drift_ms.unsigned_abs() as f64 / 1000.0);
        let now_ms = u64::try_from(millis(now.saturating_duration_since(self.epoch))).unwrap_or(0);
        let action = drift::plan(
            &target,
            &observed,
            now_ms,
            &self.cfg.planner,
            &mut self.planner,
        );
        let (kind, body) = match action {
            Action::None => return Ok(()),
            Action::Seek { to_ms } => (
                CorrectionKind::Seek,
                CommandBody::Seek(SeekOptions {
                    position: to_ms as f64 / 1000.0,
                }),
            ),
            Action::Pause => (CorrectionKind::Pause, CommandBody::Pause),
            Action::Resume => (CorrectionKind::Resume, CommandBody::Unpause),
        };
        tracing::info!(
            tv = %self.cfg.receiver_name,
            action = kind.label(),
            drift_ms,
            target_ms = target.position_ms,
            target_playing = target.playing,
            observed_ms = interpolated,
            observed_paused = snapshot.paused,
            "cast drift correction"
        );
        obs::record_correction(kind);
        match kind {
            CorrectionKind::Pause => self.expected_paused = true,
            CorrectionKind::Resume => self.expected_paused = false,
            CorrectionKind::Seek => {}
        }
        self.send_cmd(body).await
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::fake::{FakeReceiver, FakeReceiverConfig, FakeReceiverHandle};
    use crate::messages::CommandBody;
    use metrics_util::debugging::{DebugValue, DebuggingRecorder, Snapshotter};

    const WAIT: Duration = Duration::from_secs(60);

    pub(super) fn driver_cfg() -> DriverConfig {
        DriverConfig {
            planner: PlannerConfig::default(),
            reconnect_grace: Duration::from_secs(20),
            server_url: "http://pharos:8096".into(),
            receiver_name: "Living Room TV".into(),
            access_token: AccessToken::new("tv-token"),
            latency_estimate: Duration::ZERO,
            max_load_wait: Duration::from_secs(10),
        }
    }

    pub(super) struct Rig {
        pub handle: CastDriverHandle,
        pub fake: FakeReceiverHandle,
        pub events: mpsc::Receiver<DriverEvent>,
    }

    pub(super) fn start(fake_cfg: FakeReceiverConfig, cfg: DriverConfig) -> Rig {
        let (rx, fake) = FakeReceiver::new(fake_cfg);
        let (tx, events) = mpsc::channel(64);
        let handle = spawn(rx, cfg, tx);
        Rig {
            handle,
            fake,
            events,
        }
    }

    impl Rig {
        pub async fn next(&mut self) -> DriverEvent {
            tokio::time::timeout(WAIT, self.events.recv())
                .await
                .expect("no driver event in time")
                .expect("driver event channel closed")
        }

        pub async fn until_state(&mut self, want: ConnState) -> Vec<DriverEvent> {
            let mut seen = Vec::new();
            loop {
                let ev = self.next().await;
                let done = ev == DriverEvent::State(want);
                seen.push(ev);
                if done {
                    return seen;
                }
            }
        }

        pub async fn load(&self, position_ms: i64, playing: bool) {
            self.handle
                .send(GroupIntent::Load {
                    item_id: "item-1".into(),
                    item_type: "Movie".into(),
                    position_ms,
                    playing,
                })
                .await
                .unwrap();
        }
    }

    type Series = Vec<(String, Vec<(String, String)>, DebugValue)>;

    fn series(snap: &Snapshotter) -> Series {
        snap.snapshot()
            .into_vec()
            .into_iter()
            .map(|(ck, _, _, v)| {
                let labels = ck
                    .key()
                    .labels()
                    .map(|l| (l.key().to_owned(), l.value().to_owned()))
                    .collect();
                (ck.key().name().to_owned(), labels, v)
            })
            .collect()
    }

    fn counter(all: &Series, name: &str, label: (&str, &str)) -> u64 {
        all.iter()
            .filter(|(n, labels, _)| {
                n == name && labels.iter().any(|(k, v)| k == label.0 && v == label.1)
            })
            .map(|(_, _, v)| match v {
                DebugValue::Counter(c) => *c,
                _ => 0,
            })
            .sum()
    }

    #[tokio::test(start_paused = true)]
    async fn connects_launches_loads_and_follows_with_the_tv_envelope() {
        let mut rig = start(FakeReceiverConfig::default(), driver_cfg());
        rig.load(5_000, true).await;
        let states = rig.until_state(ConnState::Following).await;
        assert_eq!(
            states,
            [
                DriverEvent::State(ConnState::Connecting),
                DriverEvent::State(ConnState::Loading),
                DriverEvent::State(ConnState::Following),
            ]
        );
        let cmds = rig.fake.commands();
        assert_eq!(cmds.len(), 1, "{cmds:?}");
        let first = cmds.first().unwrap();
        assert_eq!(first.server_address, "http://pharos:8096");
        assert_eq!(first.access_token.expose(), "tv-token");
        assert_eq!(first.receiver_name.as_deref(), Some("Living Room TV"));
        match &first.body {
            CommandBody::PlayNow(o) => {
                assert_eq!(o.items.len(), 1);
                assert_eq!(o.items.first().unwrap().id, "item-1");
                assert_eq!(o.items.first().unwrap().kind, "Movie");
                assert_eq!(o.start_index, 0);
                assert_eq!(o.start_position_ticks, 50_000_000);
            }
            other => panic!("expected PlayNow, got {other:?}"),
        }
        assert_eq!(rig.fake.state().item_id.as_deref(), Some("item-1"));
    }

    #[tokio::test(start_paused = true)]
    async fn group_seek_is_applied_then_injected_drift_is_corrected_and_recorded() {
        let recorder = DebuggingRecorder::new();
        let snap = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let fake_cfg = FakeReceiverConfig {
            drift_ms: 3_000,
            ..Default::default()
        };
        let mut rig = start(fake_cfg, driver_cfg());
        rig.load(0, true).await;
        rig.until_state(ConnState::Following).await;

        rig.handle
            .send(GroupIntent::Seek {
                position_ms: 60_000,
                playing: true,
                apply_at: Instant::now(),
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(3_100)).await;

        let seeks: Vec<f64> = rig
            .fake
            .commands()
            .into_iter()
            .filter_map(|c| match c.body {
                CommandBody::Seek(o) => Some(o.position),
                _ => None,
            })
            .collect();
        assert_eq!(
            seeks.len(),
            2,
            "group seek + one drift correction: {seeks:?}"
        );
        assert!((seeks.first().unwrap() - 60.0).abs() < 0.01, "{seeks:?}");
        assert!((seeks.get(1).unwrap() - 63.0).abs() < 0.01, "{seeks:?}");
        let all = series(&snap);
        assert_eq!(
            counter(&all, "pharos_cast_correction_total", ("kind", "seek")),
            1
        );
        let drift_samples: usize = all
            .iter()
            .filter(|(n, _, _)| n == "pharos_cast_drift_seconds")
            .map(|(_, _, v)| match v {
                DebugValue::Histogram(h) => h.len(),
                _ => 0,
            })
            .sum();
        assert!(drift_samples >= 1, "{all:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_paused_group_load_pauses_the_tv_once_it_is_following() {
        let mut rig = start(FakeReceiverConfig::default(), driver_cfg());
        rig.load(7_000, false).await;
        rig.until_state(ConnState::Following).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let bodies: Vec<&'static str> = rig.fake.commands().iter().map(|c| c.body.name()).collect();
        assert_eq!(bodies, ["PlayNow", "Pause"]);
        assert!(rig.fake.state().paused);
    }

    #[tokio::test(start_paused = true)]
    async fn stop_intent_closes_the_transport_and_ends_with_user_stop() {
        let mut rig = start(FakeReceiverConfig::default(), driver_cfg());
        rig.load(0, true).await;
        rig.until_state(ConnState::Following).await;
        rig.handle.send(GroupIntent::Stop).await.unwrap();
        assert_eq!(rig.next().await, DriverEvent::State(ConnState::Ended));
        assert_eq!(
            rig.next().await,
            DriverEvent::Ended {
                reason: CastReason::UserStop
            }
        );
        assert!(rig
            .fake
            .commands()
            .iter()
            .any(|c| c.body == CommandBody::Stop));
        rig.handle.join().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_the_handle_ends_the_driver_orderly() {
        let mut rig = start(FakeReceiverConfig::default(), driver_cfg());
        rig.load(0, true).await;
        rig.until_state(ConnState::Following).await;
        let Rig {
            handle, mut events, ..
        } = rig;
        drop(handle);
        let mut last = None;
        while let Some(ev) = tokio::time::timeout(WAIT, events.recv()).await.unwrap() {
            last = Some(ev);
        }
        assert_eq!(
            last,
            Some(DriverEvent::Ended {
                reason: CastReason::UserStop
            })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_unreachable_tv_fails_with_the_cause_and_ends() {
        let fake_cfg = FakeReceiverConfig {
            refuse_connect: true,
            ..Default::default()
        };
        let mut rig = start(fake_cfg, driver_cfg());
        rig.load(0, true).await;
        let states = rig.until_state(ConnState::Failed).await;
        assert_eq!(
            states.first(),
            Some(&DriverEvent::State(ConnState::Connecting))
        );
        assert_eq!(
            rig.next().await,
            DriverEvent::Ended {
                reason: CastReason::TvUnreachable
            }
        );
    }

    fn names(rig: &Rig) -> Vec<&'static str> {
        rig.fake.commands().iter().map(|c| c.body.name()).collect()
    }

    #[tokio::test(start_paused = true)]
    async fn a_future_intent_is_held_and_lands_on_the_receiver_at_its_apply_time() {
        let fake_cfg = FakeReceiverConfig {
            command_latency_ms: 200,
            ..Default::default()
        };
        let cfg = DriverConfig {
            latency_estimate: Duration::from_millis(200),
            ..driver_cfg()
        };
        let mut rig = start(fake_cfg, cfg);
        rig.load(0, true).await;
        rig.until_state(ConnState::Following).await;
        let apply_at = Instant::now() + Duration::from_secs(5);
        rig.handle
            .send(GroupIntent::Pause {
                position_ms: 5_000,
                apply_at,
            })
            .await
            .unwrap();

        tokio::time::sleep_until(apply_at - Duration::from_millis(250)).await;
        assert_eq!(names(&rig), ["PlayNow"], "held until apply_at - latency");
        assert!(!rig.fake.state().paused);

        tokio::time::sleep_until(apply_at - Duration::from_millis(50)).await;
        assert_eq!(
            names(&rig),
            ["PlayNow", "Pause"],
            "sent early by the latency"
        );
        assert!(
            !rig.fake.state().paused,
            "receiver applies only at apply_at"
        );

        tokio::time::sleep_until(apply_at + Duration::from_millis(10)).await;
        assert!(rig.fake.state().paused);
    }

    #[tokio::test(start_paused = true)]
    async fn an_intent_already_past_its_apply_time_is_sent_immediately() {
        let mut rig = start(FakeReceiverConfig::default(), driver_cfg());
        rig.load(0, true).await;
        rig.until_state(ConnState::Following).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        let sent_at = Instant::now();
        rig.handle
            .send(GroupIntent::Pause {
                position_ms: 900,
                apply_at: sent_at - Duration::from_millis(100),
            })
            .await
            .unwrap();
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(names(&rig), ["PlayNow", "Pause"]);
        assert_eq!(Instant::now(), sent_at, "no waiting for a past apply_at");
    }

    #[tokio::test(start_paused = true)]
    async fn held_intents_are_sent_in_apply_order_not_arrival_order() {
        let mut rig = start(FakeReceiverConfig::default(), driver_cfg());
        rig.load(0, true).await;
        rig.until_state(ConnState::Following).await;
        let t0 = Instant::now();
        rig.handle
            .send(GroupIntent::Seek {
                position_ms: 30_000,
                playing: true,
                apply_at: t0 + Duration::from_secs(2),
            })
            .await
            .unwrap();
        rig.handle
            .send(GroupIntent::Pause {
                position_ms: 1_000,
                apply_at: t0 + Duration::from_secs(1),
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(2_100)).await;
        assert_eq!(names(&rig), ["PlayNow", "Pause", "Unpause", "Seek"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_item_while_following_reloads_and_follows_the_new_item() {
        let mut rig = start(FakeReceiverConfig::default(), driver_cfg());
        rig.load(0, true).await;
        rig.until_state(ConnState::Following).await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        rig.handle
            .send(GroupIntent::Load {
                item_id: "item-2".into(),
                item_type: "Episode".into(),
                position_ms: 42_000,
                playing: true,
            })
            .await
            .unwrap();
        assert_eq!(
            rig.until_state(ConnState::Following).await,
            [
                DriverEvent::State(ConnState::Loading),
                DriverEvent::State(ConnState::Following)
            ]
        );
        let cmds = rig.fake.commands();
        assert_eq!(cmds.len(), 2, "{cmds:?}");
        match &cmds.get(1).unwrap().body {
            CommandBody::PlayNow(o) => {
                assert_eq!(o.items.first().unwrap().id, "item-2");
                assert_eq!(o.items.first().unwrap().kind, "Episode");
                assert_eq!(o.start_position_ticks, 420_000_000);
            }
            other => panic!("expected PlayNow, got {other:?}"),
        }
        assert_eq!(rig.fake.state().item_id.as_deref(), Some("item-2"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_load_within_the_bound_still_reaches_following() {
        let fake_cfg = FakeReceiverConfig {
            command_latency_ms: 9_000,
            ..Default::default()
        };
        let mut rig = start(fake_cfg, driver_cfg());
        let t0 = Instant::now();
        rig.load(0, true).await;
        rig.until_state(ConnState::Following).await;
        assert_eq!(t0.elapsed(), Duration::from_secs(9));
    }

    #[tokio::test(start_paused = true)]
    async fn no_report_within_max_load_wait_ends_the_cast_and_records_why() {
        let recorder = DebuggingRecorder::new();
        let snap = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let fake_cfg = FakeReceiverConfig {
            command_latency_ms: 60_000,
            ..Default::default()
        };
        let mut rig = start(fake_cfg, driver_cfg());
        let t0 = Instant::now();
        rig.load(0, true).await;
        let seen = rig.until_state(ConnState::Failed).await;
        assert!(
            seen.contains(&DriverEvent::State(ConnState::Loading)),
            "{seen:?}"
        );
        assert_eq!(
            rig.next().await,
            DriverEvent::Ended {
                reason: CastReason::TvUnreachable
            }
        );
        assert_eq!(t0.elapsed(), Duration::from_secs(10));
        let all = series(&snap);
        assert_eq!(
            counter(&all, "pharos_cast_seat_total", ("event", "lost")),
            1
        );
        assert_eq!(
            counter(&all, "pharos_cast_seat_total", ("reason", "tv_unreachable")),
            1
        );
    }

    fn user_events(events: &mut mpsc::Receiver<DriverEvent>) -> Vec<DriverEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = events.try_recv() {
            if !matches!(ev, DriverEvent::State(_)) {
                out.push(ev);
            }
        }
        out
    }

    async fn following_rig() -> Rig {
        let mut rig = start(FakeReceiverConfig::default(), driver_cfg());
        rig.load(0, true).await;
        rig.until_state(ConnState::Following).await;
        rig
    }

    #[tokio::test(start_paused = true)]
    async fn a_pause_on_the_tv_remote_is_reported_and_not_undone() {
        let mut rig = following_rig().await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        rig.fake.user_pause();
        assert_eq!(
            rig.next().await,
            DriverEvent::UserPaused { position_ms: 2_000 }
        );
        tokio::time::sleep(Duration::from_secs(4)).await;
        assert_eq!(
            names(&rig),
            ["PlayNow"],
            "no Unpause while the group decides"
        );
        assert!(rig.fake.state().paused);
    }

    #[tokio::test(start_paused = true)]
    async fn a_resume_on_the_tv_remote_is_reported() {
        let mut rig = following_rig().await;
        rig.handle
            .send(GroupIntent::Pause {
                position_ms: 0,
                apply_at: Instant::now(),
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(
            user_events(&mut rig.events).is_empty(),
            "our own Pause is not the user"
        );
        rig.fake.user_unpause();
        assert_eq!(
            rig.next().await,
            DriverEvent::UserResumed { position_ms: 0 }
        );
        assert_eq!(names(&rig), ["PlayNow", "Pause"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_seek_on_the_tv_remote_is_reported_then_corrected_only_after_one_cooldown() {
        let mut rig = following_rig().await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        rig.fake.user_seek(Duration::from_secs(90));
        assert_eq!(
            rig.next().await,
            DriverEvent::UserSeeked {
                position_ms: 90_000
            }
        );
        tokio::time::sleep(Duration::from_secs(4)).await;
        assert_eq!(
            names(&rig),
            ["PlayNow"],
            "planner is quiet while the group answers"
        );

        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(
            names(&rig),
            ["PlayNow", "Seek"],
            "unanswered for a cooldown: the planner takes over again"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_group_answer_ends_the_suppression() {
        let mut rig = following_rig().await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        rig.fake.user_seek(Duration::from_secs(90));
        assert!(matches!(rig.next().await, DriverEvent::UserSeeked { .. }));
        rig.handle
            .send(GroupIntent::Seek {
                position_ms: 3_000,
                playing: true,
                apply_at: Instant::now(),
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(names(&rig), ["PlayNow", "Seek"]);
        assert!(
            user_events(&mut rig.events).is_empty(),
            "{:?}",
            rig.fake.state()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_on_the_tv_remote_is_reported_without_ending_the_driver() {
        let mut rig = following_rig().await;
        rig.fake.user_stop();
        assert_eq!(rig.next().await, DriverEvent::UserStopped);
        assert!(!rig.handle.is_finished());
    }

    #[tokio::test(start_paused = true)]
    async fn our_own_commands_and_their_echoes_are_never_reported_as_the_user() {
        let fake_cfg = FakeReceiverConfig {
            command_latency_ms: 150,
            drift_ms: 3_000,
            ..Default::default()
        };
        let mut rig = start(fake_cfg, driver_cfg());
        rig.load(0, true).await;
        rig.until_state(ConnState::Following).await;
        let intents = [
            GroupIntent::Seek {
                position_ms: 60_000,
                playing: true,
                apply_at: Instant::now(),
            },
            GroupIntent::Pause {
                position_ms: 61_000,
                apply_at: Instant::now() + Duration::from_secs(1),
            },
            GroupIntent::Play {
                position_ms: 61_000,
                apply_at: Instant::now() + Duration::from_secs(3),
            },
            GroupIntent::Load {
                item_id: "item-2".into(),
                item_type: "Movie".into(),
                position_ms: 10_000,
                playing: true,
            },
        ];
        for intent in intents {
            rig.handle.send(intent).await.unwrap();
            tokio::time::sleep(Duration::from_secs(4)).await;
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
        let heard = user_events(&mut rig.events);
        assert!(heard.is_empty(), "echoes leaked as user actions: {heard:?}");
        assert!(rig.fake.commands().len() >= 5, "{:?}", names(&rig));
    }

    fn play_now_start_ms(cmd: &crate::messages::ReceiverCommand) -> i64 {
        match &cmd.body {
            CommandBody::PlayNow(o) => o.start_position_ticks / TICKS_PER_MS,
            other => panic!("expected PlayNow, got {other:?}"),
        }
    }

    fn lift_refusal_after(fake: &FakeReceiverHandle, after: Duration) {
        let fake = fake.clone();
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            fake.configure(|c| c.refuse_connect = false);
        });
    }

    #[tokio::test(start_paused = true)]
    async fn a_dropped_connection_is_retried_with_backoff_and_resumes_at_the_group_position() {
        let recorder = DebuggingRecorder::new();
        let snap = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let mut rig = start(FakeReceiverConfig::default(), driver_cfg());
        rig.load(10_000, true).await;
        rig.until_state(ConnState::Following).await;
        tokio::time::sleep(Duration::from_secs(2)).await;

        let lost_at = Instant::now();
        rig.fake.configure(|c| c.refuse_connect = true);
        rig.fake.drop_connection();
        lift_refusal_after(&rig.fake, Duration::from_secs(6));

        assert_eq!(rig.next().await, DriverEvent::State(ConnState::Lost));
        assert_eq!(rig.next().await, DriverEvent::State(ConnState::Loading));
        assert_eq!(rig.next().await, DriverEvent::State(ConnState::Following));
        let down = lost_at.elapsed();
        assert!(
            down >= Duration::from_secs(6) && down <= Duration::from_secs(10),
            "reconnected after {down:?}"
        );

        let cmds = rig.fake.commands();
        assert_eq!(cmds.len(), 2, "{cmds:?}");
        let resumed_at = play_now_start_ms(cmds.get(1).unwrap());
        assert!(
            (18_000..=22_000).contains(&resumed_at),
            "re-Load must start at the group's current position, got {resumed_at}"
        );
        assert!((rig.fake.state().position_ms - resumed_at).abs() < 500);

        let all = series(&snap);
        assert_eq!(
            counter(&all, "pharos_cast_seat_total", ("event", "lost")),
            1
        );
        assert_eq!(
            counter(&all, "pharos_cast_seat_total", ("event", "reconnected")),
            1
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_tv_that_stays_away_ends_the_cast_when_the_grace_expires() {
        let recorder = DebuggingRecorder::new();
        let snap = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let mut rig = start(FakeReceiverConfig::default(), driver_cfg());
        rig.load(0, true).await;
        rig.until_state(ConnState::Following).await;
        let lost_at = Instant::now();
        rig.fake.configure(|c| c.refuse_connect = true);
        rig.fake.drop_connection();

        assert_eq!(rig.next().await, DriverEvent::State(ConnState::Lost));
        assert_eq!(rig.next().await, DriverEvent::State(ConnState::Ended));
        assert_eq!(
            rig.next().await,
            DriverEvent::Ended {
                reason: CastReason::GraceExpired
            }
        );
        assert_eq!(lost_at.elapsed(), Duration::from_secs(20));
        let all = series(&snap);
        assert_eq!(
            counter(&all, "pharos_cast_seat_total", ("event", "lost")),
            1
        );
        assert_eq!(
            counter(&all, "pharos_cast_seat_total", ("reason", "grace_expired")),
            1
        );
        assert_eq!(
            counter(&all, "pharos_cast_seat_total", ("event", "reconnected")),
            0
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_failed_reconnect_names_its_cause_in_the_seat_log() {
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(VecWriter(buf.clone()))
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let mut rig = start(FakeReceiverConfig::default(), driver_cfg());
        rig.load(0, true).await;
        rig.until_state(ConnState::Following).await;
        rig.fake.configure(|c| c.refuse_connect = true);
        rig.fake.drop_connection();
        rig.until_state(ConnState::Ended).await;

        let text = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        let ended = text
            .lines()
            .find(|l| l.contains("cast seat") && l.contains("grace_expired"))
            .unwrap_or_else(|| panic!("no grace_expired seat line in:\n{text}"));
        assert!(ended.contains("refuse_connect set"), "{ended}");
        let lost = text
            .lines()
            .find(|l| l.contains("cast seat") && l.contains("event=\"lost\""))
            .unwrap_or_else(|| panic!("no lost seat line in:\n{text}"));
        assert!(lost.contains("connection dropped"), "{lost}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_while_lost_ends_at_once_without_waiting_for_the_grace() {
        let mut rig = start(FakeReceiverConfig::default(), driver_cfg());
        rig.load(0, true).await;
        rig.until_state(ConnState::Following).await;
        rig.fake.configure(|c| c.refuse_connect = true);
        rig.fake.drop_connection();
        assert_eq!(rig.next().await, DriverEvent::State(ConnState::Lost));
        let t = Instant::now();
        tokio::time::sleep(Duration::from_secs(1)).await;
        rig.handle.send(GroupIntent::Stop).await.unwrap();
        assert_eq!(rig.next().await, DriverEvent::State(ConnState::Ended));
        assert_eq!(
            rig.next().await,
            DriverEvent::Ended {
                reason: CastReason::UserStop
            }
        );
        assert!(t.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test(start_paused = true)]
    async fn group_intents_arriving_while_lost_shape_the_reload() {
        let mut rig = start(FakeReceiverConfig::default(), driver_cfg());
        rig.load(0, true).await;
        rig.until_state(ConnState::Following).await;
        rig.fake.configure(|c| c.refuse_connect = true);
        rig.fake.drop_connection();
        lift_refusal_after(&rig.fake, Duration::from_secs(3));
        assert_eq!(rig.next().await, DriverEvent::State(ConnState::Lost));
        rig.handle
            .send(GroupIntent::Seek {
                position_ms: 100_000,
                playing: false,
                apply_at: Instant::now(),
            })
            .await
            .unwrap();
        rig.until_state(ConnState::Following).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let state = rig.fake.state();
        assert!(state.paused, "{state:?}");
        assert!((state.position_ms - 100_000).abs() < 300, "{state:?}");
    }

    #[derive(Clone)]
    struct VecWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for VecWriter {
        type Writer = SharedBuf;
        fn make_writer(&'a self) -> Self::Writer {
            SharedBuf(self.0.clone())
        }
    }

    struct SharedBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for SharedBuf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
}
