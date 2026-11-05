//! The watch-together group store (014): one active membership, the clock
//! sync, keep-alive and drift loops that run while it lasts, and what each
//! server update and local control means for the player.
//!
//! No Dioxus and no browser in here. [`Env`] is the clock, timer, spawner and
//! event sink; [`GroupPlayer`] is the `<video>`; [`GroupClient`] is the
//! network. The app supplies browser versions, the tests supply fakes.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::{Rc, Weak};

use futures_util::future::{select, Either, LocalBoxFuture};
use futures_util::FutureExt as _;
use pharos_client::api::{ClientEvent, ClientEventKind, ClientLevel, ItemRef};

use super::activity::{self, ActivityLog, Event, TrackKind};
use super::playback::MediaEvent;
use super::seen::{self, MemberSeen};
use super::together::{
    expected_position, ClockEstimator, Correction, DriftTracker, GroupClient, GroupError, GroupId,
    GroupSink, GroupSummary, GroupUpdate, Member, CLOCK_WINDOW, HOLD_BELOW_MS, SELF_SEEK_GRACE_MS,
};
use super::track_follow::{
    follows_group, route_pick, GroupTracksView, PickRoute, TrackChoice, TrackPicks,
};

/// Keep-alive cadence (T83's ghost prune is 150 s).
pub const PING_EVERY_S: u64 = 30;

/// A group command whose RPC has not answered after this long is given up on,
/// so one that never answers cannot hold back every later one.
pub const COMMAND_TIMEOUT_MS: u32 = 10_000;
/// Clock re-sync cadence, and the `groupDrift` sample cadence.
pub const SAMPLE_EVERY_S: u64 = 10;

/// What the group shows the viewer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupView {
    /// In a group now.
    pub active: bool,
    /// The group's id, for its invite link.
    pub id: Option<String>,
    pub members: Vec<Member>,
    /// This member is on their own tracks ("Just me").
    pub own_tracks: bool,
    /// Who did what lately, newest last.
    pub log: ActivityLog,
    /// Why the group is over, once it is.
    pub ended: Option<String>,
    /// Counts the group's updates, so a screen can show what each one adds
    /// (the toast) without comparing the rest.
    pub activity: u64,
    /// Counts the group's moves to another item. What members have seen
    /// changes with these and with the roster, not with every update.
    pub item_changes: u64,
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
    /// A decision the group took, for the console and the field logs.
    fn info(&self, message: &str);
    fn warn(&self, message: &str);
}

/// The `<video>` the group drives.
pub trait GroupPlayer {
    fn position_ms(&self) -> u64;
    fn paused(&self) -> bool;
    fn play(&self);
    fn pause(&self);
    fn seek_ms(&self, ms: u64);
    fn set_rate(&self, rate: f64);
    /// What the viewer calls the stream `index` of this kind, for the activity lines.
    fn track_label(&self, kind: TrackKind, index: u32) -> Option<String>;
    /// The audio and subtitle stream indexes of the item playing.
    fn stream_indexes(&self) -> (Vec<u32>, Vec<u32>);
    /// Switch the player's audio and subtitle, as its own menus do.
    fn apply_tracks(&self, picks: TrackPicks);
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
    /// The group's audio and subtitle choice.
    pub tracks: GroupTracksView,
    /// This member is on their own tracks ("Just me"), not the group's.
    pub own_tracks: bool,
    /// This member's own id in `members`; empty until a `Joined` names it.
    me: String,
    /// Who did what, as the updates came.
    pub log: ActivityLog,
    /// The queue entry the group is on, which readiness reports carry.
    pub playlist_item_id: String,
    pub ended: Option<String>,
    /// The group's updates so far.
    activity: u64,
    /// The group's moves to another item so far.
    item_changes: u64,
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

    /// Learn which roster entry is this member (`you`, empty when the server
    /// did not say) and take its own-tracks flag from the roster, which is
    /// what a rejoining member's server remembers.
    fn know_self(&mut self, you: &str) {
        if you.is_empty() {
            return;
        }
        self.me = you.to_owned();
        if let Some(m) = self.members.iter().find(|m| m.id == you) {
            self.own_tracks = m.own_tracks;
        }
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

    /// A member's display name, or `None` for the group (no id, or one the
    /// roster does not know).
    fn name_of(&self, id: Option<&str>) -> Option<String> {
        let id = id.filter(|id| !id.is_empty())?;
        self.members
            .iter()
            .find(|m| m.id == id)
            .map(|m| m.name.clone())
    }

    /// The activity events an update stands for, read against the group as it
    /// is before the update lands.
    fn events(
        &self,
        update: &GroupUpdate,
        label: &dyn Fn(TrackKind, u32) -> Option<String>,
    ) -> Vec<Event> {
        let by_member = |by: &Option<String>, action| Event {
            by: self.name_of(by.as_deref()),
            action,
        };
        let catch_up =
            |by: &Option<String>| by.as_deref() == Some(pharos_client::group::CATCH_UP_BY);
        match update {
            GroupUpdate::Play { by, .. }
            | GroupUpdate::Pause { by, .. }
            | GroupUpdate::Seek { by, .. }
                if catch_up(by) =>
            {
                Vec::new()
            }
            GroupUpdate::Play { by, .. } => vec![by_member(by, activity::Action::Play)],
            GroupUpdate::Pause {
                by, position_ms, ..
            } => vec![by_member(
                by,
                activity::Action::Pause {
                    position_ms: *position_ms,
                },
            )],
            GroupUpdate::Seek {
                by, position_ms, ..
            } => vec![by_member(
                by,
                activity::Action::Seek {
                    position_ms: *position_ms,
                },
            )],
            GroupUpdate::Member(m) => match self.members.iter().find(|x| x.id == m.id) {
                None => vec![Event {
                    by: Some(m.name.clone()),
                    action: activity::Action::Joined,
                }],
                Some(before) => {
                    let mut events = Vec::new();
                    if m.own_tracks != before.own_tracks {
                        events.push(Event {
                            by: Some(m.name.clone()),
                            action: activity::Action::OwnTracks { on: m.own_tracks },
                        });
                    }
                    if m.buffering && !before.buffering {
                        let names = self
                            .members
                            .iter()
                            .filter(|x| if x.id == m.id { true } else { x.buffering })
                            .map(|x| x.name.clone())
                            .collect();
                        events.push(Event {
                            by: None,
                            action: activity::Action::Waiting { names },
                        });
                    }
                    events
                }
            },
            GroupUpdate::Left(id) => self
                .name_of(Some(id))
                .map(|name| Event {
                    by: Some(name),
                    action: activity::Action::Left,
                })
                .into_iter()
                .collect(),
            GroupUpdate::Tracks { tracks, by } => {
                let who = self.name_of(by.as_deref());
                let mut events = Vec::new();
                let mut push = |kind, now: TrackChoice, before: TrackChoice| {
                    if now == before {
                        return;
                    }
                    let label = match now {
                        TrackChoice::Unset => return,
                        TrackChoice::Off => None,
                        TrackChoice::Index(i) => {
                            Some(label(kind, i).unwrap_or_else(|| format!("track {i}")))
                        }
                    };
                    events.push(Event {
                        by: who.clone(),
                        action: activity::Action::Tracks { kind, label },
                    });
                };
                push(TrackKind::Audio, tracks.audio, self.tracks.audio);
                push(TrackKind::Subtitles, tracks.subtitle, self.tracks.subtitle);
                events
            }
            GroupUpdate::Joined(_) | GroupUpdate::Item { .. } | GroupUpdate::Ended => Vec::new(),
        }
    }

    /// Take an update; the actions for the player and screens.
    pub fn apply(&mut self, update: GroupUpdate, now_ms: i64) -> Vec<Action> {
        self.apply_labelled(update, now_ms, &|_, _| None)
    }

    /// [`Self::apply`], naming the tracks in the activity lines with `label`.
    pub fn apply_labelled(
        &mut self,
        update: GroupUpdate,
        now_ms: i64,
        label: &dyn Fn(TrackKind, u32) -> Option<String>,
    ) -> Vec<Action> {
        let at = u64::try_from(now_ms).unwrap_or(0);
        for event in self.events(&update, label) {
            self.log.push(&event, at);
        }
        self.activity = self.activity.wrapping_add(1);
        match update {
            GroupUpdate::Joined(g) => {
                self.id = Some(g.id);
                self.members = g.members;
                self.tracks = g.tracks;
                self.know_self(&g.you);
                Vec::new()
            }
            GroupUpdate::Member(m) => {
                if !self.me.is_empty() && m.id == self.me {
                    self.own_tracks = m.own_tracks;
                }
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
                // The previous item's picks name its streams, not this
                // one's; the server's cleared `Tracks` follows. A joiner's
                // first item (no previous one) keeps what `Joined` brought.
                if !self.playlist_item_id.is_empty() && self.playlist_item_id != playlist_item_id {
                    self.tracks = GroupTracksView::default();
                }
                self.playlist_item_id = playlist_item_id;
                self.item_changes = self.item_changes.wrapping_add(1);
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
                ..
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
                ..
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
                ..
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
            GroupUpdate::Tracks { tracks, .. } => {
                self.tracks = tracks;
                Vec::new()
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
            id: self.id.clone(),
            members: self.members.clone(),
            own_tracks: self.own_tracks,
            log: self.log.clone(),
            ended: self.ended.clone(),
            activity: self.activity,
            item_changes: self.item_changes,
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
    /// Group commands not yet sent; one task sends them one at a time, so the
    /// server sees them in the order the viewer caused them.
    commands: VecDeque<(String, LocalBoxFuture<'static, Result<(), GroupError>>)>,
    /// A task is working through `commands`.
    draining: bool,
    /// That task has a command's RPC out and unanswered.
    in_flight: bool,
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
        // The server answers at most MAX_IDS at a time; ask in chunks so every
        // id gets its marks.
        let asks = ids
            .chunks(seen::MAX_IDS)
            .map(|chunk| self.client.seen(chunk.to_vec()));
        let mut all = Vec::new();
        for answer in futures_util::future::try_join_all(asks).await? {
            seen::merge(&mut all, answer);
        }
        Ok(all)
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
        // The drain task outlives a membership; its queue must too, or a
        // second task would start beside it and the order would be lost.
        let commands = std::mem::take(&mut inner.commands);
        let (draining, in_flight) = (inner.draining, inner.in_flight);
        *inner = Inner {
            epoch,
            player,
            rate: 1.0,
            commands,
            draining,
            in_flight,
            ..Inner::default()
        };
        inner.state.watching = watching.or(shown);
        epoch
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
                inner.state.tracks = summary.tracks;
                inner.state.know_self(&summary.you);
            }
            inner.epoch
        };
        self.publish();
        self.follow_group_tracks();
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
        self.follow_group_tracks();
    }

    /// Put the player on the group's audio and subtitle, unless this member is
    /// on their own or the item has no such stream.
    fn follow_group_tracks(&self) {
        let (player, own, tracks) = {
            let inner = self.inner.borrow();
            if !inner.active {
                return;
            }
            (
                inner.player.clone(),
                inner.state.own_tracks,
                inner.state.tracks,
            )
        };
        let Some(player) = player else {
            return;
        };
        let (audio, subtitles) = player.stream_indexes();
        let picks = follows_group(own, tracks, &audio, &subtitles);
        let (verdict, reason) = match (&picks, own) {
            (Some(_), _) => ("apply", "following the group"),
            (None, true) => ("skip", "own tracks"),
            (None, false) => ("skip", "nothing the item has"),
        };
        self.env.info(&format!(
            "pharos: group tracks follow verdict={verdict} reason={reason:?} own_tracks={own} group_audio={:?} group_subtitle={:?} item_audio={audio:?} item_subtitles={subtitles:?} picks={picks:?}",
            tracks.audio, tracks.subtitle
        ));
        if let Some(picks) = picks {
            player.apply_tracks(picks);
        }
    }

    /// A menu pick of an audio stream. `true` when it went to the group, which
    /// applies it to every following player, this one included; `false` when
    /// the caller changes only its own player.
    pub fn pick_audio(&self, index: u32) -> bool {
        self.pick(TrackChoice::Index(index), TrackChoice::Unset)
    }

    /// A menu pick of a subtitle stream, or `None` for off; see [`Self::pick_audio`].
    pub fn pick_subtitle(&self, index: Option<u32>) -> bool {
        self.pick(
            TrackChoice::Unset,
            index.map_or(TrackChoice::Off, TrackChoice::Index),
        )
    }

    fn pick(&self, audio: TrackChoice, subtitle: TrackChoice) -> bool {
        let (active, own, playlist_item_id, epoch) = {
            let inner = self.inner.borrow();
            (
                inner.active,
                inner.state.own_tracks,
                inner.state.playlist_item_id.clone(),
                inner.epoch,
            )
        };
        let route = route_pick(active, own);
        let reason = match (active, own) {
            (false, _) => "not in a group",
            (true, true) => "own tracks",
            (true, false) => "following the group",
        };
        self.env.info(&format!(
            "pharos: group tracks pick route={route:?} reason={reason:?} audio={audio:?} subtitle={subtitle:?} item={playlist_item_id:?}"
        ));
        if route == PickRoute::ApplyLocally {
            return false;
        }
        if let Some(me) = self.me() {
            let client = self.client.clone();
            self.env.spawn(Box::pin(async move {
                let Err(e) = client.set_tracks(audio, subtitle, playlist_item_id).await else {
                    return;
                };
                me.env
                    .warn(&format!("pharos: group command failed: {}", e.0));
                me.apply_pick_here(audio, subtitle, epoch);
            }));
        }
        true
    }

    /// The group did not take a pick (the call failed), so change this player
    /// alone and say so. The server drops a stale, unchanged or non-member pick
    /// without an answer; none of those needs a local change.
    fn apply_pick_here(&self, audio: TrackChoice, subtitle: TrackChoice, epoch: u64) {
        let picks = TrackPicks {
            audio: match audio {
                TrackChoice::Index(i) => Some(i),
                _ => None,
            },
            subtitle: match subtitle {
                TrackChoice::Off => Some(None),
                TrackChoice::Index(i) => Some(Some(i)),
                _ => None,
            },
        };
        let player = {
            let mut inner = self.inner.borrow_mut();
            if !inner.active || inner.epoch != epoch {
                return;
            }
            let at = u64::try_from(self.env.now_ms()).unwrap_or(0);
            let kinds = [
                (picks.audio.is_some(), TrackKind::Audio),
                (picks.subtitle.is_some(), TrackKind::Subtitles),
            ];
            for kind in kinds.into_iter().filter_map(|(set, k)| set.then_some(k)) {
                inner.state.log.push(
                    &Event {
                        by: None,
                        action: activity::Action::PickNotShared { kind },
                    },
                    at,
                );
            }
            inner.state.activity = inner.state.activity.wrapping_add(1);
            inner.player.clone()
        };
        self.env.warn(&format!(
            "pharos: group refused the track pick, applied here audio={:?} subtitle={:?}",
            picks.audio, picks.subtitle
        ));
        self.publish();
        if let Some(player) = player {
            player.apply_tracks(picks);
        }
    }

    /// "Follow group" (`true`) or "Just me" (`false`). Following puts the
    /// player on the group's tracks at once. Does nothing outside a group.
    pub fn follow_tracks(&self, follow: bool) {
        let (before, epoch) = {
            let mut inner = self.inner.borrow_mut();
            if !inner.active {
                return;
            }
            let before = inner.state.own_tracks;
            inner.state.own_tracks = !follow;
            (before, inner.epoch)
        };
        self.publish();
        if let Some(me) = self.me() {
            let client = self.client.clone();
            self.env.spawn(Box::pin(async move {
                let Err(e) = client.follow_tracks(follow).await else {
                    return;
                };
                me.env
                    .warn(&format!("pharos: group command failed: {}", e.0));
                // The server never heard it, so the flag it keeps is the old
                // one; unless the membership has moved on, show that again.
                let reverted = {
                    let mut inner = me.inner.borrow_mut();
                    let current = inner.active && inner.epoch == epoch;
                    if current {
                        inner.state.own_tracks = before;
                    }
                    current
                };
                if reverted {
                    me.publish();
                }
            }));
        }
        if follow {
            self.follow_group_tracks();
        }
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
        let tracks_changed = matches!(update, GroupUpdate::Joined(_) | GroupUpdate::Tracks { .. });
        let (actions, wait) = {
            let mut inner = self.inner.borrow_mut();
            if inner.leaving || inner.epoch != epoch {
                return;
            }
            let player = inner.player.clone();
            let label = |kind, index| player.as_ref().and_then(|p| p.track_label(kind, index));
            let actions = inner.state.apply_labelled(update, now, &label);
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
        if tracks_changed {
            self.follow_group_tracks();
        }
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
        let kind = if paused { "unpause" } else { "pause" };
        self.command(kind.to_owned(), async move {
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
        self.command(format!("seek({position_ms})"), async move {
            client.seek(position_ms).await
        });
        true
    }

    /// Sends a group command after every earlier one has been answered: two
    /// RPCs in flight can reach the server, and the group, in reverse order
    /// (buffering end before its start leaves a member buffering for good).
    /// `kind` names the command and its key argument, `queued` how many commands
    /// (the one in flight included) are ahead of it, for the diagnostics.
    fn command(
        &self,
        kind: String,
        fut: impl std::future::Future<Output = Result<(), GroupError>> + 'static,
    ) {
        let Some(me) = self.me() else {
            return;
        };
        let (queued, start) = {
            let mut inner = self.inner.borrow_mut();
            let queued = inner.commands.len() + usize::from(inner.in_flight);
            inner.commands.push_back((kind.clone(), fut.boxed_local()));
            let start = !inner.draining;
            inner.draining = true;
            (queued, start)
        };
        self.env.info(&format!(
            "pharos: group command sent kind={kind} queued={queued}"
        ));
        if !start {
            return;
        }
        self.env.spawn(Box::pin(async move {
            loop {
                // The borrow ends before the await; `draining` clears in the
                // same borrow that finds the queue empty.
                let next = {
                    let mut inner = me.inner.borrow_mut();
                    let next = inner.commands.pop_front();
                    inner.in_flight = next.is_some();
                    if next.is_none() {
                        inner.draining = false;
                    }
                    next
                };
                let Some((kind, next)) = next else {
                    return;
                };
                match select(next, me.env.sleep(COMMAND_TIMEOUT_MS)).await {
                    Either::Left((Err(e), _)) => me
                        .env
                        .warn(&format!("pharos: group command failed: {}", e.0)),
                    Either::Left((Ok(()), _)) => {}
                    Either::Right(_) => me.env.warn(&format!(
                        "pharos: group command timed out kind={kind} after_ms={COMMAND_TIMEOUT_MS}"
                    )),
                }
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
        self.command(format!("buffering({buffering})"), async move {
            client.buffering(playlist, position_ms, buffering).await
        });
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
            self.command(format!("ready(playing={playing})"), async move {
                client.ready(playlist, position, playing).await
            });
        }
    }
}

/// The `<video>` the stage plays and the item it shows.
struct Video {
    item: ItemRef,
    player: Rc<dyn GroupPlayer>,
}

/// Holds the stage's video apart from any session, so a session made after
/// the video mounted (a join from another page) still drives it, and one
/// replaced on a reconnect keeps it.
pub struct Seat<C, E> {
    session: RefCell<Option<Rc<Session<C, E>>>>,
    video: RefCell<Option<Video>>,
    log: Box<dyn Fn(&str)>,
}

impl<C: GroupClient + Clone + 'static, E: Env + 'static> Seat<C, E> {
    pub fn new(log: impl Fn(&str) + 'static) -> Self {
        Self {
            session: RefCell::default(),
            video: RefCell::default(),
            log: Box::new(log),
        }
    }

    /// The session the stage's video is driven by, if one was made.
    pub fn current(&self) -> Option<Rc<Session<C, E>>> {
        self.session.borrow().clone()
    }

    /// A video mounted on the stage showing `item`; the session, if any,
    /// drives it from here.
    pub fn attach(&self, item: ItemRef, player: Rc<dyn GroupPlayer>) {
        let session = self.current();
        *self.video.borrow_mut() = Some(Video {
            item,
            player: Rc::clone(&player),
        });
        if let Some(session) = &session {
            session.set_watching(Some(item));
            session.set_player(Some(player));
        }
        (self.log)(&format!(
            "pharos: group player attached reason=stage_mounted item={item} session={}",
            session
                .as_ref()
                .map_or("none", |s| if s.active() { "in_group" } else { "idle" })
        ));
    }

    /// The video `player` went from the stage. A newer video that already
    /// took its place stays.
    pub fn detach(&self, player: &Rc<dyn GroupPlayer>) {
        let gone = {
            let mut video = self.video.borrow_mut();
            match video.as_ref() {
                Some(v) if std::ptr::addr_eq(Rc::as_ptr(&v.player), Rc::as_ptr(player)) => {
                    video.take()
                }
                _ => None,
            }
        };
        let Some(gone) = gone else {
            (self.log)("pharos: group player detach skipped reason=superseded");
            return;
        };
        if let Some(session) = self.current() {
            session.set_player(None);
            session.set_watching(None);
        }
        (self.log)(&format!(
            "pharos: group player detached reason=video_gone item={}",
            gone.item
        ));
    }

    /// The session to create or join a group with: the one in a group, else
    /// a new one from `make`.
    pub fn session(&self, make: impl FnOnce() -> Rc<Session<C, E>>) -> Rc<Session<C, E>> {
        let mut current = self.session.borrow_mut();
        if let Some(session) = current.as_ref().filter(|s| s.active()) {
            return Rc::clone(session);
        }
        let session = make();
        *current = Some(Rc::clone(&session));
        drop(current);
        if let Some(video) = self.video.borrow().as_ref() {
            session.set_watching(Some(video.item));
            session.set_player(Some(Rc::clone(&video.player)));
            (self.log)(&format!(
                "pharos: group player attached reason=session_created item={}",
                video.item
            ));
        }
        session
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::native::together::{ClockSample, FakeGroups};

    #[derive(Clone, Default)]
    struct TestEnv {
        now: Rc<Cell<i64>>,
        reports: Rc<RefCell<Vec<ClientEvent>>>,
        ui: Rc<RefCell<Vec<Ui>>>,
        tasks: Rc<RefCell<Vec<LocalBoxFuture<'static, ()>>>>,
        logs: Rc<RefCell<Vec<String>>>,
        /// Whether the command timeout has run out; it never does until a
        /// test says so, so a held RPC stays held.
        timeout_elapsed: Rc<Cell<bool>>,
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
            } else if ms == COMMAND_TIMEOUT_MS {
                let elapsed = self.timeout_elapsed.clone();
                std::future::poll_fn(move |_| {
                    if elapsed.get() {
                        std::task::Poll::Ready(())
                    } else {
                        std::task::Poll::Pending
                    }
                })
                .boxed_local()
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
        fn info(&self, message: &str) {
            self.logs.borrow_mut().push(message.to_owned());
        }
        fn warn(&self, message: &str) {
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
        /// Audio and subtitle stream indexes of the item.
        streams: RefCell<(Vec<u32>, Vec<u32>)>,
        applied: RefCell<Vec<TrackPicks>>,
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
        fn track_label(&self, kind: TrackKind, index: u32) -> Option<String> {
            Some(format!("{kind:?} {index}"))
        }
        fn stream_indexes(&self) -> (Vec<u32>, Vec<u32>) {
            self.streams.borrow().clone()
        }
        fn apply_tracks(&self, picks: TrackPicks) {
            self.applied.borrow_mut().push(picks);
        }
    }

    fn member(id: &str, buffering: bool) -> Member {
        Member {
            id: id.into(),
            name: id.to_uppercase(),
            buffering,
            own_tracks: false,
        }
    }

    fn summary() -> GroupSummary {
        GroupSummary {
            id: "g1".into(),
            name: "Friday".into(),
            item: None,
            members: vec![member("me", false)],
            tracks: GroupTracksView::default(),
            you: String::new(),
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
        let many: Vec<ItemRef> = (0..250).map(ItemRef::Real).collect();
        let _ = rig.session.seen(many).now_or_never();
        assert_eq!(
            calls_with(&rig, "seen("),
            vec!["seen(200)".to_owned(), "seen(50)".to_owned()]
        );
        let asked = rig.session.seen(ids.clone()).now_or_never();
        assert!(matches!(asked, Some(Ok(_))));
        assert_eq!(
            calls_with(&rig, "seen(").last().map(String::as_str),
            Some("seen(2)")
        );
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
                    position_ms: 5_000,
                    by: None
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
                    position_ms: 20_000,
                    by: None
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
                    position_ms: 21_000,
                    by: None
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
            by: None,
        });
        rig.env.settle();
        assert_eq!(*rig.player.log.borrow(), ["seek(5000)", "play"]);
        rig.session.deliver(GroupUpdate::Pause {
            at_server_ms: 100_000,
            position_ms: 9_000,
            by: None,
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

    /// A server that answers slowly must not reorder what the viewer did:
    /// waiting then canplay reaches it as buffering true, then false.
    #[test]
    fn group_commands_reach_the_server_in_the_order_they_were_sent() {
        let rig = rig();
        rig.session.deliver(GroupUpdate::Item {
            item: ItemRef::Real(7),
            playlist_item_id: "pl-7".into(),
            position_ms: 3_000,
            playing: false,
        });
        rig.player.position.set(3_000);
        rig.player.paused.set(true);
        rig.session.on_media(MediaEvent::CanPlay);
        rig.env.settle();

        rig.groups.hold_buffering();
        rig.session.on_media(MediaEvent::Waiting);
        rig.session.on_media(MediaEvent::CanPlay);
        let mut live: Vec<LocalBoxFuture<'static, ()>> = Vec::new();
        pump(&rig.env, &mut live);
        // Releasing the second call first simulates a server that answers out
        // of order. Under the fix the second is not on the wire yet, so this
        // is a no-op; the first goes through, then the second may start.
        rig.groups.release_buffering(1);
        pump(&rig.env, &mut live);
        rig.groups.release_buffering(0);
        pump(&rig.env, &mut live);
        rig.groups.release_buffering(1);
        pump(&rig.env, &mut live);
        assert_eq!(
            calls_with(&rig, "buffering"),
            ["buffering(pl-7,3000,true)", "buffering(pl-7,3000,false)"]
        );
        let sent = rig.env.logged("pharos: group command sent kind=buffering");
        assert_eq!(
            sent,
            [
                "pharos: group command sent kind=buffering(true) queued=0",
                "pharos: group command sent kind=buffering(false) queued=1",
            ]
        );
    }

    /// An RPC that never answers is given up on, so it cannot hold back the
    /// commands after it.
    #[test]
    fn a_command_that_never_answers_does_not_block_the_next() {
        let rig = rig();
        rig.groups.hold_buffering();
        rig.session.on_media(MediaEvent::Waiting);
        assert!(rig.session.seek(5_000));
        let mut live: Vec<LocalBoxFuture<'static, ()>> = Vec::new();
        pump(&rig.env, &mut live);
        assert!(calls_with(&rig, "seek").is_empty(), "held behind buffering");
        rig.env.timeout_elapsed.set(true);
        pump(&rig.env, &mut live);
        assert_eq!(calls_with(&rig, "seek"), ["seek(5000)"]);
        assert_eq!(
            rig.env
                .logged("pharos: group command timed out kind=buffering(true)"),
            ["pharos: group command timed out kind=buffering(true) after_ms=10000"]
        );
    }

    /// Joining again resets the membership, not the queue a running drain
    /// task works through.
    #[test]
    fn a_new_membership_keeps_the_command_queue_and_its_drain() {
        let rig = rig();
        rig.groups.hold_buffering();
        rig.session.on_media(MediaEvent::Waiting);
        let mut live: Vec<LocalBoxFuture<'static, ()>> = Vec::new();
        pump(&rig.env, &mut live);
        assert!(rig.session.inner.borrow().in_flight);
        let _ = rig.session.join("g1").now_or_never();
        let inner = rig.session.inner.borrow();
        assert!(inner.draining && inner.in_flight);
    }

    /// Polls the tasks the session spawned, keeping the parked ones, until a
    /// pass changes nothing.
    fn pump(env: &TestEnv, live: &mut Vec<LocalBoxFuture<'static, ()>>) {
        loop {
            live.extend(env.tasks.borrow_mut().drain(..));
            let before = live.len();
            live.retain_mut(|t| t.now_or_never().is_none());
            if live.len() == before && env.tasks.borrow().is_empty() {
                return;
            }
        }
    }

    /// The session's own seek makes the video wait; that wait is not the group
    /// buffering, or the group freezes and seeks again.
    #[test]
    fn a_wait_right_after_the_sessions_own_play_seek_is_not_buffering() {
        let rig = rig();
        rig.session.deliver(GroupUpdate::Play {
            at_server_ms: 100_000,
            position_ms: 10_000,
            by: None,
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
            by: None,
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
            by: None,
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
            by: None,
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
            by: None,
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
    fn a_new_item_clears_the_previous_items_picks_but_a_first_item_keeps_a_joiners() {
        let mut state = GroupState::default();
        let joined = GroupSummary {
            tracks: groups_tracks(),
            ..summary()
        };
        let _ = state.apply(GroupUpdate::Joined(joined), 0);
        let _ = state.apply(item_update(0, true), 0);
        assert_eq!(state.tracks, groups_tracks(), "first item");
        let _ = state.apply(item_update(0, true), 0);
        assert_eq!(state.tracks, groups_tracks(), "same item");
        let _ = state.apply(
            GroupUpdate::Item {
                item: ItemRef::Real(8),
                playlist_item_id: "p2".into(),
                position_ms: 0,
                playing: true,
            },
            0,
        );
        assert_eq!(state.tracks, GroupTracksView::default());
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
                by: None,
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
                by: None,
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
            by: None,
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

    type TestSeat = Seat<FakeGroups, TestEnv>;

    fn seat() -> (TestSeat, Rc<RefCell<Vec<String>>>) {
        let logged = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&logged);
        (
            Seat::new(move |m: &str| sink.borrow_mut().push(m.to_owned())),
            logged,
        )
    }

    fn new_session(env: &TestEnv) -> Rc<Session<FakeGroups, TestEnv>> {
        Session::new(FakeGroups::with_groups(vec![summary()]), env.clone())
    }

    fn logged_with(logged: &Rc<RefCell<Vec<String>>>, prefix: &str) -> bool {
        logged.borrow().iter().any(|m| m.starts_with(prefix))
    }

    #[test]
    fn a_video_on_the_stage_before_the_join_is_driven_once_the_group_is_joined() {
        let (seat, logged) = seat();
        let env = TestEnv::default();
        env.now.set(100_000);
        let video = Rc::new(TestPlayer::default());
        seat.attach(ItemRef::Real(7), video.clone());
        let session = seat.session(|| new_session(&env));
        assert!(matches!(session.join("g1").now_or_never(), Some(Ok(_))));
        env.settle();
        session.deliver(GroupUpdate::Pause {
            at_server_ms: 100_000,
            position_ms: 42_000,
            by: None,
        });
        env.settle();
        assert!(video.log.borrow().contains(&"seek(42000)".to_owned()));
        assert_eq!(
            session.inner.borrow().state.watching,
            Some(ItemRef::Real(7))
        );
        assert!(logged_with(
            &logged,
            "pharos: group player attached reason=session_created"
        ));
    }

    /// A group started from the player (V479): the video already playing is
    /// the group's from the start, at its own place, and keeps playing once
    /// the group plays; the view names the group for its invite link.
    #[test]
    fn a_group_started_mid_watch_drives_the_video_already_playing() {
        let (seat, _) = seat();
        let env = TestEnv::default();
        env.now.set(100_000);
        let video = Rc::new(TestPlayer::default());
        video.position.set(42_000);
        seat.attach(ItemRef::Real(7), video.clone());
        let session = seat.session(|| new_session(&env));
        let made = session.create(ItemRef::Real(7), video.position_ms());
        assert!(matches!(made.now_or_never(), Some(Ok(_))));
        env.settle();
        let groups = session.client.calls();
        assert!(groups.contains(&"create(7,42000)".to_owned()), "{groups:?}");
        let view = session.view();
        assert!(view.active);
        assert_eq!(view.id.as_deref(), Some("g2"));
        let sink = session.client.sinks().remove(0);
        sink(GroupUpdate::Item {
            item: ItemRef::Real(7),
            playlist_item_id: "p1".into(),
            position_ms: 42_000,
            playing: true,
        });
        sink(GroupUpdate::Play {
            at_server_ms: 100_000,
            position_ms: 42_000,
            by: None,
        });
        env.settle();
        assert!(!video.paused(), "{:?}", video.log.borrow());
        assert!(
            video
                .log
                .borrow()
                .iter()
                .all(|l| !l.starts_with("seek(") || l == "seek(42000)"),
            "only the group's own place: {:?}",
            video.log.borrow()
        );
        assert_eq!(
            session.inner.borrow().state.watching,
            Some(ItemRef::Real(7))
        );
    }

    #[test]
    fn a_video_arriving_after_the_session_is_attached_and_caught_up() {
        let (seat, logged) = seat();
        let env = TestEnv::default();
        env.now.set(100_000);
        let session = seat.session(|| new_session(&env));
        assert!(matches!(session.join("g1").now_or_never(), Some(Ok(_))));
        env.settle();
        session.deliver(GroupUpdate::Pause {
            at_server_ms: 100_000,
            position_ms: 42_000,
            by: None,
        });
        let video = Rc::new(TestPlayer::default());
        seat.attach(ItemRef::Real(7), video.clone());
        assert_eq!(*video.log.borrow(), ["seek(42000)"]);
        assert!(logged_with(
            &logged,
            "pharos: group player attached reason=stage_mounted"
        ));
    }

    #[test]
    fn a_session_replacing_an_ended_one_keeps_the_video() {
        let (seat, _) = seat();
        let env = TestEnv::default();
        let video = Rc::new(TestPlayer::default());
        seat.attach(ItemRef::Real(7), video);
        let first = seat.session(|| new_session(&env));
        let second = seat.session(|| new_session(&env));
        assert!(!Rc::ptr_eq(&first, &second));
        assert!(second.inner.borrow().player.is_some());
        assert_eq!(second.inner.borrow().state.watching, Some(ItemRef::Real(7)));
    }

    #[test]
    fn the_video_going_detaches_the_group_and_says_why() {
        let (seat, logged) = seat();
        let env = TestEnv::default();
        let video: Rc<dyn GroupPlayer> = Rc::new(TestPlayer::default());
        seat.attach(ItemRef::Real(7), video.clone());
        let session = seat.session(|| new_session(&env));
        seat.detach(&video);
        assert!(session.inner.borrow().player.is_none());
        assert_eq!(session.inner.borrow().state.watching, None);
        assert!(logged_with(
            &logged,
            "pharos: group player detached reason=video_gone"
        ));
    }

    #[test]
    fn an_older_video_going_leaves_the_newer_one_attached() {
        let (seat, _) = seat();
        let env = TestEnv::default();
        let old: Rc<dyn GroupPlayer> = Rc::new(TestPlayer::default());
        let new: Rc<dyn GroupPlayer> = Rc::new(TestPlayer::default());
        let session = seat.session(|| new_session(&env));
        seat.attach(ItemRef::Real(7), old.clone());
        seat.attach(ItemRef::Real(8), new);
        seat.detach(&old);
        assert!(session.inner.borrow().player.is_some());
        assert_eq!(
            session.inner.borrow().state.watching,
            Some(ItemRef::Real(8))
        );
    }

    fn groups_tracks() -> GroupTracksView {
        GroupTracksView {
            audio: TrackChoice::Index(2),
            subtitle: TrackChoice::Index(4),
        }
    }

    fn picks_for_groups_tracks() -> TrackPicks {
        TrackPicks {
            audio: Some(2),
            subtitle: Some(Some(4)),
        }
    }

    fn tracks_update() -> GroupUpdate {
        GroupUpdate::Tracks {
            tracks: groups_tracks(),
            by: Some("ada".into()),
        }
    }

    fn rig_with_streams() -> Rig {
        let rig = rig();
        *rig.player.streams.borrow_mut() = (vec![1, 2], vec![3, 4]);
        rig
    }

    #[test]
    fn a_following_member_applies_the_groups_tracks() {
        let rig = rig_with_streams();
        rig.session.deliver(tracks_update());
        assert_eq!(*rig.player.applied.borrow(), [picks_for_groups_tracks()]);
    }

    #[test]
    fn an_own_tracks_member_never_applies_them() {
        let rig = rig_with_streams();
        rig.session.follow_tracks(false);
        rig.env.settle();
        rig.session.deliver(tracks_update());
        rig.session.set_player(Some(rig.player.clone()));
        assert!(rig.player.applied.borrow().is_empty());
        assert!(rig.session.view().own_tracks);
        assert_eq!(calls_with(&rig, "follow_tracks"), ["follow_tracks(false)"]);
    }

    #[test]
    fn following_again_applies_what_the_group_is_on() {
        let rig = rig_with_streams();
        rig.session.follow_tracks(false);
        rig.session.deliver(tracks_update());
        rig.session.follow_tracks(true);
        rig.env.settle();
        assert_eq!(*rig.player.applied.borrow(), [picks_for_groups_tracks()]);
        assert!(!rig.session.view().own_tracks);
        assert_eq!(
            calls_with(&rig, "follow_tracks"),
            ["follow_tracks(false)", "follow_tracks(true)"]
        );
    }

    #[test]
    fn a_late_joiner_applies_the_tracks_from_joined() {
        let rig = rig_with_streams();
        rig.session.deliver(GroupUpdate::Joined(GroupSummary {
            tracks: groups_tracks(),
            ..summary()
        }));
        assert_eq!(*rig.player.applied.borrow(), [picks_for_groups_tracks()]);
    }

    #[test]
    fn a_player_that_arrives_later_starts_on_the_groups_tracks() {
        let rig = rig();
        rig.session.deliver(tracks_update());
        assert!(rig.player.applied.borrow().is_empty());
        *rig.player.streams.borrow_mut() = (vec![2], vec![]);
        rig.session.set_player(Some(rig.player.clone()));
        assert_eq!(
            *rig.player.applied.borrow(),
            [TrackPicks {
                audio: Some(2),
                subtitle: None
            }]
        );
    }

    #[test]
    fn menu_picks_go_to_the_group_while_following_and_stay_local_otherwise() {
        let rig = rig_with_streams();
        assert!(rig.session.pick_audio(2));
        assert!(rig.session.pick_subtitle(None));
        rig.env.settle();
        assert_eq!(
            calls_with(&rig, "set_tracks"),
            ["set_tracks(Index(2),Unset,)", "set_tracks(Unset,Off,)"]
        );
        rig.session.follow_tracks(false);
        assert!(!rig.session.pick_audio(1));
        let idle = Session::new(FakeGroups::default(), TestEnv::default());
        assert!(!idle.pick_subtitle(Some(3)));
    }

    #[test]
    fn track_decisions_are_logged_with_their_inputs_verdict_and_reason() {
        let rig = rig_with_streams();
        rig.session.deliver(tracks_update());
        assert!(rig.session.pick_subtitle(Some(3)));
        rig.session.follow_tracks(false);
        assert!(!rig.session.pick_audio(1));
        let logs = rig.env.logs.borrow().clone();
        let has = |want: &str| {
            assert!(
                logs.iter().any(|l| l.contains(want)),
                "no log line with {want}: {logs:#?}"
            );
        };
        has("pharos: group tracks follow verdict=apply reason=\"following the group\" own_tracks=false group_audio=Index(2) group_subtitle=Index(4) item_audio=[1, 2] item_subtitles=[3, 4]");
        has("pharos: group tracks pick route=SendToGroup reason=\"following the group\" audio=Unset subtitle=Index(3)");
        has("pharos: group tracks pick route=ApplyLocally reason=\"own tracks\" audio=Index(1) subtitle=Unset");
    }

    #[test]
    fn only_a_move_to_another_item_counts_as_an_item_change() {
        let mut state = GroupState::default();
        let _ = state.apply(GroupUpdate::Joined(summary()), 0);
        let _ = state.apply(
            GroupUpdate::Member(Member {
                buffering: true,
                ..member("me", false)
            }),
            0,
        );
        let _ = state.apply(
            GroupUpdate::Pause {
                at_server_ms: 0,
                position_ms: 0,
                by: None,
            },
            0,
        );
        assert_eq!(state.view(true).item_changes, 0);
        assert_eq!(state.view(true).activity, 3, "every update is activity");
        let _ = state.apply(item_update(0, true), 0);
        assert_eq!(state.view(true).item_changes, 1);
    }

    #[test]
    fn own_tracks_come_from_the_roster_on_joining_and_from_the_members_own_update() {
        let mut state = GroupState::default();
        let mine = Member {
            own_tracks: true,
            ..member("me", false)
        };
        let joined = GroupSummary {
            members: vec![member("other", false), mine.clone()],
            you: "me".into(),
            ..summary()
        };
        let _ = state.apply(GroupUpdate::Joined(joined), 0);
        assert!(state.own_tracks, "a rejoining member's server-side mark");

        let other_on = Member {
            own_tracks: true,
            ..member("other", false)
        };
        let _ = state.apply(GroupUpdate::Member(other_on), 0);
        let back = Member {
            own_tracks: false,
            ..mine
        };
        let _ = state.apply(GroupUpdate::Member(back), 0);
        assert!(!state.own_tracks, "the member's own update");
        let _ = state.apply(
            GroupUpdate::Member(Member {
                own_tracks: true,
                ..member("other", false)
            }),
            0,
        );
        assert!(!state.own_tracks, "another member's update is not ours");
    }

    #[test]
    fn a_pick_the_group_cannot_take_is_applied_here_and_said_so() {
        let rig = rig_with_streams();
        rig.groups.refuse_tracks();
        assert!(rig.session.pick_subtitle(Some(3)));
        assert!(
            rig.player.applied.borrow().is_empty(),
            "not before it fails"
        );
        rig.env.settle();
        assert_eq!(
            *rig.player.applied.borrow(),
            [TrackPicks {
                audio: None,
                subtitle: Some(Some(3))
            }]
        );
        assert_eq!(
            rig.session.view().log.lines(),
            ["Couldn't change the group's subtitles; changed yours"]
        );
    }

    #[test]
    fn a_just_me_the_server_refused_is_taken_back() {
        let rig = rig();
        rig.groups.refuse_follow();
        rig.session.follow_tracks(false);
        assert!(rig.session.view().own_tracks, "shown at once");
        rig.env.settle();
        assert!(!rig.session.view().own_tracks);
        let shown = rig.env.ui.borrow().iter().rev().find_map(|u| match u {
            Ui::View(v) => Some(v.own_tracks),
            Ui::Navigate(_) => None,
        });
        assert_eq!(shown, Some(false), "the screen is told");
    }

    #[test]
    fn a_pick_names_the_queue_entry_it_was_made_on() {
        let rig = rig_with_streams();
        rig.session.deliver(GroupUpdate::Item {
            item: ItemRef::Real(7),
            playlist_item_id: "pl-7".into(),
            position_ms: 0,
            playing: false,
        });
        rig.session.pick_audio(2);
        rig.env.settle();
        assert_eq!(
            calls_with(&rig, "set_tracks"),
            ["set_tracks(Index(2),Unset,pl-7)"]
        );
    }

    #[test]
    fn a_pick_reaches_every_following_player_through_the_group() {
        let rig = rig_with_streams();
        assert!(rig.session.pick_subtitle(Some(3)));
        rig.env.settle();
        assert_eq!(
            *rig.player.applied.borrow(),
            [TrackPicks {
                audio: None,
                subtitle: Some(Some(3))
            }]
        );
    }

    fn named(id: &str, name: &str) -> Member {
        Member {
            id: id.into(),
            name: name.into(),
            buffering: false,
            own_tracks: false,
        }
    }

    /// A group of Ada and Bo, seen from Ada's session.
    fn pair() -> Rig {
        let rig = rig();
        rig.session.deliver(GroupUpdate::Joined(GroupSummary {
            members: vec![named("ada", "Ada"), named("bo", "Bo")],
            ..summary()
        }));
        rig
    }

    fn lines(rig: &Rig) -> Vec<String> {
        rig.session.view().log.lines()
    }

    #[test]
    fn a_pause_names_the_member_who_caused_it_and_a_gate_names_no_one() {
        let rig = pair();
        rig.session.deliver(GroupUpdate::Pause {
            at_server_ms: 0,
            position_ms: 724_000,
            by: Some("bo".into()),
        });
        rig.session.deliver(GroupUpdate::Play {
            at_server_ms: 0,
            position_ms: 724_000,
            by: Some(String::new()),
        });
        rig.session.deliver(GroupUpdate::Seek {
            at_server_ms: 0,
            position_ms: 60_000,
            by: Some("nobody".into()),
        });
        assert_eq!(
            lines(&rig),
            [
                "Bo paused at 12:04",
                "Everyone's ready, playing",
                "The group skipped to 1:00"
            ]
        );
    }

    #[test]
    fn a_catch_up_or_buffering_command_is_not_a_line() {
        let rig = pair();
        let by = Some(pharos_client::group::CATCH_UP_BY.to_owned());
        rig.session.deliver(GroupUpdate::Play {
            at_server_ms: 0,
            position_ms: 0,
            by: by.clone(),
        });
        rig.session.deliver(GroupUpdate::Pause {
            at_server_ms: 0,
            position_ms: 724_000,
            by: by.clone(),
        });
        rig.session.deliver(GroupUpdate::Seek {
            at_server_ms: 0,
            position_ms: 60_000,
            by,
        });
        assert!(lines(&rig).is_empty());
    }

    #[test]
    fn roster_changes_buffering_and_own_tracks_become_lines() {
        let rig = pair();
        rig.session.deliver(GroupUpdate::Member(named("cy", "Cy")));
        rig.session.deliver(GroupUpdate::Member(Member {
            buffering: true,
            ..named("bo", "Bo")
        }));
        rig.session.deliver(GroupUpdate::Member(Member {
            own_tracks: true,
            buffering: true,
            ..named("bo", "Bo")
        }));
        rig.session.deliver(GroupUpdate::Left("cy".into()));
        assert_eq!(
            lines(&rig),
            ["Waiting for Bo", "Bo is on their own tracks", "Cy left"]
        );
    }

    #[test]
    fn a_tracks_change_names_who_and_what_changed() {
        let rig = pair();
        rig.session.deliver(GroupUpdate::Tracks {
            tracks: GroupTracksView {
                audio: TrackChoice::Index(2),
                subtitle: TrackChoice::Off,
            },
            by: Some("bo".into()),
        });
        assert_eq!(
            lines(&rig),
            ["Bo switched audio to Audio 2", "Bo turned subtitles off"]
        );
        // The same state again says nothing.
        rig.session.deliver(GroupUpdate::Tracks {
            tracks: GroupTracksView {
                audio: TrackChoice::Index(2),
                subtitle: TrackChoice::Off,
            },
            by: Some("bo".into()),
        });
        assert_eq!(lines(&rig).len(), 2);
    }

    #[test]
    fn the_toast_lasts_four_seconds_from_the_clock() {
        let rig = pair();
        rig.env.now.set(200_000);
        rig.session.deliver(GroupUpdate::Pause {
            at_server_ms: 0,
            position_ms: 0,
            by: Some("ada".into()),
        });
        let log = rig.session.view().log;
        assert_eq!(log.toast(203_999), Some("Ada paused at 0:00"));
        assert_eq!(log.toast(204_000), None);
    }
}
