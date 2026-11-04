//! The watch-together group store (014): one active membership, the clock
//! sync, keep-alive and drift loops that run while it lasts, and what each
//! server update and local control means for the player.
//!
//! No Dioxus and no browser in here. [`Env`] is the clock, timer, spawner and
//! event sink; [`GroupPlayer`] is the `<video>`; [`GroupClient`] is the
//! network. The app supplies browser versions, the tests supply fakes.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use futures_util::future::LocalBoxFuture;
use pharos_client::api::{ClientEvent, ClientEventKind, ClientLevel, ItemRef};

use super::playback::MediaEvent;
use super::seen::MemberSeen;
use super::together::{
    expected_position, ClockEstimator, Correction, DriftTracker, GroupClient, GroupError, GroupId,
    GroupSink, GroupSummary, GroupUpdate, Member, CLOCK_WINDOW, HOLD_BELOW_MS, SELF_SEEK_GRACE_MS,
};

/// Keep-alive cadence (T83's ghost prune is 150 s).
pub const PING_EVERY_S: u64 = 30;
/// Clock re-sync cadence, and the `groupDrift` sample cadence.
pub const SAMPLE_EVERY_S: u64 = 10;

/// What the group shows the viewer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupView {
    /// In a group now.
    pub active: bool,
    pub members: Vec<Member>,
    /// Why the group is over, once it is.
    pub ended: Option<String>,
}

/// What the store asks the screens to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ui {
    View(GroupView),
    /// Show this item on the watch page.
    Navigate(ItemRef),
}

/// The world outside the store.
pub trait Env {
    /// Wall-clock unix milliseconds.
    fn now_ms(&self) -> i64;
    fn sleep(&self, ms: u32) -> LocalBoxFuture<'static, ()>;
    fn spawn(&self, fut: LocalBoxFuture<'static, ()>);
    /// A client event for telemetry (sent only when the viewer opted in).
    fn report(&self, event: ClientEvent);
    fn ui(&self, ui: Ui);
    fn warn(&self, message: &str);
    /// A decision the group took, for the console and the field logs.
    fn info(&self, message: &str);
}

/// The `<video>` the group drives.
pub trait GroupPlayer {
    fn position_ms(&self) -> u64;
    fn paused(&self) -> bool;
    fn play(&self);
    fn pause(&self);
    fn seek_ms(&self, ms: u64);
    fn set_rate(&self, rate: f64);
}

/// When an anchor was struck.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stamp {
    /// In server time.
    Server(i64),
    /// At this local time, before the clock had a sample to turn it into
    /// server time.
    Local(i64),
}

/// Where the group's playback is, as the server last stated it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Anchor {
    position_ms: u64,
    stamp: Stamp,
    playing: bool,
}

/// What a server update calls for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Navigate(ItemRef),
    /// After `delay_ms`, be at `position_ms` and playing.
    Play {
        delay_ms: u64,
        position_ms: u64,
    },
    /// After `delay_ms`, be at `position_ms` and paused.
    Pause {
        delay_ms: u64,
        position_ms: u64,
    },
    /// After `delay_ms`, be at `position_ms`.
    Seek {
        delay_ms: u64,
        position_ms: u64,
    },
}

/// The group as the client knows it. Pure: every change comes through
/// [`GroupState::apply`].
#[derive(Debug, Default)]
pub struct GroupState {
    pub id: Option<GroupId>,
    pub members: Vec<Member>,
    /// The queue entry the group is on, which readiness reports carry.
    pub playlist_item_id: String,
    pub ended: Option<String>,
    /// The item the watch page shows.
    watching: Option<ItemRef>,
    clock: ClockEstimator,
    anchor: Option<Anchor>,
    last_command_ms: Option<i64>,
    /// A new item or a seek landed: the group waits for this member's ready.
    owes_ready: bool,
}

impl GroupState {
    pub fn clock(&self) -> &ClockEstimator {
        &self.clock
    }

    /// Take a clock sample. `true` when it is the first, and an anchor that
    /// was waiting for it can now be read.
    pub fn push_clock(&mut self, sample: super::together::ClockSample) -> bool {
        self.clock.push(sample);
        let Some(anchor) = self.anchor.as_mut() else {
            return false;
        };
        let Stamp::Local(at) = anchor.stamp else {
            return false;
        };
        let Some(server_ms) = self.clock.server_now(at) else {
            return false;
        };
        anchor.stamp = Stamp::Server(server_ms);
        true
    }

    pub fn set_watching(&mut self, item: Option<ItemRef>) {
        self.watching = item;
    }

    /// Server time now, once the clock has a sample.
    fn server_now(&self, now_ms: i64) -> Option<i64> {
        self.clock.server_now(now_ms)
    }

    /// When a command stamped `at_server_ms` falls: the wait to it, or how
    /// late it already is.
    fn due(&self, at_server_ms: i64, now_ms: i64) -> (u64, u64) {
        match self.server_now(now_ms) {
            Some(here) if at_server_ms > here => {
                (u64::try_from(at_server_ms - here).unwrap_or(0), 0)
            }
            Some(here) => (0, u64::try_from(here - at_server_ms).unwrap_or(0)),
            None => (0, 0),
        }
    }

    /// Where the group is now, and whether it is playing; `None` before it
    /// has said anything.
    pub fn expected(&self, now_ms: i64) -> Option<(u64, bool)> {
        let a = self.anchor?;
        let here = self.server_now(now_ms)?;
        let server_ms = match a.stamp {
            Stamp::Server(ms) => ms,
            Stamp::Local(at) => self.server_now(at)?,
        };
        Some((
            expected_position(a.position_ms, server_ms, here, a.playing),
            a.playing,
        ))
    }

    /// Milliseconds since the last server command, large before any.
    pub fn since_command_ms(&self, now_ms: i64) -> u64 {
        self.last_command_ms
            .map_or(u64::MAX, |t| u64::try_from(now_ms - t).unwrap_or(0))
    }

    /// Take an update; the actions for the player and screens.
    pub fn apply(&mut self, update: GroupUpdate, now_ms: i64) -> Vec<Action> {
        match update {
            GroupUpdate::Joined(g) => {
                self.id = Some(g.id);
                self.members = g.members;
                Vec::new()
            }
            GroupUpdate::Member(m) => {
                match self.members.iter_mut().find(|x| x.id == m.id) {
                    Some(x) => *x = m,
                    None => self.members.push(m),
                }
                Vec::new()
            }
            GroupUpdate::Left(id) => {
                self.members.retain(|m| m.id != id);
                Vec::new()
            }
            GroupUpdate::Item {
                item,
                playlist_item_id,
                position_ms,
                playing,
            } => {
                self.playlist_item_id = playlist_item_id;
                self.owes_ready = true;
                self.last_command_ms = Some(now_ms);
                self.anchor = Some(Anchor {
                    position_ms,
                    stamp: self
                        .server_now(now_ms)
                        .map_or(Stamp::Local(now_ms), Stamp::Server),
                    playing,
                });
                if self.watching == Some(item) {
                    Vec::new()
                } else {
                    vec![Action::Navigate(item)]
                }
            }
            GroupUpdate::Play {
                at_server_ms,
                position_ms,
            } => {
                let (delay_ms, late) = self.due(at_server_ms, now_ms);
                self.settle(position_ms, at_server_ms, true, now_ms);
                vec![Action::Play {
                    delay_ms,
                    position_ms: position_ms + late,
                }]
            }
            GroupUpdate::Pause {
                at_server_ms,
                position_ms,
            } => {
                let (delay_ms, _) = self.due(at_server_ms, now_ms);
                self.settle(position_ms, at_server_ms, false, now_ms);
                vec![Action::Pause {
                    delay_ms,
                    position_ms,
                }]
            }
            GroupUpdate::Seek {
                at_server_ms,
                position_ms,
            } => {
                let playing = self.anchor.map(|a| a.playing);
                let (delay_ms, late) = self.due(at_server_ms, now_ms);
                // Without an anchor the seek cannot say whether the group
                // plays, so it states no anchor either.
                match playing {
                    Some(playing) => self.settle(position_ms, at_server_ms, playing, now_ms),
                    None => self.last_command_ms = Some(now_ms),
                }
                self.owes_ready = true;
                vec![Action::Seek {
                    delay_ms,
                    position_ms: if playing == Some(true) {
                        position_ms + late
                    } else {
                        position_ms
                    },
                }]
            }
            GroupUpdate::Ended => {
                self.ended = Some("This group has ended".to_owned());
                self.id = None;
                self.members.clear();
                self.anchor = None;
                Vec::new()
            }
        }
    }

    fn settle(&mut self, position_ms: u64, server_ms: i64, playing: bool, now_ms: i64) {
        self.anchor = Some(Anchor {
            position_ms,
            stamp: Stamp::Server(server_ms),
            playing,
        });
        self.last_command_ms = Some(now_ms);
    }

    pub fn view(&self, active: bool) -> GroupView {
        GroupView {
            active,
            members: self.members.clone(),
            ended: self.ended.clone(),
        }
    }
}

#[derive(Default)]
struct Inner {
    state: GroupState,
    active: bool,
    /// Bumped when a membership starts or stops; loops and delayed actions
    /// of an older one stand down.
    epoch: u64,
    player: Option<Rc<dyn GroupPlayer>>,
    tracker: DriftTracker,
    rate: f64,
    buffering_sent: bool,
    /// When this member said ready, until the group's next play: the time it
    /// spent waiting on the others.
    ready_sent_ms: Option<i64>,
    last_drift_ms: Option<i64>,
    /// This member left; what the server still sends is not for the screens.
    leaving: bool,
    /// When this session last moved the player itself.
    self_seek_ms: Option<i64>,
    /// The video waited within the grace of our own seek; it is reported as
    /// buffering only if it outlasts the grace.
    held_waiting: bool,
}

impl Inner {
    fn within_self_seek_grace(&self, now: i64) -> bool {
        let grace = i64::try_from(SELF_SEEK_GRACE_MS).unwrap_or(i64::MAX);
        self.self_seek_ms
            .is_some_and(|seek| now.saturating_sub(seek) < grace)
    }

    /// Milliseconds since this session's last own seek, 0 when there was none.
    fn since_self_seek(&self, now: i64) -> i64 {
        self.self_seek_ms.map_or(0, |seek| now.saturating_sub(seek))
    }
}

/// The one active membership and everything that runs for it.
pub struct Session<C, E> {
    client: C,
    env: E,
    inner: RefCell<Inner>,
    this: RefCell<Weak<Self>>,
}

impl<C: GroupClient + Clone + 'static, E: Env + 'static> Session<C, E> {
    pub fn new(client: C, env: E) -> Rc<Self> {
        let session = Rc::new(Self {
            client,
            env,
            inner: RefCell::new(Inner {
                rate: 1.0,
                ..Inner::default()
            }),
            this: RefCell::new(Weak::new()),
        });
        *session.this.borrow_mut() = Rc::downgrade(&session);
        session
    }

    fn me(&self) -> Option<Rc<Self>> {
        self.this.borrow().upgrade()
    }

    /// Whether a membership is active.
    pub fn active(&self) -> bool {
        self.inner.borrow().active
    }

    /// What each member has seen of `ids`; empty when not in a group.
    pub async fn seen(&self, ids: Vec<ItemRef>) -> Result<Vec<MemberSeen>, GroupError> {
        if !self.active() {
            return Ok(Vec::new());
        }
        self.client.seen(ids).await
    }

    pub fn view(&self) -> GroupView {
        let inner = self.inner.borrow();
        inner.state.view(inner.active)
    }

    fn publish(&self) {
        self.env.ui(Ui::View(self.view()));
    }

    /// A sink for the membership of `epoch`; once that membership is over, what
    /// it still receives is dropped.
    fn sink(&self, epoch: u64) -> GroupSink {
        let weak = self.this.borrow().clone();
        Rc::new(move |update| {
            if let Some(session) = weak.upgrade() {
                session.deliver_in(epoch, update);
            }
        })
    }

    /// Start a group on `item` at `position_ms`.
    pub async fn create(
        &self,
        item: ItemRef,
        position_ms: u64,
    ) -> Result<GroupSummary, GroupError> {
        let epoch = self.prepare(Some(item)).await;
        let made = self
            .client
            .create(item, position_ms, self.sink(epoch))
            .await;
        self.began(made)
    }

    /// Join the group `id`.
    pub async fn join(&self, id: &str) -> Result<GroupSummary, GroupError> {
        let epoch = self.prepare(None).await;
        let made = self.client.join(id, self.sink(epoch)).await;
        self.began(made)
    }

    /// A clean slate for a new membership, whose epoch it returns; an old
    /// one is left first. The page being watched stays unless `watching`
    /// names another.
    async fn prepare(&self, watching: Option<ItemRef>) -> u64 {
        if self.active() {
            self.leave().await;
        }
        let mut inner = self.inner.borrow_mut();
        inner.epoch += 1;
        let epoch = inner.epoch;
        let player = inner.player.take();
        let shown = inner.state.watching;
        *inner = Inner {
            epoch,
            player,
            rate: 1.0,
            ..Inner::default()
        };
        inner.state.watching = watching.or(shown);
        epoch
    }

    /// Take the player and the watched item of a session this one replaces.
    pub fn adopt(&self, old: &Self) {
        let (player, watching) = {
            let inner = old.inner.borrow();
            (inner.player.clone(), inner.state.watching)
        };
        let mut inner = self.inner.borrow_mut();
        inner.player = player;
        inner.state.watching = watching;
    }

    fn began(&self, made: Result<GroupSummary, GroupError>) -> Result<GroupSummary, GroupError> {
        let summary = made?;
        let epoch = {
            let mut inner = self.inner.borrow_mut();
            // An `Ended` that raced the answer leaves nothing to run.
            if inner.state.ended.is_some() {
                return Err(GroupError("This group has ended".into()));
            }
            inner.active = true;
            inner.state.id = Some(summary.id.clone());
            if inner.state.members.is_empty() {
                inner.state.members = summary.members.clone();
            }
            inner.epoch
        };
        self.publish();
        if let Some(me) = self.me() {
            self.env.spawn(Box::pin(async move { me.run(epoch).await }));
        }
        Ok(summary)
    }

    /// Leave the group. The player keeps what it is playing.
    pub async fn leave(&self) {
        let was = {
            let mut inner = self.inner.borrow_mut();
            inner.epoch += 1;
            inner.leaving = true;
            std::mem::replace(&mut inner.active, false)
        };
        self.reset_rate();
        self.publish();
        if was {
            if let Err(e) = self.client.leave().await {
                self.env
                    .warn(&format!("pharos: leaving the group failed: {}", e.0));
            }
        }
    }

    fn reset_rate(&self) {
        let player = {
            let mut inner = self.inner.borrow_mut();
            inner.tracker = DriftTracker::default();
            let nudged = (inner.rate - 1.0).abs() > f64::EPSILON;
            inner.rate = 1.0;
            nudged.then(|| inner.player.clone()).flatten()
        };
        if let Some(player) = player {
            player.set_rate(1.0);
        }
    }

    /// The watch page's `<video>`, or none when it goes. A player that
    /// arrives mid-playback catches up to the group.
    pub fn set_player(&self, player: Option<Rc<dyn GroupPlayer>>) {
        self.inner.borrow_mut().player = player;
        self.catch_up();
    }

    /// Put the player where the group is: there, and playing if it plays.
    fn catch_up(&self) {
        let (player, expected) = {
            let inner = self.inner.borrow();
            (
                inner.player.clone(),
                inner
                    .active
                    .then(|| inner.state.expected(self.env.now_ms()))
                    .flatten(),
            )
        };
        if let (Some(player), Some((position_ms, playing))) = (player, expected) {
            self.inner.borrow_mut().self_seek_ms = Some(self.env.now_ms());
            player.seek_ms(position_ms);
            if playing {
                player.play();
            }
        }
    }

    /// The item the watch page shows.
    pub fn set_watching(&self, item: Option<ItemRef>) {
        self.inner.borrow_mut().state.set_watching(item);
    }

    /// An update from the server, for the current membership.
    pub fn deliver(&self, update: GroupUpdate) {
        let epoch = self.inner.borrow().epoch;
        self.deliver_in(epoch, update);
    }

    /// An update from the membership of `epoch`; dropped once that one is
    /// over.
    fn deliver_in(&self, epoch: u64, update: GroupUpdate) {
        let now = self.env.now_ms();
        let ended = update == GroupUpdate::Ended;
        let is_play = matches!(update, GroupUpdate::Play { .. });
        let (actions, wait) = {
            let mut inner = self.inner.borrow_mut();
            if inner.leaving || inner.epoch != epoch {
                return;
            }
            let actions = inner.state.apply(update, now);
            let wait = if is_play {
                inner.ready_sent_ms.take().map(|t| now - t)
            } else {
                None
            };
            if ended {
                inner.epoch += 1;
                inner.active = false;
            }
            (actions, wait)
        };
        if ended {
            self.reset_rate();
        }
        if let Some(wait_ms) = wait {
            self.emit(
                ClientEventKind::GroupWait,
                ClientLevel::Info,
                vec![("wait_ms", wait_ms.max(0).to_string())],
            );
        }
        self.publish();
        let epoch = self.inner.borrow().epoch;
        for action in actions {
            self.perform(action, epoch);
        }
    }

    fn perform(&self, action: Action, epoch: u64) {
        let (delay_ms, position_ms, then): (u64, u64, fn(&dyn GroupPlayer)) = match action {
            Action::Navigate(item) => {
                self.env.ui(Ui::Navigate(item));
                return;
            }
            Action::Play {
                delay_ms,
                position_ms,
            } => (delay_ms, position_ms, |p| p.play()),
            Action::Pause {
                delay_ms,
                position_ms,
            } => (delay_ms, position_ms, |p| p.pause()),
            Action::Seek {
                delay_ms,
                position_ms,
            } => (delay_ms, position_ms, |_| {}),
        };
        let Some(me) = self.me() else {
            return;
        };
        self.env.spawn(Box::pin(async move {
            if delay_ms > 0 {
                me.env
                    .sleep(u32::try_from(delay_ms).unwrap_or(u32::MAX))
                    .await;
            }
            let player = {
                let inner = me.inner.borrow();
                if inner.epoch != epoch {
                    return;
                }
                inner.player.clone()
            };
            let Some(player) = player else {
                return;
            };
            if player.position_ms().abs_diff(position_ms) > HOLD_BELOW_MS {
                me.inner.borrow_mut().self_seek_ms = Some(me.env.now_ms());
                player.seek_ms(position_ms);
            }
            then(&*player);
        }));
    }

    fn emit(
        &self,
        kind: ClientEventKind,
        level: ClientLevel,
        mut fields: Vec<(&'static str, String)>,
    ) {
        let Some(group) = self.inner.borrow().state.id.clone() else {
            return;
        };
        fields.insert(0, ("group", group));
        self.env.report(ClientEvent {
            kind,
            level,
            route: "watch".to_owned(),
            fields,
        });
    }

    // ---- loops -----------------------------------------------------------

    /// Everything that runs for a membership: the first clock samples, then
    /// a tick a second until the membership ends.
    async fn run(&self, epoch: u64) {
        for _ in 0..CLOCK_WINDOW {
            if self.inner.borrow().epoch != epoch {
                return;
            }
            self.sync_clock().await;
        }
        let mut n = 0u64;
        loop {
            self.env.sleep(1_000).await;
            if self.inner.borrow().epoch != epoch {
                return;
            }
            n += 1;
            self.tick(n).await;
        }
    }

    /// The work for the `n`th second of a membership: the drift check every
    /// second, the drift sample and clock sync every 10 s, the ping every 30 s.
    pub async fn tick(&self, n: u64) {
        self.check_drift();
        self.report_held_stall();
        if n % SAMPLE_EVERY_S == 0 {
            self.report_drift();
            self.sync_clock().await;
        }
        if n % PING_EVERY_S == 0 {
            if let Err(e) = self.client.ping().await {
                self.env
                    .warn(&format!("pharos: group ping failed: {}", e.0));
            }
        }
    }

    /// One clock round trip, closed with `clockReport`.
    pub async fn sync_clock(&self) {
        let t1 = self.env.now_ms();
        match self.client.clock(t1).await {
            Ok(reply) => {
                let t4 = self.env.now_ms();
                let first = self.inner.borrow_mut().state.push_clock(reply.sample(t4));
                if first {
                    self.catch_up();
                }
                if let Err(e) = self
                    .client
                    .clock_report(t1, reply.recv_ms, reply.send_ms, t4)
                    .await
                {
                    self.env
                        .warn(&format!("pharos: clock report failed: {}", e.0));
                }
            }
            Err(e) => self
                .env
                .warn(&format!("pharos: group clock failed: {}", e.0)),
        }
    }

    /// Hold the player to the group's clock.
    pub fn check_drift(&self) {
        let now = self.env.now_ms();
        let (player, expected, since, nudged_before) = {
            let inner = self.inner.borrow();
            let Some(player) = inner.player.clone() else {
                return;
            };
            let Some((expected, true)) = inner.state.expected(now) else {
                return;
            };
            (
                player,
                expected,
                inner.state.since_command_ms(now),
                (inner.rate - 1.0).abs() > f64::EPSILON,
            )
        };
        // Still loading after a seek of ours: another seek would restart the
        // load and push back the grace that reports a real stall.
        if player.paused() || self.inner.borrow().held_waiting {
            return;
        }
        let actual = player.position_ms();
        let expected = i64::try_from(expected).unwrap_or(i64::MAX);
        let actual_i = i64::try_from(actual).unwrap_or(i64::MAX);
        let correction = {
            let mut inner = self.inner.borrow_mut();
            inner.last_drift_ms = Some(actual_i - expected);
            inner.tracker.check(expected, actual_i, since)
        };
        match correction {
            Correction::Hold if nudged_before => self.set_rate(&player, 1.0),
            Correction::Hold => {}
            Correction::Rate(rate) => self.set_rate(&player, rate),
            Correction::Seek(to) => {
                self.set_rate(&player, 1.0);
                self.inner.borrow_mut().self_seek_ms = Some(now);
                player.seek_ms(to);
                self.emit(
                    ClientEventKind::GroupHardSeek,
                    ClientLevel::Warn,
                    vec![("drift_ms", (actual_i - expected).to_string())],
                );
            }
        }
    }

    fn set_rate(&self, player: &Rc<dyn GroupPlayer>, rate: f64) {
        let changed = {
            let mut inner = self.inner.borrow_mut();
            let changed = (inner.rate - rate).abs() > f64::EPSILON;
            inner.rate = rate;
            changed
        };
        if changed {
            player.set_rate(rate);
        }
    }

    /// The `groupDrift` sample: where the playhead is against the group's clock.
    pub fn report_drift(&self) {
        let drift = self.inner.borrow().last_drift_ms;
        if let Some(drift_ms) = drift {
            self.emit(
                ClientEventKind::GroupDrift,
                ClientLevel::Info,
                vec![("drift_ms", drift_ms.to_string())],
            );
        }
    }

    // ---- local controls ----------------------------------------------------

    /// Play or pause as the viewer asked: a group command, not the player.
    /// `false` when not in a group (the player acts on its own).
    pub fn toggle_play(&self) -> bool {
        let (active, paused) = {
            let inner = self.inner.borrow();
            (
                inner.active,
                inner.player.as_ref().map_or(true, |p| p.paused()),
            )
        };
        if !active {
            return false;
        }
        let client = self.client.clone();
        self.command(async move {
            if paused {
                client.unpause().await
            } else {
                client.pause().await
            }
        });
        true
    }

    /// Seek as the viewer asked: a group command. `false` when not in a group.
    pub fn seek(&self, position_ms: u64) -> bool {
        if !self.active() {
            return false;
        }
        let client = self.client.clone();
        self.command(async move { client.seek(position_ms).await });
        true
    }

    fn command(&self, fut: impl std::future::Future<Output = Result<(), GroupError>> + 'static) {
        let Some(me) = self.me() else {
            return;
        };
        self.env.spawn(Box::pin(async move {
            if let Err(e) = fut.await {
                me.env
                    .warn(&format!("pharos: group command failed: {}", e.0));
            }
        }));
    }

    /// A wait held past the self-seek grace is a real stall: report it as
    /// buffering now, once.
    fn report_held_stall(&self) {
        let now = self.env.now_ms();
        let (playlist, position, buffering, line) = {
            let mut inner = self.inner.borrow_mut();
            if !inner.active
                || !inner.held_waiting
                || inner.buffering_sent
                || inner.state.owes_ready
                || inner.within_self_seek_grace(now)
            {
                return;
            }
            let Some(player) = inner.player.clone() else {
                return;
            };
            let line = format!(
                "pharos: group wait reported reason=grace_expired held_ms={} position_ms={}",
                inner.since_self_seek(now),
                player.position_ms()
            );
            // `held_waiting` stays set: the drift check keeps off a player
            // still waiting until `canplay` or `playing` clears it.
            inner.buffering_sent = true;
            (
                inner.state.playlist_item_id.clone(),
                player.position_ms(),
                true,
                line,
            )
        };
        self.env.info(&line);
        self.report_buffering(playlist, position, buffering);
    }

    fn report_buffering(&self, playlist: String, position_ms: u64, buffering: bool) {
        let client = self.client.clone();
        self.command(async move { client.buffering(playlist, position_ms, buffering).await });
    }

    /// The local video's events, as the group needs them: buffering when it
    /// runs dry, ready when it can play a new item or a new position.
    pub fn on_media(&self, event: MediaEvent) {
        let now = self.env.now_ms();
        let (playlist, position, playing, send_buffering, send_ready, line) = {
            let mut inner = self.inner.borrow_mut();
            if !inner.active {
                return;
            }
            let Some(player) = inner.player.clone() else {
                return;
            };
            let owes = inner.state.owes_ready;
            let (mut buffering, mut ready) = (None, false);
            let mut line = None;
            match event {
                MediaEvent::Waiting if !owes && !inner.buffering_sent => {
                    if inner.within_self_seek_grace(now) {
                        line = Some(format!(
                            "pharos: group wait held reason=self_seek since_seek_ms={} position_ms={}",
                            inner.since_self_seek(now),
                            player.position_ms()
                        ));
                        inner.held_waiting = true;
                    } else {
                        inner.buffering_sent = true;
                        buffering = Some(true);
                    }
                }
                MediaEvent::CanPlay | MediaEvent::Seeked | MediaEvent::Playing => {
                    if matches!(event, MediaEvent::CanPlay | MediaEvent::Playing) {
                        if inner.held_waiting {
                            let reason = if matches!(event, MediaEvent::CanPlay) {
                                "canplay"
                            } else {
                                "playing"
                            };
                            line = Some(format!(
                                "pharos: group wait released reason={reason} held_ms={} position_ms={}",
                                inner.since_self_seek(now),
                                player.position_ms()
                            ));
                        }
                        inner.held_waiting = false;
                        inner.self_seek_ms = None;
                    }
                    if inner.buffering_sent {
                        inner.buffering_sent = false;
                        buffering = Some(false);
                    }
                    if owes {
                        inner.state.owes_ready = false;
                        inner.ready_sent_ms = Some(now);
                        ready = true;
                    }
                }
                _ => {}
            }
            (
                inner.state.playlist_item_id.clone(),
                player.position_ms(),
                !player.paused(),
                buffering,
                ready,
                line,
            )
        };
        if let Some(line) = line {
            self.env.info(&line);
        }
        if let Some(buffering) = send_buffering {
            self.report_buffering(playlist.clone(), position, buffering);
        }
        if send_ready {
            let client = self.client.clone();
            self.command(async move { client.ready(playlist, position, playing).await });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use futures_util::FutureExt as _;

    use super::*;
    use crate::native::together::{ClockSample, FakeGroups};

    #[derive(Clone, Default)]
    struct TestEnv {
        now: Rc<Cell<i64>>,
        reports: Rc<RefCell<Vec<ClientEvent>>>,
        ui: Rc<RefCell<Vec<Ui>>>,
        tasks: Rc<RefCell<Vec<LocalBoxFuture<'static, ()>>>>,
        logs: Rc<RefCell<Vec<String>>>,
    }

    impl Env for TestEnv {
        fn now_ms(&self) -> i64 {
            self.now.get()
        }
        /// The one-second tick parks, so the membership's loop does not spin;
        /// other waits (a scheduled command) pass at once.
        fn sleep(&self, ms: u32) -> LocalBoxFuture<'static, ()> {
            if ms == 1_000 {
                std::future::pending().boxed_local()
            } else {
                std::future::ready(()).boxed_local()
            }
        }
        fn spawn(&self, fut: LocalBoxFuture<'static, ()>) {
            self.tasks.borrow_mut().push(fut);
        }
        fn report(&self, event: ClientEvent) {
            self.reports.borrow_mut().push(event);
        }
        fn ui(&self, ui: Ui) {
            self.ui.borrow_mut().push(ui);
        }
        fn warn(&self, _message: &str) {}
        fn info(&self, message: &str) {
            self.logs.borrow_mut().push(message.to_owned());
        }
    }

    impl TestEnv {
        /// Poll every spawned task until none is new; a parked one (the tick
        /// loop) is dropped.
        fn settle(&self) {
            loop {
                let tasks: Vec<_> = self.tasks.borrow_mut().drain(..).collect();
                if tasks.is_empty() {
                    return;
                }
                for task in tasks {
                    let _ = task.now_or_never();
                }
            }
        }

        fn kinds(&self) -> Vec<ClientEventKind> {
            self.reports.borrow().iter().map(|e| e.kind).collect()
        }

        /// The logged lines that start with `prefix`.
        fn logged(&self, prefix: &str) -> Vec<String> {
            self.logs
                .borrow()
                .iter()
                .filter(|l| l.starts_with(prefix))
                .cloned()
                .collect()
        }
    }

    #[derive(Default)]
    struct TestPlayer {
        position: Cell<u64>,
        paused: Cell<bool>,
        log: RefCell<Vec<String>>,
    }

    impl GroupPlayer for TestPlayer {
        fn position_ms(&self) -> u64 {
            self.position.get()
        }
        fn paused(&self) -> bool {
            self.paused.get()
        }
        fn play(&self) {
            self.paused.set(false);
            self.log.borrow_mut().push("play".into());
        }
        fn pause(&self) {
            self.paused.set(true);
            self.log.borrow_mut().push("pause".into());
        }
        fn seek_ms(&self, ms: u64) {
            self.position.set(ms);
            self.log.borrow_mut().push(format!("seek({ms})"));
        }
        fn set_rate(&self, rate: f64) {
            self.log.borrow_mut().push(format!("rate({rate})"));
        }
    }

    fn member(id: &str, buffering: bool) -> Member {
        Member {
            id: id.into(),
            name: id.to_uppercase(),
            buffering,
        }
    }

    fn summary() -> GroupSummary {
        GroupSummary {
            id: "g1".into(),
            name: "Friday".into(),
            item: None,
            members: vec![member("me", false)],
        }
    }

    struct Rig {
        session: Rc<Session<FakeGroups, TestEnv>>,
        groups: FakeGroups,
        env: TestEnv,
        player: Rc<TestPlayer>,
    }

    /// A session in a group, past its first nine clock samples.
    fn rig() -> Rig {
        let groups = FakeGroups::with_groups(vec![summary()]);
        let env = TestEnv::default();
        env.now.set(100_000);
        let session = Session::new(groups.clone(), env.clone());
        let player = Rc::new(TestPlayer::default());
        session.set_player(Some(player.clone()));
        let joined = session.join("g1");
        assert!(matches!(joined.now_or_never(), Some(Ok(_))));
        env.settle();
        Rig {
            session,
            groups,
            env,
            player,
        }
    }

    fn calls(rig: &Rig) -> Vec<String> {
        rig.groups.calls()
    }

    fn calls_with(rig: &Rig, prefix: &str) -> Vec<String> {
        calls(rig)
            .into_iter()
            .filter(|c| c.starts_with(prefix))
            .collect()
    }

    #[test]
    fn seen_asks_the_group_only_while_in_one() {
        let rig = rig();
        let ids = vec![ItemRef::Real(1), ItemRef::Real(2)];
        let asked = rig.session.seen(ids.clone()).now_or_never();
        assert!(matches!(asked, Some(Ok(_))));
        assert_eq!(calls_with(&rig, "seen("), vec!["seen(2)".to_owned()]);
        let idle = Session::new(FakeGroups::default(), TestEnv::default());
        let none = idle.seen(ids).now_or_never();
        assert!(matches!(none, Some(Ok(v)) if v.is_empty()));
    }

    #[test]
    fn nine_clock_round_trips_on_join_each_reported() {
        let rig = rig();
        assert_eq!(calls_with(&rig, "clock(").len(), 9);
        assert_eq!(calls_with(&rig, "clock_report(").len(), 9);
        assert_eq!(
            rig.session.inner.borrow().state.clock().offset_ms(),
            Some(0)
        );
    }

    #[test]
    fn a_tick_syncs_the_clock_every_10_s_and_pings_every_30_s() {
        let rig = rig();
        let clocks = calls_with(&rig, "clock(").len();
        for n in 1..=30 {
            let _ = rig.session.tick(n).now_or_never();
        }
        assert_eq!(calls_with(&rig, "clock(").len() - clocks, 3);
        assert_eq!(calls_with(&rig, "ping").len(), 1);
    }

    #[test]
    fn joined_member_and_left_drive_the_member_strip() {
        let rig = rig();
        rig.session.deliver(GroupUpdate::Joined(GroupSummary {
            members: vec![member("me", false), member("ada", false)],
            ..summary()
        }));
        rig.session
            .deliver(GroupUpdate::Member(member("bo", false)));
        rig.session
            .deliver(GroupUpdate::Member(member("ada", true)));
        rig.session.deliver(GroupUpdate::Left("me".into()));
        let view = rig.session.view();
        assert!(view.active);
        assert_eq!(view.members, [member("ada", true), member("bo", false)]);
        assert_eq!(rig.env.ui.borrow().last(), Some(&Ui::View(view)));
    }

    #[test]
    fn a_new_item_navigates_unless_it_is_already_showing() {
        let rig = rig();
        let item = |id| GroupUpdate::Item {
            item: ItemRef::Real(id),
            playlist_item_id: "p1".into(),
            position_ms: 0,
            playing: false,
        };
        rig.session.set_watching(Some(ItemRef::Real(7)));
        rig.session.deliver(item(7));
        assert!(!rig
            .env
            .ui
            .borrow()
            .contains(&Ui::Navigate(ItemRef::Real(7))));
        rig.session.deliver(item(8));
        assert!(rig
            .env
            .ui
            .borrow()
            .contains(&Ui::Navigate(ItemRef::Real(8))));
    }

    #[test]
    fn play_pause_and_seek_run_on_the_server_clock() {
        let mut state = GroupState::default();
        // The server clock runs 1000 ms ahead of ours.
        state.push_clock(ClockSample {
            t1: 0,
            t2: 1_000,
            t3: 1_000,
            t4: 0,
        });
        // 500 ms from now in server time: wait for it.
        assert_eq!(
            state.apply(
                GroupUpdate::Play {
                    at_server_ms: 11_500,
                    position_ms: 5_000
                },
                10_000
            ),
            [Action::Play {
                delay_ms: 500,
                position_ms: 5_000
            }]
        );
        // Already 300 ms past its time: join it where it has got to.
        assert_eq!(
            state.apply(
                GroupUpdate::Seek {
                    at_server_ms: 11_000,
                    position_ms: 20_000
                },
                10_300
            ),
            [Action::Seek {
                delay_ms: 0,
                position_ms: 20_300
            }]
        );
        assert_eq!(
            state.apply(
                GroupUpdate::Pause {
                    at_server_ms: 12_000,
                    position_ms: 21_000
                },
                11_000
            ),
            [Action::Pause {
                delay_ms: 0,
                position_ms: 21_000
            }]
        );
        assert_eq!(state.expected(11_500), Some((21_000, false)));
    }

    #[test]
    fn a_scheduled_play_seeks_and_plays_the_local_video() {
        let rig = rig();
        rig.player.paused.set(true);
        rig.session.deliver(GroupUpdate::Play {
            at_server_ms: 100_000,
            position_ms: 5_000,
        });
        rig.env.settle();
        assert_eq!(*rig.player.log.borrow(), ["seek(5000)", "play"]);
        rig.session.deliver(GroupUpdate::Pause {
            at_server_ms: 100_000,
            position_ms: 9_000,
        });
        rig.env.settle();
        assert_eq!(
            rig.player.log.borrow()[2..],
            ["seek(9000)".to_owned(), "pause".to_owned()]
        );
    }

    #[test]
    fn ended_clears_the_group_and_says_why() {
        let rig = rig();
        rig.session.deliver(GroupUpdate::Ended);
        let view = rig.session.view();
        assert!(!view.active);
        assert!(view.members.is_empty());
        assert_eq!(view.ended.as_deref(), Some("This group has ended"));
        let before = calls(&rig).len();
        rig.session.check_drift();
        assert_eq!(calls(&rig).len(), before);
    }

    #[test]
    fn leaving_sends_leave_and_ignores_the_ended_that_follows() {
        let rig = rig();
        let _ = rig.session.leave().now_or_never();
        assert!(calls(&rig).contains(&"leave".to_owned()));
        rig.session.deliver(GroupUpdate::Ended);
        let view = rig.session.view();
        assert!(!view.active);
        assert_eq!(view.ended, None);
    }

    #[test]
    fn local_controls_in_a_group_send_commands_not_play() {
        let rig = rig();
        rig.player.paused.set(true);
        assert!(rig.session.toggle_play());
        rig.player.paused.set(false);
        assert!(rig.session.toggle_play());
        assert!(rig.session.seek(42_000));
        rig.env.settle();
        let calls = calls(&rig);
        assert_eq!(
            calls[calls.len() - 3..],
            ["unpause", "pause", "seek(42000)"]
        );
        assert!(
            rig.player.log.borrow().is_empty(),
            "the player is not touched"
        );
    }

    #[test]
    fn controls_outside_a_group_are_left_to_the_player() {
        let groups = FakeGroups::default();
        let session = Session::new(groups.clone(), TestEnv::default());
        assert!(!session.toggle_play());
        assert!(!session.seek(1_000));
        assert!(groups.calls().is_empty());
    }

    #[test]
    fn buffering_and_ready_carry_the_last_items_playlist_id() {
        let rig = rig();
        rig.session.deliver(GroupUpdate::Item {
            item: ItemRef::Real(7),
            playlist_item_id: "pl-7".into(),
            position_ms: 3_000,
            playing: false,
        });
        rig.player.position.set(3_000);
        rig.player.paused.set(true);
        // Loading the new item is covered by ready, not a buffering report.
        rig.session.on_media(MediaEvent::Waiting);
        rig.session.on_media(MediaEvent::CanPlay);
        rig.session.on_media(MediaEvent::CanPlay);
        rig.env.settle();
        assert_eq!(calls_with(&rig, "ready"), ["ready(pl-7,3000,false)"]);
        assert!(calls_with(&rig, "buffering").is_empty());

        // Running dry later is buffering, and recovering ends it.
        rig.session.on_media(MediaEvent::Waiting);
        rig.session.on_media(MediaEvent::Waiting);
        rig.session.on_media(MediaEvent::CanPlay);
        rig.env.settle();
        assert_eq!(
            calls_with(&rig, "buffering"),
            ["buffering(pl-7,3000,true)", "buffering(pl-7,3000,false)"]
        );
    }

    /// The session's own seek makes the video wait; that wait is not the group
    /// buffering, or the group freezes and seeks again.
    #[test]
    fn a_wait_right_after_the_sessions_own_play_seek_is_not_buffering() {
        let rig = rig();
        rig.session.deliver(GroupUpdate::Play {
            at_server_ms: 100_000,
            position_ms: 10_000,
        });
        rig.env.settle();
        assert!(rig.player.log.borrow().contains(&"seek(10000)".to_owned()));
        rig.session.on_media(MediaEvent::Waiting);
        rig.env.settle();
        assert!(calls_with(&rig, "buffering").is_empty());
    }

    #[test]
    fn a_wait_right_after_a_hard_drift_seek_is_not_buffering() {
        let rig = playing_rig();
        rig.env.now.set(110_000);
        rig.player.position.set(15_000);
        rig.session.check_drift();
        assert_eq!(
            rig.player.log.borrow().last().map(String::as_str),
            Some("seek(20000)")
        );
        rig.session.on_media(MediaEvent::Waiting);
        rig.env.settle();
        assert!(calls_with(&rig, "buffering").is_empty());
        assert_eq!(
            rig.env
                .logged(
                    "pharos: group wait held reason=self_seek since_seek_ms=0 position_ms=20000"
                )
                .len(),
            1
        );
    }

    /// A wait that outlives the self-seek grace with no `canplay` is a real
    /// stall. The player is paused here so the drift check does not seek
    /// again and restart the grace.
    #[test]
    fn a_held_wait_becomes_buffering_once_the_grace_runs_out() {
        let rig = rig();
        rig.session.deliver(GroupUpdate::Play {
            at_server_ms: 100_000,
            position_ms: 10_000,
        });
        rig.env.settle();
        rig.session.on_media(MediaEvent::Waiting);
        rig.env.settle();
        assert!(calls_with(&rig, "buffering").is_empty());

        rig.player.paused.set(true);
        rig.env.now.set(104_999);
        assert!(rig.session.tick(1).now_or_never().is_some());
        rig.env.settle();
        assert!(calls_with(&rig, "buffering").is_empty());

        rig.env.now.set(105_000);
        assert!(rig.session.tick(2).now_or_never().is_some());
        rig.env.settle();
        let sent = calls_with(&rig, "buffering");
        assert_eq!(sent.len(), 1);
        assert!(sent[0].ends_with(",true)"), "got {sent:?}");
        assert_eq!(
            rig.env
                .logged(
                    "pharos: group wait reported reason=grace_expired held_ms=5000 position_ms="
                )
                .len(),
            1
        );
    }

    /// A playing player stuck after a hard drift seek is not sought again
    /// each second, so the grace runs out and the stall is reported.
    #[test]
    fn a_stall_after_a_drift_seek_is_reported_without_reseeking() {
        let rig = playing_rig();
        rig.env.now.set(110_000);
        rig.player.position.set(15_000);
        rig.session.check_drift();
        rig.session.on_media(MediaEvent::Waiting);
        rig.env.settle();
        let seeks = || {
            rig.player
                .log
                .borrow()
                .iter()
                .filter(|l| l.starts_with("seek("))
                .count()
        };
        let before = seeks();

        // Past the report too: a player still waiting is not sought again
        // until it can play.
        for (n, now) in [(1, 112_000), (2, 114_000), (3, 115_000), (4, 125_000)] {
            rig.env.now.set(now);
            assert!(rig.session.tick(n).now_or_never().is_some());
            rig.env.settle();
        }
        assert_eq!(seeks(), before);
        let sent = calls_with(&rig, "buffering");
        assert_eq!(sent.len(), 1);
        assert!(sent[0].ends_with(",true)"), "got {sent:?}");
    }

    #[test]
    fn a_wait_with_no_recent_seek_is_buffering_at_once() {
        let rig = playing_rig();
        rig.env.now.set(200_000);
        rig.session.on_media(MediaEvent::Waiting);
        rig.env.settle();
        assert_eq!(calls_with(&rig, "buffering").len(), 1);
    }

    #[test]
    fn a_held_wait_that_recovers_sends_nothing() {
        let rig = rig();
        rig.session.deliver(GroupUpdate::Play {
            at_server_ms: 100_000,
            position_ms: 10_000,
        });
        rig.env.settle();
        rig.session.on_media(MediaEvent::Waiting);
        rig.session.on_media(MediaEvent::CanPlay);
        rig.env.settle();
        assert_eq!(
            rig.env
                .logged("pharos: group wait released reason=canplay held_ms=0 ")
                .len(),
            1
        );
        rig.env.now.set(105_000);
        assert!(rig.session.tick(1).now_or_never().is_some());
        rig.env.settle();
        assert!(calls_with(&rig, "buffering").is_empty());
        assert!(rig.env.logged("pharos: group wait reported").is_empty());
    }

    /// Playing group at 10 s of server time 100_000; our clock offset is 0.
    fn playing_rig() -> Rig {
        let rig = rig();
        rig.session.deliver(GroupUpdate::Play {
            at_server_ms: 100_000,
            position_ms: 10_000,
        });
        rig.env.settle();
        rig.player.log.borrow_mut().clear();
        rig
    }

    #[test]
    fn small_drift_nudges_the_rate_and_big_drift_hard_seeks() {
        let rig = playing_rig();
        // 10 s later the group is at 20 s; the video is 300 ms behind.
        rig.env.now.set(110_000);
        rig.player.position.set(19_700);
        rig.session.check_drift();
        assert_eq!(*rig.player.log.borrow(), ["rate(1.05)"]);
        // Caught up: back to normal speed.
        rig.player.position.set(20_010);
        rig.session.check_drift();
        assert_eq!(rig.player.log.borrow()[1], "rate(1)");
        // 5 s behind: seek to where the group is.
        rig.player.position.set(15_000);
        rig.session.check_drift();
        assert_eq!(
            rig.player.log.borrow().last().map(String::as_str),
            Some("seek(20000)")
        );
    }

    #[test]
    fn a_fresh_server_command_holds_off_corrections() {
        let rig = playing_rig();
        rig.env.now.set(101_000);
        rig.player.position.set(90_000);
        rig.session.check_drift();
        assert!(rig.player.log.borrow().is_empty());
    }

    /// FR-005. Disarm: remove the `self.env.report(..)` call in `emit` and
    /// every assertion on the reported events goes red.
    #[test]
    fn drift_hard_seeks_and_waits_are_reported() {
        let rig = rig();
        rig.player.paused.set(true);
        rig.player.position.set(0);
        rig.session.deliver(GroupUpdate::Item {
            item: ItemRef::Real(7),
            playlist_item_id: "pl".into(),
            position_ms: 0,
            playing: false,
        });
        rig.session.on_media(MediaEvent::CanPlay);
        rig.env.settle();
        // Ready was sent at 100_000; the play comes 2.5 s later.
        rig.env.now.set(102_500);
        rig.session.deliver(GroupUpdate::Play {
            at_server_ms: 102_500,
            position_ms: 0,
        });
        rig.env.settle();
        {
            let reports = rig.env.reports.borrow();
            assert_eq!(reports.len(), 1);
            assert_eq!(reports[0].kind, ClientEventKind::GroupWait);
            assert_eq!(reports[0].level, ClientLevel::Info);
            assert_eq!(
                reports[0].fields,
                [("group", "g1".to_owned()), ("wait_ms", "2500".to_owned())]
            );
        }
        // 10 s on, the video is 200 ms ahead: sampled and nudged.
        rig.env.now.set(112_500);
        rig.player.paused.set(false);
        rig.player.position.set(10_200);
        rig.session.check_drift();
        rig.session.report_drift();
        // Then the video is 14 s behind: a hard seek, reported.
        rig.env.now.set(118_500);
        rig.player.position.set(2_000);
        rig.session.check_drift();
        assert_eq!(
            rig.env.kinds(),
            [
                ClientEventKind::GroupWait,
                ClientEventKind::GroupDrift,
                ClientEventKind::GroupHardSeek
            ]
        );
        let reports = rig.env.reports.borrow();
        assert_eq!(
            reports[1].fields,
            [("group", "g1".to_owned()), ("drift_ms", "200".to_owned())]
        );
        assert_eq!(reports[1].level, ClientLevel::Info);
        assert_eq!(reports[2].level, ClientLevel::Warn);
        assert_eq!(
            reports[2].fields,
            [
                ("group", "g1".to_owned()),
                ("drift_ms", "-14000".to_owned())
            ]
        );
    }

    fn item_update(position_ms: u64, playing: bool) -> GroupUpdate {
        GroupUpdate::Item {
            item: ItemRef::Real(7),
            playlist_item_id: "p1".into(),
            position_ms,
            playing,
        }
    }

    /// A session joined but not yet past its clock samples, with a player.
    fn unsynced() -> Rig {
        let groups = FakeGroups::with_groups(vec![summary()]);
        let env = TestEnv::default();
        env.now.set(100_000);
        let session = Session::new(groups.clone(), env.clone());
        let player = Rc::new(TestPlayer::default());
        session.set_player(Some(player.clone()));
        assert!(matches!(session.join("g1").now_or_never(), Some(Ok(_))));
        Rig {
            session,
            groups,
            env,
            player,
        }
    }

    #[test]
    fn an_item_before_the_first_clock_sample_is_kept_and_read_once_it_arrives() {
        let mut state = GroupState::default();
        let _ = state.apply(item_update(5_000, true), 100_000);
        assert_eq!(state.expected(104_000), None);
        let first = state.push_clock(ClockSample {
            t1: 0,
            t2: 1_000,
            t3: 1_000,
            t4: 0,
        });
        assert!(first);
        // The item landed at 100 000 here, which is 101 000 there.
        assert_eq!(state.expected(104_000), Some((9_000, true)));
    }

    #[test]
    fn a_late_joiner_is_caught_up_when_the_first_sample_lands() {
        let rig = unsynced();
        rig.session.deliver(item_update(5_000, true));
        assert!(rig.player.log.borrow().is_empty());
        rig.env.now.set(103_000);
        rig.env.settle();
        assert_eq!(*rig.player.log.borrow(), ["seek(8000)", "play"]);
    }

    #[test]
    fn a_seek_with_no_anchor_does_not_claim_the_group_is_paused() {
        let mut state = GroupState::default();
        let actions = state.apply(
            GroupUpdate::Seek {
                at_server_ms: 0,
                position_ms: 3_000,
            },
            100_000,
        );
        assert_eq!(state.anchor, None);
        assert_eq!(
            actions,
            [Action::Seek {
                delay_ms: 0,
                position_ms: 3_000
            }]
        );
    }

    #[test]
    fn a_seek_after_an_unresolved_item_keeps_playing() {
        let mut state = GroupState::default();
        let _ = state.apply(item_update(5_000, true), 100_000);
        let _ = state.apply(
            GroupUpdate::Seek {
                at_server_ms: 100_500,
                position_ms: 20_000,
            },
            100_500,
        );
        assert_eq!(state.anchor.map(|a| a.playing), Some(true));
    }

    #[test]
    fn a_player_arriving_in_a_paused_group_is_moved_to_its_position() {
        let rig = rig();
        rig.session.deliver(GroupUpdate::Pause {
            at_server_ms: 100_000,
            position_ms: 42_000,
        });
        let late = Rc::new(TestPlayer::default());
        rig.session.set_player(Some(late.clone()));
        assert_eq!(*late.log.borrow(), ["seek(42000)"]);
    }

    #[test]
    fn updates_from_an_older_membership_are_dropped() {
        let rig = rig();
        let old = rig.groups.sinks().remove(0);
        let made = rig.session.create(ItemRef::Real(1), 0);
        assert!(matches!(made.now_or_never(), Some(Ok(_))));
        rig.env.settle();
        old(GroupUpdate::Ended);
        old(GroupUpdate::Member(member("ghost", false)));
        let view = rig.session.view();
        assert!(view.active);
        assert_eq!(view.ended, None);
        assert!(!view.members.contains(&member("ghost", false)));
        let new = rig.groups.sinks().remove(0);
        new(GroupUpdate::Member(member("bo", false)));
        assert!(rig.session.view().members.contains(&member("bo", false)));
    }

    #[test]
    fn a_replacement_session_keeps_the_player_and_the_watched_item() {
        let rig = rig();
        rig.session.set_watching(Some(ItemRef::Real(7)));
        let next = Session::new(FakeGroups::with_groups(vec![summary()]), rig.env.clone());
        next.adopt(&rig.session);
        assert!(next.inner.borrow().player.is_some());
        assert_eq!(next.inner.borrow().state.watching, Some(ItemRef::Real(7)));
        assert!(matches!(next.join("g1").now_or_never(), Some(Ok(_))));
        assert_eq!(next.inner.borrow().state.watching, Some(ItemRef::Real(7)));
        assert!(next.inner.borrow().player.is_some());
    }
}
