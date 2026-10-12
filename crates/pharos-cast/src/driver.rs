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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Exit {
    Ended(CastReason),
    Failed(CastReason),
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
                self.enter(ConnState::Ended).await;
                self.record(CastEvent::Ended, reason, "");
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
        let command = self.envelope(body);
        self.transport.send(&command).await?;
        self.last_command_at = Some(Instant::now());
        tracing::debug!(tv = %self.cfg.receiver_name, command = name, "cast command sent");
        Ok(())
    }

    async fn send_or_exit(&mut self, body: CommandBody) -> Option<Exit> {
        let name = body.name();
        match self.send(body).await {
            Ok(()) => None,
            Err(e) => Some(self.lost(&format!("sending {name}: {e}"))),
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

    fn lost(&self, detail: &str) -> Exit {
        self.record(CastEvent::Lost, CastReason::TvUnreachable, detail);
        Exit::Ended(CastReason::TvUnreachable)
    }

    async fn on_transport(&mut self, event: TransportEvent) -> Option<Exit> {
        match event {
            TransportEvent::Report(report) => self.on_report(report).await,
            TransportEvent::HeartbeatLost => Some(self.lost("heartbeat lost")),
            TransportEvent::Closed { reason } => Some(self.lost(&format!("closed: {reason}"))),
        }
    }

    async fn on_intent(&mut self, intent: GroupIntent) -> Option<Exit> {
        match intent {
            GroupIntent::Load {
                item_id,
                item_type,
                position_ms,
                playing,
            } => self.load(item_id, item_type, position_ms, playing).await,
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
        self.flush_due(Instant::now()).await
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
            if let Some(exit) = self
                .apply(h.position_ms, h.playing, h.force_seek, h.apply_at)
                .await
            {
                return Some(exit);
            }
        }
        None
    }

    async fn load(
        &mut self,
        item_id: String,
        item_type: String,
        position_ms: i64,
        playing: bool,
    ) -> Option<Exit> {
        let body = CommandBody::PlayNow(PlayNowOptions {
            items: vec![PlayItem {
                id: item_id.clone(),
                kind: item_type,
            }],
            start_index: 0,
            start_position_ticks: position_ms.saturating_mul(TICKS_PER_MS),
            media_source_id: None,
            audio_stream_index: None,
            subtitle_stream_index: None,
        });
        if let Some(exit) = self.send_or_exit(body).await {
            return Some(exit);
        }
        self.held.clear();
        self.clock = Some(GroupClock {
            position_ms,
            at: Instant::now(),
            playing,
        });
        self.loaded = Some(LoadedItem { item_id });
        self.last_report = None;
        self.planner = PlannerState::new();
        self.expected_paused = false;
        self.load_deadline = Some(Instant::now() + self.cfg.max_load_wait);
        self.enter(ConnState::Loading).await;
        None
    }

    /// Bring the receiver to the group's new position and play state.
    async fn apply(
        &mut self,
        position_ms: i64,
        playing: bool,
        force_seek: bool,
        at: Instant,
    ) -> Option<Exit> {
        if self.loaded.is_none() {
            tracing::debug!(tv = %self.cfg.receiver_name, position_ms, "cast intent before any Load ignored");
            return None;
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
            if let Some(exit) = self.send_or_exit(CommandBody::Unpause).await {
                return Some(exit);
            }
        }
        if !playing && !self.expected_paused {
            self.expected_paused = true;
            if let Some(exit) = self.send_or_exit(CommandBody::Pause).await {
                return Some(exit);
            }
        }
        if force_seek || off {
            let body = CommandBody::Seek(SeekOptions {
                position: position_ms as f64 / 1000.0,
            });
            return self.send_or_exit(body).await;
        }
        None
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
        self.last_report = Some(snapshot);
        match self.state {
            ConnState::Loading => {
                self.load_deadline = None;
                self.enter(ConnState::Following).await;
                let playing = self.clock.map_or(true, |c| c.playing);
                if !playing {
                    self.expected_paused = true;
                    if let Some(exit) = self.send_or_exit(CommandBody::Pause).await {
                        return Some(exit);
                    }
                }
                self.flush_due(now).await
            }
            ConnState::Following if matches!(report, ReceiverReport::PlaybackProgress(_)) => {
                self.correct(now, snapshot).await
            }
            _ => None,
        }
    }

    async fn correct(&mut self, now: Instant, snapshot: Snapshot) -> Option<Exit> {
        let clock = self.clock?;
        if let Some(sent) = self.last_command_at {
            if now.saturating_duration_since(sent) < SETTLE {
                return None;
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
            Action::None => return None,
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
        self.send_or_exit(body).await
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
}
