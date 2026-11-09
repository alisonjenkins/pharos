//! The watch-together group store (014): one active membership, the clock
//! sync, keep-alive and drift loops that run while it lasts, and what each
//! server update and local control means for the player.
//!
//! No Dioxus and no browser in here. [`Env`] is the clock, timer, spawner and
//! event sink; [`GroupPlayer`] is the `<video>`; [`GroupClient`] is the
//! network. The app supplies browser versions, the tests supply fakes.

use std::cell::{Cell, RefCell};
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
    GroupSink, GroupSummary, GroupUpdate, Member, CLOCK_WINDOW, HOLD_BELOW_MS, SEEK_ABOVE_MS,
    SELF_SEEK_GRACE_MS, SETTLE_AFTER_COMMAND_MS,
};
use super::track_follow::{
    follows_group, route_pick, GroupTracksView, PickRoute, TrackChoice, TrackPicks,
};

/// How long a rejoin waits before each retry. They add up to 89 s: past a
/// restarted server's three 20 s rounds of loading its saved groups, which
/// took 51 s from boot in production on 2026-11-08.
pub const REJOIN_BACKOFF_MS: [u32; 10] = [
    500, 1_500, 4_000, 8_000, 12_000, 12_000, 12_000, 12_000, 12_000, 15_000,
];

/// A member the server keeps refusing rejoins at most this often.
pub const REJOIN_AFTER_GONE_EVERY_MS: u32 = 30_000;

/// What the viewer reads when the group this tab was in cannot be joined
/// again (B807); the server's reason follows.
pub fn rejoin_failure(reason: &str) -> String {
    format!("Couldn't rejoin your group, so you're watching on your own. ({reason})")
}

/// Keep-alive cadence (T83's ghost prune is 150 s).
pub const PING_EVERY_S: u64 = 30;

/// A group command whose RPC has not answered after this long is given up on,
/// so one that never answers cannot hold back every later one.
pub const COMMAND_TIMEOUT_MS: u32 = 10_000;
/// Clock re-sync cadence, and the `groupDrift` sample cadence.
pub const SAMPLE_EVERY_S: u64 = 10;
/// Playhead travel past a buffering report that proves the video plays
/// again: two `timeupdate`s of real playback, more than a gap nudge moves it.
pub const PLAYING_AGAIN_MS: u64 = 500;

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
    /// What the group plays; `None` until it has said.
    pub item: Option<ItemRef>,
}

impl GroupView {
    /// Whether the group drives a player showing `item`: a member who opens
    /// something else plays it on their own (B590).
    pub fn drives(&self, item: ItemRef) -> bool {
        self.active && self.item.map_or(true, |on| on == item)
    }
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
    /// The video played to its end (it is paused there, not by anyone).
    fn ended(&self) -> bool;
    /// The video has data to play at its position, and is not seeking.
    fn has_data(&self) -> bool;
    fn play(&self);
    fn pause(&self);
    fn seek_ms(&self, ms: u64);
    fn set_rate(&self, rate: f64);
    /// What the viewer calls the stream `index` of this kind, for the activity
    /// lines. Indexes here are the primary file's, as the group's (ADR-0089).
    fn track_label(&self, kind: TrackKind, index: u32) -> Option<String>;
    /// The audio and subtitle streams of the version playing, as the primary
    /// file's indexes.
    fn stream_indexes(&self) -> (Vec<u32>, Vec<u32>);
    /// Switch the player's audio and subtitle, as its own menus do; `picks`
    /// name the primary file's streams.
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
    /// Be at the group's `position_ms`, which only re-states where the group
    /// is (its item arrived for the item already showing), not a member's
    /// seek: a video playing close to it is left to the drift check (B761).
    Align {
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
    /// The queue entry whose end a member kept the group watching (B1027).
    next_held: Option<String>,
    pub ended: Option<String>,
    /// The group's updates so far.
    activity: u64,
    /// The group's moves to another item so far.
    item_changes: u64,
    /// The item the watch page shows.
    watching: Option<ItemRef>,
    /// What the group plays.
    item: Option<ItemRef>,
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

    /// The stage shows what the group plays, or one of them is not known yet.
    fn drives_stage(&self) -> bool {
        match (self.item, self.watching) {
            (Some(on), Some(shown)) => on == shown,
            _ => true,
        }
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

    /// Where a player newly loading `item` should start: the group's
    /// position, while `item` is what the group plays. Before the first clock
    /// sample that is the anchor's own position; the catch-up corrects it
    /// once the clock is known.
    pub fn start_for(&self, item: ItemRef, now_ms: i64) -> Option<u64> {
        if self.item != Some(item) {
            return None;
        }
        self.group_ms(now_ms)
    }

    /// Where the group is now; before the first clock sample, where it
    /// last said it was.
    fn group_ms(&self, now_ms: i64) -> Option<u64> {
        self.expected(now_ms)
            .map(|(at, _)| at)
            .or_else(|| self.anchor.map(|a| a.position_ms))
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
            GroupUpdate::NextHeld { by, .. } => vec![by_member(by, activity::Action::HeldNext)],
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
                    self.next_held = None;
                }
                self.playlist_item_id = playlist_item_id;
                self.item = Some(item);
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
                    // Already showing it, perhaps loaded at this member's
                    // own resume point before the item arrived: put it at the
                    // group's (a no-op when already close). The seek's
                    // landing carries the ready (B664).
                    vec![Action::Align { position_ms }]
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
            GroupUpdate::NextHeld {
                playlist_item_id, ..
            } => {
                self.next_held = Some(playlist_item_id);
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

    /// A member kept the group watching the end of the entry it is on, so no
    /// "Up next" plays on from it (B1027).
    pub fn holds_next(&self) -> bool {
        !self.playlist_item_id.is_empty()
            && self.next_held.as_deref() == Some(self.playlist_item_id.as_str())
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
            item: self.item,
        }
    }
}

/// A queued group command: the epoch of the membership it was made in,
/// its kind for the diagnostics, and the RPC.
type QueuedCommand = (u64, String, LocalBoxFuture<'static, Result<(), GroupError>>);

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
    /// Where the playhead was when buffering was reported.
    buffering_at_ms: u64,
    /// When this member said ready, until the group's next play: the time it
    /// spent waiting on the others.
    ready_sent_ms: Option<i64>,
    /// The ready owed was held once to seek a player that loaded away from
    /// the group (B664); the next chance sends it wherever the player is.
    ready_held: bool,
    last_drift_ms: Option<i64>,
    /// This member left; what the server still sends is not for the screens.
    leaving: bool,
    /// When the clock was last sampled and the server last pinged, by the
    /// wall clock: a hidden tab's timers can fire once a minute, so counting
    /// one-second ticks would ping every 30 minutes and be pruned (B893).
    sampled_ms: i64,
    pinged_ms: i64,
    /// When this session last moved the player itself.
    self_seek_ms: Option<i64>,
    /// When the drift check last started a player it found paused while the
    /// group plays (B660), so a refused `play()` is not retried every tick.
    resumed_ms: Option<i64>,
    /// The video waited within the grace of our own seek; it is reported as
    /// buffering only if it outlasts the grace.
    held_waiting: bool,
    /// Group commands not yet sent, each with the epoch of the membership it
    /// was made in; one task sends them one at a time, so the server sees
    /// them in the order the viewer caused them.
    commands: VecDeque<QueuedCommand>,
    /// When the last "couldn't reach the group" line was shown: a burst of
    /// failures (a socket outage fails every queued command and buffering
    /// report) shows one line per toast, not one per command.
    failure_shown_ms: Option<i64>,
    /// A task is working through `commands`.
    draining: bool,
    /// That task has a command's RPC out and unanswered.
    in_flight: bool,
}

impl Inner {
    /// The owed ready, now that the player has data: sent, unless the player
    /// sits away from the group, when it is seeked there first, once, and the
    /// ready waits for that seek's landing (B664). Answers whether to send it,
    /// and the line for a hold.
    fn ready_or_hold(&mut self, player: &Rc<dyn GroupPlayer>, now: i64) -> (bool, Option<String>) {
        let away = self
            .state
            .group_ms(now)
            .filter(|at| player.position_ms().abs_diff(*at) > SEEK_ABOVE_MS);
        match away {
            Some(at) if !self.ready_held => {
                self.ready_held = true;
                self.self_seek_ms = Some(now);
                let line = format!(
                    "pharos: group ready held reason=away position_ms={} group_ms={at}",
                    player.position_ms()
                );
                player.seek_ms(at);
                (false, Some(line))
            }
            _ => {
                self.state.owes_ready = false;
                self.ready_held = false;
                self.ready_sent_ms = Some(now);
                (true, None)
            }
        }
    }

    /// The player the group may move: none while the stage shows something
    /// other than what the group plays.
    fn driven(&self) -> Option<Rc<dyn GroupPlayer>> {
        self.player.clone().filter(|_| self.state.drives_stage())
    }

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
    /// A rejoin is under way; a second trigger waits for it.
    rejoining: Cell<bool>,
    /// When a rejoin triggered by the server last started, so a server that
    /// keeps refusing this member cannot start one per call.
    rejoined_ms: Cell<Option<i64>>,
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
            rejoining: Cell::new(false),
            rejoined_ms: Cell::new(None),
        });
        *session.this.borrow_mut() = Rc::downgrade(&session);
        session
    }

    fn me(&self) -> Option<Rc<Self>> {
        self.this.borrow().upgrade()
    }

    /// The group client, to move it to a new connection.
    pub fn client(&self) -> &C {
        &self.client
    }

    /// Whether a membership is active.
    pub fn active(&self) -> bool {
        self.inner.borrow().active
    }

    /// Where a player newly loading `item` starts, while in a group that
    /// plays it: the group's position, not this member's own resume point
    /// (B664). `None` out of a group or for another item.
    pub fn start_for(&self, item: ItemRef) -> Option<u64> {
        let inner = self.inner.borrow();
        if !inner.active {
            return None;
        }
        inner.state.start_for(item, self.env.now_ms())
    }

    /// Whether the group drives the stage's player: in a group, and the
    /// stage shows what the group plays.
    pub fn drives(&self) -> bool {
        let inner = self.inner.borrow();
        inner.active && inner.state.drives_stage()
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
        let answers = futures_util::future::try_join_all(asks)
            .await
            .inspect_err(|e| self.rejoin_if_gone(e, "seen"))?;
        let mut all = Vec::new();
        for answer in answers {
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
        self.began(epoch, made).await
    }

    /// Join the group `id`.
    pub async fn join(&self, id: &str) -> Result<GroupSummary, GroupError> {
        let epoch = self.prepare(None).await;
        let made = self.client.join(id, self.sink(epoch)).await;
        self.began(epoch, made).await
    }

    /// The membership was lost while the viewer is still in the group: the
    /// connection it lived on was replaced, or the server no longer counts
    /// this member. Joins the same group again, without leaving it first or
    /// showing the viewer out of it, so the server's seat for this device
    /// is taken back and the group's item brings the player to it.
    ///
    /// A server that restarted answers that it knows no such group until it
    /// has loaded its saved groups (its boot reconciliation retries every
    /// 20 s), so a refusal is retried on [`REJOIN_BACKOFF_MS`]. Only an
    /// `ended` from the group, or the backoff running out, gives up; then the
    /// viewer reads why, and watches on their own.
    pub async fn rejoin(&self, reason: &'static str) {
        let id = {
            let inner = self.inner.borrow();
            inner.state.id.clone().filter(|_| inner.active)
        };
        let Some(id) = id else {
            return;
        };
        if self.rejoining.replace(true) {
            self.env.info(&format!(
                "pharos: group rejoin skipped reason={reason} group={id} cause=in_progress"
            ));
            return;
        }
        let started = self.env.now_ms();
        self.env.info(&format!(
            "pharos: group rejoin started reason={reason} group={id}"
        ));
        let mut attempts = 0u32;
        let mut backoff = REJOIN_BACKOFF_MS.iter();
        let outcome = loop {
            attempts = attempts.saturating_add(1);
            // Out of the old membership without a `leave`: that would end
            // the seat this join takes back, and publish the viewer out.
            self.inner.borrow_mut().active = false;
            let epoch = self.prepare(None).await;
            let made = self.client.join(&id, self.sink(epoch)).await;
            let error = match self.began(epoch, made).await {
                Ok(_) => break Ok(()),
                Err(e) => e,
            };
            if self.inner.borrow().epoch != epoch {
                break Err(None);
            }
            let ended = self.inner.borrow().state.ended.is_some();
            match backoff.next() {
                Some(&wait) if !ended => {
                    self.env.info(&format!(
                        "pharos: group rejoin retrying reason={reason} group={id} attempt={attempts} wait_ms={wait} error={}",
                        error.0
                    ));
                    self.env.sleep(wait).await;
                    // The viewer left, or joined another group, meanwhile.
                    if self.inner.borrow().epoch != epoch {
                        break Err(None);
                    }
                }
                _ => break Err(Some(error)),
            }
        };
        self.rejoining.set(false);
        let waited = self.env.now_ms().saturating_sub(started);
        match outcome {
            Ok(()) => self.env.info(&format!(
                "pharos: group rejoined reason={reason} group={id} attempts={attempts} waited_ms={waited}"
            )),
            Err(None) => self.env.info(&format!(
                "pharos: group rejoin stopped reason={reason} group={id} attempts={attempts} cause=superseded"
            )),
            Err(Some(e)) => {
                self.env.warn(&format!(
                    "pharos: group rejoin failed reason={reason} group={id} attempts={attempts} waited_ms={waited} error={}",
                    e.0
                ));
                {
                    let mut inner = self.inner.borrow_mut();
                    inner.active = false;
                    inner.state.ended.get_or_insert_with(|| rejoin_failure(&e.0));
                }
                self.publish();
            }
        }
    }

    /// A membership call was refused because the server no longer counts
    /// this member in the group (it pruned a silent member, B893): rejoin,
    /// at most once per [`REJOIN_AFTER_GONE_EVERY_MS`].
    fn rejoin_if_gone(&self, error: &GroupError, call: &str) {
        if !error.is_gone() || !self.active() {
            return;
        }
        let now = self.env.now_ms();
        let every = i64::from(REJOIN_AFTER_GONE_EVERY_MS);
        if self
            .rejoined_ms
            .get()
            .is_some_and(|at| now.saturating_sub(at) < every)
        {
            return;
        }
        self.rejoined_ms.set(Some(now));
        self.env.info(&format!(
            "pharos: group membership gone call={call}, rejoining"
        ));
        if let Some(me) = self.me() {
            self.env
                .spawn(Box::pin(async move { me.rejoin("not_a_member").await }));
        }
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
        // Commands of the old membership stay queued but are dropped when
        // reached, so none reaches the new group.
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

    /// The answer to the call that began membership `epoch`. If the viewer
    /// left, or began another membership, while it was in flight (B1005),
    /// the answer is stale: it must not put them back in the group. A server
    /// that already took the seat is told to drop it, unless a newer
    /// membership began, whose seat that `leave` would end.
    async fn began(
        &self,
        epoch: u64,
        made: Result<GroupSummary, GroupError>,
    ) -> Result<GroupSummary, GroupError> {
        let summary = made?;
        let stale = {
            let inner = self.inner.borrow();
            // `prepare` resets `leaving`, so it is set only while no newer
            // membership has begun.
            (inner.epoch != epoch).then_some(inner.leaving && !inner.active)
        };
        if let Some(left) = stale {
            self.env.info(&format!(
                "pharos: group join answer superseded group={} left={left}",
                summary.id
            ));
            if left {
                if let Err(e) = self.client.leave().await {
                    self.env
                        .warn(&format!("pharos: leaving the group failed: {}", e.0));
                }
            }
            return Err(GroupError("The join was superseded".into()));
        }
        let epoch = {
            let mut inner = self.inner.borrow_mut();
            // An `Ended` that raced the answer leaves nothing to run.
            if inner.state.ended.is_some() {
                return Err(GroupError::gone());
            }
            inner.active = true;
            let now = self.env.now_ms();
            inner.sampled_ms = now;
            inner.pinged_ms = now;
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
        // The player may have loaded while the join was in flight; its
        // events then found no group.
        self.ready_if_loaded();
        if let Some(me) = self.me() {
            self.env.spawn(Box::pin(async move { me.run(epoch).await }));
        }
        Ok(summary)
    }

    /// A ready is owed and the player already has data, so no media event
    /// will come to carry it: the player loaded while a join was in flight,
    /// or the item it is owed for needed no seek (B665). Say it now.
    fn ready_if_loaded(&self) {
        let now = self.env.now_ms();
        let (playlist, position, playing, line) = {
            let mut inner = self.inner.borrow_mut();
            if !inner.active || !inner.state.owes_ready {
                return;
            }
            let Some(player) = inner.driven().filter(|p| p.has_data()) else {
                return;
            };
            let (said, line) = inner.ready_or_hold(&player, now);
            if !said {
                if let Some(line) = line {
                    drop(inner);
                    self.env.info(&line);
                }
                return;
            }
            (
                inner.state.playlist_item_id.clone(),
                player.position_ms(),
                !player.paused(),
                format!(
                    "pharos: group ready sent reason=loaded position_ms={}",
                    player.position_ms()
                ),
            )
        };
        self.env.info(&line);
        let client = self.client.clone();
        self.command(format!("ready(playing={playing})"), async move {
            client.ready(playlist, position, playing).await
        });
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
        {
            let mut inner = self.inner.borrow_mut();
            inner.player = player;
            // A new element gets its own one hold (B664).
            inner.ready_held = false;
        }
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
            (inner.driven(), inner.state.own_tracks, inner.state.tracks)
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

    /// The picks this member applies to a player of streams `audio` and
    /// `subtitles`: the group's, unless this member is out of the group or on
    /// their own tracks. `None` when there is nothing to apply.
    pub fn tracks_followed(&self, audio: &[u32], subtitles: &[u32]) -> Option<TrackPicks> {
        let inner = self.inner.borrow();
        if !inner.active {
            return None;
        }
        follows_group(inner.state.own_tracks, inner.state.tracks, audio, subtitles)
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
                inner.active && inner.state.drives_stage(),
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
                me.env.warn(&format!(
                    "pharos: group command failed kind=set_tracks: {}",
                    e.0
                ));
                me.apply_pick_here(audio, subtitle, epoch);
            }));
        }
        true
    }

    /// Move the group to `item` from `position_ms` (014 US9): the next or
    /// previous episode, or another title, picked on the player the group
    /// drives. `true` when it went to the group, whose item update then
    /// brings every member there, this one included; `false` when the caller
    /// plays it alone (not in a group, or the stage shows another item).
    /// Should the group refuse it (a server before schemaVersion 23, or an
    /// item another member may not see), this member plays it alone.
    pub fn move_to(&self, item: ItemRef, position_ms: u64) -> bool {
        self.send_move(item, position_ms, true)
    }

    /// Move the group to `item` from `position_ms` because the viewer asked
    /// for it on a title page ("Play for the group", 014 US10), even while
    /// this member watches something else alone. `false` out of a group,
    /// where the caller starts one. A refused move plays the item alone, as
    /// for `move_to`.
    pub fn play_for_group(&self, item: ItemRef, position_ms: u64) -> bool {
        self.send_move(item, position_ms, false)
    }

    /// `setItem` to the group, from the player it drives when `from_stage`.
    fn send_move(&self, item: ItemRef, position_ms: u64, from_stage: bool) -> bool {
        let (driving, from, epoch) = {
            let inner = self.inner.borrow();
            (
                inner.active && (!from_stage || inner.state.drives_stage()),
                inner.state.playlist_item_id.clone(),
                inner.epoch,
            )
        };
        self.env.info(&format!(
            "pharos: group move item={item} position_ms={position_ms} from={from:?} asked={} route={}",
            if from_stage { "player" } else { "title" },
            if driving { "group" } else { "alone" }
        ));
        if !driving {
            return false;
        }
        if let Some(me) = self.me() {
            let client = self.client.clone();
            self.env.spawn(Box::pin(async move {
                let Err(e) = client.set_item(item, position_ms, from).await else {
                    return;
                };
                me.env.warn(&format!(
                    "pharos: group move refused item={item} reason={:?}; playing it alone",
                    e.0
                ));
                let still = {
                    let inner = me.inner.borrow();
                    inner.active && inner.epoch == epoch
                };
                if still {
                    me.env.ui(Ui::Navigate(item));
                }
            }));
        }
        true
    }

    /// A command the group never heard: say so on the activity toast, or the
    /// viewer's click seems to do nothing (B807). Nothing for a membership
    /// that has moved on.
    fn command_failed(&self, epoch: u64) {
        {
            let mut inner = self.inner.borrow_mut();
            if !inner.active || inner.epoch != epoch {
                return;
            }
            let now = self.env.now_ms();
            let toast = i64::try_from(activity::TOAST_MS).unwrap_or(i64::MAX);
            if inner
                .failure_shown_ms
                .is_some_and(|shown| now.saturating_sub(shown) < toast)
            {
                return;
            }
            inner.failure_shown_ms = Some(now);
            let at = u64::try_from(now).unwrap_or(0);
            inner.state.log.push(
                &Event {
                    by: None,
                    action: activity::Action::CommandFailed,
                },
                at,
            );
            inner.state.activity = inner.state.activity.wrapping_add(1);
        }
        self.publish();
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
            inner.driven()
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
                me.env.warn(&format!(
                    "pharos: group command failed kind=follow_tracks({follow}): {}",
                    e.0
                ));
                me.command_failed(epoch);
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
                inner.driven(),
                inner
                    .active
                    .then(|| inner.state.expected(self.env.now_ms()))
                    .flatten(),
            )
        };
        if let (Some(player), Some((position_ms, playing))) = (player, expected) {
            if self.needs_seek(&*player, position_ms, playing, "catch_up") {
                self.inner.borrow_mut().self_seek_ms = Some(self.env.now_ms());
                player.seek_ms(position_ms);
            }
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
        // A play or an align only re-states where the group is; a seek or a
        // pause is a member's own move, followed exactly.
        let restates = match action {
            Action::Play { .. } => Some(("play", true)),
            Action::Align { .. } => Some(("align", false)),
            _ => None,
        };
        let owes_on_arrival = matches!(action, Action::Seek { .. } | Action::Align { .. });
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
            Action::Align { position_ms } => (0, position_ms, |_| {}),
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
                inner.driven()
            };
            let Some(player) = player else {
                return;
            };
            let far = match restates {
                Some((source, play)) => {
                    let group_playing =
                        play || me.inner.borrow().state.anchor.is_some_and(|a| a.playing);
                    me.needs_seek(&*player, position_ms, group_playing, source)
                }
                None => player.position_ms().abs_diff(position_ms) > HOLD_BELOW_MS,
            };
            if far {
                me.inner.borrow_mut().self_seek_ms = Some(me.env.now_ms());
                player.seek_ms(position_ms);
            } else if owes_on_arrival {
                // Already there: no `seeked` will come to carry a ready the
                // group's item asked for, so a loaded player says it now.
                me.ready_if_loaded();
            }
            then(&*player);
        }));
    }

    /// Whether re-stating the group's position to `player` takes a seek. A
    /// seek restarts the video's buffering, a visible stutter while it plays,
    /// so a video playing along with a playing group is moved only when the
    /// gap is too big to close by rate soon (`NUDGE_BELOW_MS`); otherwise it
    /// is put exactly (B761). A skipped seek is logged with its inputs.
    fn needs_seek(
        &self,
        player: &dyn GroupPlayer,
        position_ms: u64,
        group_playing: bool,
        source: &str,
    ) -> bool {
        let at = player.position_ms();
        let off = at.abs_diff(position_ms);
        let rolling = group_playing && !player.paused();
        let limit = if rolling {
            NUDGE_BELOW_MS
        } else {
            HOLD_BELOW_MS
        };
        if off > limit {
            return true;
        }
        if rolling && off > HOLD_BELOW_MS {
            self.env.info(&format!(
                "pharos: group seek skipped reason=small_gap source={source} off_ms={off} position_ms={at} group_ms={position_ms}"
            ));
        }
        false
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
        loop {
            self.env.sleep(1_000).await;
            if self.inner.borrow().epoch != epoch {
                return;
            }
            self.tick().await;
        }
    }

    /// One second of a membership: the drift check, then the drift sample and
    /// clock sync once 10 s have passed since the last, and the ping once 30 s
    /// have.
    pub async fn tick(&self) {
        self.check_drift();
        self.report_held_stall();
        let (sample, ping) = {
            let now = self.env.now_ms();
            let mut inner = self.inner.borrow_mut();
            (
                elapsed(&mut inner.sampled_ms, now, SAMPLE_EVERY_S),
                elapsed(&mut inner.pinged_ms, now, PING_EVERY_S),
            )
        };
        if sample {
            self.report_drift();
            self.sync_clock().await;
        }
        if ping {
            if let Err(e) = self.client.ping().await {
                self.env
                    .warn(&format!("pharos: group ping failed: {}", e.0));
                self.rejoin_if_gone(&e, "ping");
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
            let Some(player) = inner.driven() else {
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
        // Still loading: another seek would restart the load, push back the
        // grace that reports a real stall, and land on data not yet fetched.
        let (loading, owes_ready) = {
            let inner = self.inner.borrow();
            (
                inner.held_waiting || inner.buffering_sent,
                inner.state.owes_ready,
            )
        };
        if loading {
            return;
        }
        if player.paused() {
            // A player still owing its ready is loading what the group asked
            // for; one at its end has nothing left to play.
            if !owes_ready && !player.ended() {
                self.resume_paused(&player, expected, since, now);
            }
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

    /// B660: the group plays but the driven player sits paused, with nothing
    /// loading. Nothing else would start it (the drift check skips a paused
    /// player, and the server thinks every member plays), so put it where the
    /// group is and start it: after the server's last command has settled,
    /// and once per settle window, so a refused `play()` is not asked again
    /// each second.
    fn resume_paused(&self, player: &Rc<dyn GroupPlayer>, expected: u64, since: u64, now: i64) {
        let settle = i64::try_from(SETTLE_AFTER_COMMAND_MS).unwrap_or(i64::MAX);
        {
            let mut inner = self.inner.borrow_mut();
            let recent = inner
                .resumed_ms
                .is_some_and(|at| now.saturating_sub(at) < settle);
            if since < SETTLE_AFTER_COMMAND_MS || recent {
                return;
            }
            inner.resumed_ms = Some(now);
            inner.self_seek_ms = Some(now);
        }
        self.env.info(&format!(
            "pharos: group resume reason=paused_while_group_plays position_ms={} expected_ms={expected}",
            player.position_ms()
        ));
        self.set_rate(player, 1.0);
        player.seek_ms(expected);
        player.play();
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
    /// `false` when the group does not drive the stage (the player acts on its own).
    pub fn toggle_play(&self) -> bool {
        let (active, paused) = {
            let inner = self.inner.borrow();
            (
                inner.active && inner.state.drives_stage(),
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

    /// Seek as the viewer asked: a group command. `false` when the group does
    /// not drive the stage.
    pub fn seek(&self, position_ms: u64) -> bool {
        if !self.drives() {
            return false;
        }
        let client = self.client.clone();
        self.command(format!("seek({position_ms})"), async move {
            client.seek(position_ms).await
        });
        true
    }

    /// The viewer cancelled "Up next": the group keeps watching this entry's
    /// end, so every member's countdown stops too (B1027). `false` when the
    /// group does not drive the stage, where the cancel is this player's.
    pub fn hold_next(&self) -> bool {
        if !self.drives() {
            return false;
        }
        let playlist_item_id = self.inner.borrow().state.playlist_item_id.clone();
        // Before the group's item arrives the server would hold whatever it
        // plays, not the entry this viewer cancelled on.
        if playlist_item_id.is_empty() {
            return false;
        }
        let client = self.client.clone();
        self.command(format!("hold_next({playlist_item_id})"), async move {
            client.hold_next(playlist_item_id).await
        });
        true
    }

    /// The queue entry the group plays; empty out of a group or before its
    /// first item. A player's "Up next" belongs to one entry (B1027).
    pub fn playlist_item_id(&self) -> String {
        let inner = self.inner.borrow();
        if inner.active {
            inner.state.playlist_item_id.clone()
        } else {
            String::new()
        }
    }

    /// A member kept the group watching the end of what the stage shows, so
    /// this player's "Up next" must not play on (B1027).
    pub fn next_held(&self) -> bool {
        let inner = self.inner.borrow();
        inner.active && inner.state.drives_stage() && inner.state.holds_next()
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
            let epoch = inner.epoch;
            inner
                .commands
                .push_back((epoch, kind.clone(), fut.boxed_local()));
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
                let Some((epoch, kind, next)) = next else {
                    return;
                };
                // The client resolves its membership only when the command
                // is polled, so a command of an earlier membership would
                // reach the group joined since: drop it instead.
                let now = me.inner.borrow().epoch;
                if epoch != now {
                    me.env.info(&format!(
                        "pharos: group command dropped kind={kind} reason=membership_changed epoch={epoch} now={now}"
                    ));
                    continue;
                }
                match select(next, me.env.sleep(COMMAND_TIMEOUT_MS)).await {
                    Either::Left((Err(e), _)) => {
                        me.env.warn(&format!(
                            "pharos: group command failed kind={kind}: {}",
                            e.0
                        ));
                        me.command_failed(epoch);
                        me.rejoin_if_gone(&e, "command");
                    }
                    Either::Left((Ok(()), _)) => {}
                    Either::Right(_) => {
                        me.env.warn(&format!(
                            "pharos: group command timed out kind={kind} after_ms={COMMAND_TIMEOUT_MS}"
                        ));
                        me.command_failed(epoch);
                    }
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
            let Some(player) = inner.driven() else {
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
            inner.buffering_at_ms = player.position_ms();
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
            let Some(player) = inner.driven() else {
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
                        inner.buffering_at_ms = player.position_ms();
                        buffering = Some(true);
                    }
                }
                // A video can play on with no `canplay` or `playing` after a
                // wait; the playhead moving is the recovery it never announced.
                MediaEvent::TimeUpdate
                    if inner.buffering_sent
                        && !player.paused()
                        && player.position_ms()
                            >= inner.buffering_at_ms.saturating_add(PLAYING_AGAIN_MS) =>
                {
                    line = Some(format!(
                        "pharos: group buffering ended reason=playhead_moved from_ms={} position_ms={}",
                        inner.buffering_at_ms,
                        player.position_ms()
                    ));
                    inner.buffering_sent = false;
                    inner.held_waiting = false;
                    buffering = Some(false);
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
                    // A player can load at this member's own resume point
                    // after the catch-up seek (the engine applies its start
                    // late): put it at the group's position first, once, and
                    // say ready when that seek lands (B664).
                    if owes {
                        let (said, held) = inner.ready_or_hold(&player, now);
                        ready = said;
                        line = held.or(line);
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

/// Below this, a video playing along with a playing group is not seeked to
/// re-state the group's position: the drift check closes the gap by rate in
/// under about 10 s, where a seek would rebuffer (B761).
const NUDGE_BELOW_MS: u64 = 500;

/// Whether `every_s` seconds have passed since `*last` by `now`; if so,
/// `*last` moves to `now`.
fn elapsed(last: &mut i64, now: i64, every_s: u64) -> bool {
    let every_ms = i64::try_from(every_s.saturating_mul(1_000)).unwrap_or(i64::MAX);
    if now.saturating_sub(*last) < every_ms {
        return false;
    }
    *last = now;
    true
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
        ended: Cell<bool>,
        has_data: Cell<bool>,
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
        fn ended(&self) -> bool {
            self.ended.get()
        }
        fn has_data(&self) -> bool {
            self.has_data.get()
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
        for _ in 1..=30 {
            rig.env.now.set(rig.env.now.get() + 1_000);
            let _ = rig.session.tick().now_or_never();
        }
        assert_eq!(calls_with(&rig, "clock(").len() - clocks, 3);
        assert_eq!(calls_with(&rig, "ping").len(), 1);
    }

    /// The `View`s the session published, oldest first.
    fn views(rig: &Rig) -> Vec<GroupView> {
        rig.env
            .ui
            .borrow()
            .iter()
            .filter_map(|u| match u {
                Ui::View(v) => Some(v.clone()),
                Ui::Navigate(_) => None,
            })
            .collect()
    }

    /// B894: after a server restart the new connection's join is refused
    /// until the server has loaded its saved groups. The rejoin retries
    /// through that, never leaves the group or shows the viewer out of it,
    /// and ends back in it with a new membership.
    #[test]
    fn a_rejoin_retries_while_the_server_loads_its_groups() {
        let rig = rig();
        let views_before = views(&rig).len();
        let epoch = rig.session.inner.borrow().epoch;
        rig.groups.gone_for_joins(3);
        let _ = rig.session.rejoin("reconnect").now_or_never();
        rig.env.settle();
        assert_eq!(
            calls_with(&rig, "join(g1)").len(),
            5,
            "the first join, 3 refused, 1 taken"
        );
        assert!(
            calls_with(&rig, "leave").is_empty(),
            "a rejoin never leaves"
        );
        assert!(rig.session.active());
        assert!(rig.session.inner.borrow().epoch > epoch, "a new membership");
        assert!(
            views(&rig).iter().skip(views_before).all(|v| v.active),
            "the viewer is never shown out of the group"
        );
        assert_eq!(
            rig.env
                .logged("pharos: group rejoined reason=reconnect group=g1 attempts=4 ")
                .len(),
            1,
            "{:?}",
            rig.env.logs.borrow()
        );
    }

    /// A group the server no longer has, past every retry, is given up on:
    /// the viewer reads why and watches on their own.
    #[test]
    fn a_rejoin_that_never_finds_the_group_says_so() {
        let rig = rig();
        rig.groups.gone_for_joins(u32::MAX);
        let _ = rig.session.rejoin("reconnect").now_or_never();
        rig.env.settle();
        assert_eq!(
            calls_with(&rig, "join(g1)").len(),
            1 + 1 + REJOIN_BACKOFF_MS.len(),
            "one try, then one per backoff step"
        );
        assert!(!rig.session.active());
        let last = views(&rig).pop();
        assert!(
            last.as_ref().is_some_and(|v| !v.active),
            "a view out of the group after giving up"
        );
        assert_eq!(
            last.and_then(|v| v.ended).as_deref(),
            Some("Couldn't rejoin your group, so you're watching on your own. (This group has ended)")
        );
    }

    /// B893: a member the server pruned is told so by its next ping; it joins
    /// the group again rather than going on as a member nobody hears.
    #[test]
    fn a_pruned_member_rejoins_on_the_ping_that_finds_out() {
        let rig = rig();
        rig.groups.gone_pings(true);
        rig.env.now.set(rig.env.now.get() + 30_000);
        let _ = rig.session.tick().now_or_never();
        rig.env.settle();
        assert_eq!(calls_with(&rig, "join(g1)").len(), 2);
        assert!(rig.session.active());
        // The server still refuses: no second rejoin within the 30 s.
        rig.env.now.set(rig.env.now.get() + 29_000);
        let _ = rig.session.tick().now_or_never();
        rig.env.settle();
        assert_eq!(calls_with(&rig, "join(g1)").len(), 2, "one rejoin per 30 s");
    }

    /// Out of a group there is nothing to rejoin.
    #[test]
    fn a_rejoin_out_of_a_group_does_nothing() {
        let session = Session::new(FakeGroups::default(), TestEnv::default());
        let _ = session.rejoin("reconnect").now_or_never();
        assert!(!session.active());
    }

    /// B893: a hidden tab's timers fire about once a minute, so the one-second
    /// tick comes 60 s apart. Each such tick still pings, well inside the
    /// server's 150 s prune, where counting ticks pinged every 30 minutes.
    #[test]
    fn a_throttled_tick_still_pings_by_the_clock() {
        let rig = rig();
        let pings = || calls_with(&rig, "ping").len();
        for _ in 0..3 {
            rig.env.now.set(rig.env.now.get() + 60_000);
            let _ = rig.session.tick().now_or_never();
        }
        assert_eq!(pings(), 3, "one ping a minute");
        rig.env.now.set(rig.env.now.get() + 1_000);
        let _ = rig.session.tick().now_or_never();
        assert_eq!(pings(), 3, "a tick right after a ping does not ping again");
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

    /// 014 US9: the next episode picked on the group's player goes to the
    /// group, from the entry playing, and the group's item update is what
    /// brings this member there.
    #[test]
    fn a_move_goes_to_the_group_and_its_item_brings_this_member() {
        let rig = rig();
        rig.session.set_watching(Some(ItemRef::Real(7)));
        rig.session.deliver(item_update(0, true));
        rig.env.settle();
        assert!(rig.session.move_to(ItemRef::Real(8), 0));
        rig.env.settle();
        assert_eq!(calls_with(&rig, "set_item"), ["set_item(8,0,p1)"]);
        assert!(rig
            .env
            .ui
            .borrow()
            .contains(&Ui::Navigate(ItemRef::Real(8))));
    }

    /// Off the group's item (a song while the group is on a film, V508), a
    /// pick plays alone: nothing goes to the group.
    #[test]
    fn a_move_from_another_item_plays_alone() {
        let rig = rig();
        rig.session.deliver(item_update(0, true));
        rig.session.set_watching(Some(ItemRef::Real(9)));
        rig.env.settle();
        assert!(!rig.session.move_to(ItemRef::Real(10), 0));
        rig.env.settle();
        assert!(calls_with(&rig, "set_item").is_empty());
    }

    /// "Play for the group" on a title page (014 US10) moves the group even
    /// from a member watching something else alone, from the position asked,
    /// and the group's item update then brings this member to it.
    #[test]
    fn playing_a_title_for_the_group_moves_it_from_anywhere() {
        let rig = rig();
        rig.session.deliver(item_update(0, true));
        rig.session.set_watching(Some(ItemRef::Real(9)));
        rig.env.settle();
        assert!(rig.session.play_for_group(ItemRef::Real(10), 5_000));
        rig.env.settle();
        assert_eq!(calls_with(&rig, "set_item"), ["set_item(10,5000,p1)"]);
        assert!(rig
            .env
            .ui
            .borrow()
            .contains(&Ui::Navigate(ItemRef::Real(10))));
    }

    /// Out of a group there is no group to play for: the caller starts one.
    #[test]
    fn playing_for_the_group_out_of_one_sends_nothing() {
        let groups = FakeGroups::with_groups(vec![summary()]);
        let env = TestEnv::default();
        let session = Session::new(groups.clone(), env.clone());
        assert!(!session.play_for_group(ItemRef::Real(10), 0));
        env.settle();
        assert!(groups.calls().iter().all(|c| !c.starts_with("set_item")));
    }

    /// A move the group refuses (an older server, or an item another member
    /// may not see) still takes this member there, alone.
    #[test]
    fn a_refused_move_plays_the_item_alone() {
        let rig = rig();
        rig.groups.refuse_set_item();
        rig.session.set_watching(Some(ItemRef::Real(7)));
        rig.session.deliver(item_update(0, true));
        rig.env.settle();
        assert!(rig.session.move_to(ItemRef::Real(8), 0));
        rig.env.settle();
        assert!(rig
            .env
            .ui
            .borrow()
            .contains(&Ui::Navigate(ItemRef::Real(8))));
        assert_eq!(rig.env.logged("pharos: group move refused item=8").len(), 1);
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

    /// B1005: a join held in flight by a push to the sink, as the server's
    /// pushes can reach a member before the answer does.
    fn hold_join(groups: &FakeGroups) {
        groups.push_on_join(vec![GroupUpdate::Item {
            item: ItemRef::Real(7),
            playlist_item_id: "pl-7".into(),
            position_ms: 0,
            playing: false,
        }]);
    }

    fn poll_once<T>(f: &mut std::pin::Pin<Box<impl std::future::Future<Output = T>>>) -> bool {
        let mut cx = std::task::Context::from_waker(futures_util::task::noop_waker_ref());
        std::future::Future::poll(f.as_mut(), &mut cx).is_ready()
    }

    /// B1005: Leave pressed while a join is in flight finds the session not
    /// yet active and sends nothing; the answer must not put the viewer back,
    /// and the seat the server took is dropped. Disarm check: began ignoring
    /// the epoch leaves the session active and sends no leave.
    #[test]
    fn leaving_while_a_join_is_in_flight_drops_the_seat_it_takes() {
        let groups = FakeGroups::with_groups(vec![summary()]);
        let session = Session::new(groups.clone(), TestEnv::default());
        hold_join(&groups);
        let mut joined = Box::pin(session.join("g1"));
        assert!(!poll_once(&mut joined));
        let _ = session.leave().now_or_never();
        assert!(groups.calls().iter().all(|c| c != "leave"));
        assert!(poll_once(&mut joined));
        assert!(!session.view().active);
        assert_eq!(groups.calls(), ["join(g1)", "leave"]);
    }

    /// B1005: the same for a rejoin, which also reads as `superseded`, not
    /// as rejoined.
    #[test]
    fn leaving_while_a_rejoin_is_in_flight_drops_the_seat_it_takes() {
        let rig = rig();
        hold_join(&rig.groups);
        let mut rejoined = Box::pin(rig.session.rejoin("reconnect"));
        assert!(!poll_once(&mut rejoined));
        let _ = rig.session.leave().now_or_never();
        assert!(poll_once(&mut rejoined));
        assert!(!rig.session.view().active);
        assert_eq!(calls_with(&rig, "leave").len(), 1);
        let logs = rig.env.logs.borrow().clone();
        assert!(
            logs.iter().any(|l| l.contains("cause=superseded")),
            "{logs:#?}"
        );
        assert!(
            logs.iter().all(|l| !l.contains("group rejoined")),
            "{logs:#?}"
        );
    }

    /// B1005: an answer to a join that a newer join replaced is ignored, and
    /// no leave is sent: it would end the newer seat.
    #[test]
    fn a_join_answer_after_a_newer_join_began_is_ignored() {
        let groups = FakeGroups::with_groups(vec![summary()]);
        let session = Session::new(groups.clone(), TestEnv::default());
        hold_join(&groups);
        let mut first = Box::pin(session.join("g1"));
        assert!(!poll_once(&mut first));
        hold_join(&groups);
        let mut second = Box::pin(session.join("g1"));
        assert!(!poll_once(&mut second));
        assert!(poll_once(&mut first));
        assert!(groups.calls().iter().all(|c| c != "leave"));
        assert!(!session.view().active);
        assert!(poll_once(&mut second));
        assert!(session.view().active);
        assert!(groups.calls().iter().all(|c| c != "leave"));
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
    fn a_group_does_not_drive_a_player_showing_another_item() {
        let rig = rig();
        rig.session.set_watching(Some(ItemRef::Real(7)));
        rig.session.deliver(item_update(26_000, true));
        rig.env.settle();
        assert!(rig.session.drives());
        // The member opens a song while the group stays on its film.
        rig.session.set_watching(Some(ItemRef::Real(9)));
        let view = rig.session.view();
        assert!(view.active, "still a member");
        assert!(view.drives(ItemRef::Real(7)) && !view.drives(ItemRef::Real(9)));
        assert!(!rig.session.drives());
        let before = calls(&rig).len();
        rig.player.log.borrow_mut().clear();
        assert!(!rig.session.toggle_play());
        assert!(!rig.session.seek(1_000));
        rig.session.deliver(GroupUpdate::Play {
            at_server_ms: 100_000,
            position_ms: 5_000,
            by: None,
        });
        rig.env.settle();
        assert_eq!(calls(&rig).len(), before, "nothing went to the group");
        assert!(
            rig.player.log.borrow().is_empty(),
            "the group's play does not move the song"
        );
    }

    #[test]
    fn a_group_that_has_not_named_its_item_drives_the_player() {
        let rig = rig();
        assert!(rig.session.drives());
        assert!(rig.session.view().drives(ItemRef::Real(9)));
        let mut solo = rig.session.view();
        solo.active = false;
        assert!(!solo.drives(ItemRef::Real(7)));
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

    /// B664: a player that loads at the member's own resume point, after
    /// the group's catch-up seek, is put at the group's position before it
    /// says it is ready, so a member joining a paused group waits there and
    /// not 30 s away. Disarm check: sending ready at once fails this.
    #[test]
    fn a_player_ready_away_from_the_group_is_seeked_there_first() {
        let rig = rig();
        rig.session.deliver(GroupUpdate::Item {
            item: ItemRef::Real(7),
            playlist_item_id: "pl-7".into(),
            position_ms: 3_000,
            playing: false,
        });
        // The engine applied this member's own resume point after the seek.
        rig.player.position.set(30_000);
        rig.player.paused.set(true);
        rig.session.on_media(MediaEvent::CanPlay);
        rig.env.settle();
        assert!(calls_with(&rig, "ready").is_empty(), "{:?}", calls(&rig));
        assert_eq!(rig.player.position.get(), 3_000);
        assert_eq!(
            rig.env.logged("pharos: group ready held reason=away").len(),
            1
        );
        // The seek lands; now it is ready, at the group's position.
        rig.session.on_media(MediaEvent::Seeked);
        rig.env.settle();
        assert_eq!(calls_with(&rig, "ready"), ["ready(pl-7,3000,false)"]);
    }

    /// B664, the other order (Firefox): the player loaded at this member's
    /// own resume point before the group's item arrived, so no media event
    /// follows to carry the ready. The item puts the player at the group's
    /// position, and that seek's landing says ready.
    #[test]
    fn an_item_for_a_player_loaded_away_from_the_group_seeks_it_there() {
        let rig = rig();
        rig.session.set_watching(Some(ItemRef::Real(7)));
        rig.player.position.set(72_000);
        rig.player.paused.set(true);
        rig.session.deliver(GroupUpdate::Item {
            item: ItemRef::Real(7),
            playlist_item_id: "pl-7".into(),
            position_ms: 2_000,
            playing: false,
        });
        rig.env.settle();
        assert_eq!(rig.player.position.get(), 2_000);
        rig.session.on_media(MediaEvent::Seeked);
        rig.env.settle();
        assert_eq!(calls_with(&rig, "ready"), ["ready(pl-7,2000,false)"]);
    }

    /// B664, a third order (Firefox): the group's item reached the member
    /// while its join was still in flight, so the player's events up to
    /// then went to a session not yet in the group. The first clock sample
    /// of the membership puts the player at the group's position, and that
    /// seek's landing says ready.
    #[test]
    fn a_player_that_loaded_while_the_join_was_in_flight_is_put_at_the_group() {
        let groups = FakeGroups::with_groups(vec![summary()]);
        let env = TestEnv::default();
        env.now.set(100_000);
        let session = Session::new(groups.clone(), env.clone());
        let player = Rc::new(TestPlayer::default());
        session.set_watching(Some(ItemRef::Real(7)));
        session.set_player(Some(player.clone()));
        groups.push_on_join(vec![GroupUpdate::Item {
            item: ItemRef::Real(7),
            playlist_item_id: "pl-7".into(),
            position_ms: 2_000,
            playing: false,
        }]);
        let mut joined = Box::pin(session.join("g1"));
        let mut cx = std::task::Context::from_waker(futures_util::task::noop_waker_ref());
        assert!(std::future::Future::poll(joined.as_mut(), &mut cx).is_pending());
        // Before the answer: the item's seek runs, then the engine applies
        // this member's own resume point and its events find no group.
        env.settle();
        player.position.set(72_000);
        player.paused.set(true);
        session.on_media(MediaEvent::Seeked);
        session.on_media(MediaEvent::CanPlay);
        player.has_data.set(true);
        assert!(matches!(
            std::future::Future::poll(joined.as_mut(), &mut cx),
            std::task::Poll::Ready(Ok(_))
        ));
        env.settle();
        assert_eq!(player.position.get(), 2_000, "{:?}", player.log.borrow());
        session.on_media(MediaEvent::Seeked);
        env.settle();
        let ready: Vec<String> = groups
            .calls()
            .into_iter()
            .filter(|c| c.starts_with("ready"))
            .collect();
        assert_eq!(ready, ["ready(pl-7,2000,false)"]);
    }

    /// B664, the root of it: a player that loads while the group plays an
    /// item starts at the group's position, not at this member's own resume
    /// point, so no late resume seek can carry it away after its ready.
    #[test]
    fn a_player_loading_the_groups_item_starts_at_the_group() {
        let rig = rig();
        assert_eq!(rig.session.start_for(ItemRef::Real(7)), None);
        rig.session.deliver(GroupUpdate::Item {
            item: ItemRef::Real(7),
            playlist_item_id: "pl-7".into(),
            position_ms: 1_900,
            playing: false,
        });
        assert_eq!(rig.session.start_for(ItemRef::Real(7)), Some(1_900));
        assert_eq!(rig.session.start_for(ItemRef::Real(8)), None);
        rig.session.leave().now_or_never();
        assert_eq!(rig.session.start_for(ItemRef::Real(7)), None);
    }

    /// B665 in Firefox: after a reload the player loaded while the rejoin
    /// was in flight, so its `canplay` reached a session not yet in the
    /// group. The group's item then put it where it already was (no seek, so
    /// no `seeked`), and nothing else came to carry the ready: the group
    /// waited on this member for good. A loaded player says ready as soon
    /// as the item it owes one for arrives.
    #[test]
    fn a_player_loaded_before_the_rejoin_says_ready_for_the_groups_item() {
        let groups = FakeGroups::with_groups(vec![summary()]);
        let env = TestEnv::default();
        env.now.set(100_000);
        let session = Session::new(groups.clone(), env.clone());
        let player = Rc::new(TestPlayer::default());
        player.position.set(400);
        player.paused.set(true);
        session.set_watching(Some(ItemRef::Real(7)));
        session.set_player(Some(player.clone()));
        session.on_media(MediaEvent::CanPlay);
        player.has_data.set(true);
        assert!(matches!(session.join("g1").now_or_never(), Some(Ok(_))));
        env.settle();
        session.deliver(GroupUpdate::Item {
            item: ItemRef::Real(7),
            playlist_item_id: "pl-7".into(),
            position_ms: 369,
            playing: false,
        });
        env.settle();
        let ready: Vec<String> = groups
            .calls()
            .into_iter()
            .filter(|c| c.starts_with("ready"))
            .collect();
        assert_eq!(
            ready,
            ["ready(pl-7,400,false)"],
            "{:?}",
            player.log.borrow()
        );
    }

    /// A loaded player handed to a new session (a second group joined after
    /// the first was left) says ready for an item that needs no seek: the
    /// element's own state says it has data, whatever session saw its events.
    #[test]
    fn a_loaded_player_reused_by_a_new_session_says_ready() {
        let (seat, _) = seat();
        let env = TestEnv::default();
        env.now.set(100_000);
        let video = Rc::new(TestPlayer::default());
        video.position.set(400);
        video.paused.set(true);
        video.has_data.set(true);
        seat.attach(ItemRef::Real(7), video.clone());
        let session = seat.session(|| new_session(&env));
        assert!(matches!(session.join("g1").now_or_never(), Some(Ok(_))));
        env.settle();
        session.deliver(GroupUpdate::Item {
            item: ItemRef::Real(7),
            playlist_item_id: "pl-7".into(),
            position_ms: 369,
            playing: false,
        });
        env.settle();
        let ready: Vec<String> = session
            .client
            .calls()
            .into_iter()
            .filter(|c| c.starts_with("ready"))
            .collect();
        assert_eq!(ready, ["ready(pl-7,400,false)"]);
    }

    /// Saying a ready from a loaded player must not end the grace of a seek
    /// this session made itself: that seek's `seeked` is still ours, not the
    /// viewer's, and must not move the group.
    #[test]
    fn a_ready_from_a_loaded_player_keeps_the_self_seek_grace() {
        let rig = rig();
        rig.session.set_watching(Some(ItemRef::Real(7)));
        rig.player.position.set(400);
        rig.player.paused.set(true);
        rig.player.has_data.set(true);
        rig.session.inner.borrow_mut().self_seek_ms = Some(99_900);
        rig.session.deliver(GroupUpdate::Item {
            item: ItemRef::Real(7),
            playlist_item_id: "pl-7".into(),
            position_ms: 369,
            playing: false,
        });
        rig.env.settle();
        assert_eq!(calls_with(&rig, "ready"), ["ready(pl-7,400,false)"]);
        assert_eq!(rig.session.inner.borrow().self_seek_ms, Some(99_900));
    }

    /// Each new element gets its own one hold: a player remounted after a
    /// hold that loads away from the group again is seeked again, not said
    /// ready where it stands (B664).
    #[test]
    fn a_remounted_player_away_from_the_group_is_held_again() {
        let rig = rig();
        rig.session.deliver(GroupUpdate::Item {
            item: ItemRef::Real(7),
            playlist_item_id: "pl-7".into(),
            position_ms: 3_000,
            playing: false,
        });
        rig.player.position.set(30_000);
        rig.session.on_media(MediaEvent::CanPlay);
        rig.env.settle();
        let again = Rc::new(TestPlayer::default());
        again.position.set(30_000);
        rig.session.set_player(Some(again.clone()));
        again.position.set(30_000);
        rig.session.on_media(MediaEvent::CanPlay);
        rig.env.settle();
        assert!(calls_with(&rig, "ready").is_empty(), "{:?}", calls(&rig));
        assert_eq!(again.position.get(), 3_000);
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

    /// B807: a command the group never heard says so on the activity toast
    /// instead of leaving the click looking dead. Disarm check: dropping the
    /// `command_failed` calls fails all three.
    #[test]
    fn a_failed_command_tells_the_viewer() {
        let rig = rig();
        rig.groups.refuse_seek();
        assert!(rig.session.seek(5_000));
        let mut live: Vec<LocalBoxFuture<'static, ()>> = Vec::new();
        pump(&rig.env, &mut live);
        let lines = rig.session.view().log.lines();
        assert_eq!(
            lines.last().map(String::as_str),
            Some("Couldn't reach the group. Try again.")
        );
        assert_eq!(
            rig.env
                .logged("pharos: group command failed kind=seek(5000)"),
            ["pharos: group command failed kind=seek(5000): connection lost"]
        );
    }

    /// A burst of failures (an outage fails every queued command) shows one
    /// line per toast, and a failure after the toast has gone shows again.
    #[test]
    fn a_burst_of_failed_commands_shows_one_line() {
        let rig = rig();
        rig.groups.refuse_seek();
        let failed = |rig: &Rig| {
            rig.session
                .view()
                .log
                .lines()
                .iter()
                .filter(|l| l.as_str() == "Couldn't reach the group. Try again.")
                .count()
        };
        let mut live: Vec<LocalBoxFuture<'static, ()>> = Vec::new();
        for at in [5_000, 6_000, 7_000] {
            assert!(rig.session.seek(at));
            pump(&rig.env, &mut live);
        }
        assert_eq!(failed(&rig), 1, "{:?}", rig.session.view().log.lines());
        rig.env.now.set(rig.env.now.get() + 4_000);
        assert!(rig.session.seek(8_000));
        pump(&rig.env, &mut live);
        assert_eq!(failed(&rig), 2);
    }

    #[test]
    fn a_command_that_times_out_tells_the_viewer() {
        let rig = rig();
        rig.groups.hold_buffering();
        rig.session.on_media(MediaEvent::Waiting);
        let mut live: Vec<LocalBoxFuture<'static, ()>> = Vec::new();
        pump(&rig.env, &mut live);
        rig.env.timeout_elapsed.set(true);
        pump(&rig.env, &mut live);
        assert_eq!(
            rig.session.view().log.lines().last().map(String::as_str),
            Some("Couldn't reach the group. Try again.")
        );
    }

    #[test]
    fn a_follow_toggle_the_group_missed_tells_the_viewer() {
        let rig = rig();
        rig.groups.refuse_follow();
        rig.session.follow_tracks(false);
        let mut live: Vec<LocalBoxFuture<'static, ()>> = Vec::new();
        pump(&rig.env, &mut live);
        assert_eq!(
            rig.session.view().log.lines().last().map(String::as_str),
            Some("Couldn't reach the group. Try again.")
        );
    }

    /// Joining again resets the membership, not the queue a running drain
    /// task works through; a command of the old membership still queued is
    /// dropped when reached, never sent to the new group.
    #[test]
    fn a_new_membership_keeps_the_drain_and_drops_the_old_commands() {
        let rig = rig();
        rig.groups.hold_buffering();
        rig.session.on_media(MediaEvent::Waiting);
        assert!(rig.session.seek(5_000));
        let mut live: Vec<LocalBoxFuture<'static, ()>> = Vec::new();
        pump(&rig.env, &mut live);
        assert!(rig.session.inner.borrow().in_flight);
        let epoch = rig.session.inner.borrow().epoch;
        // The fake ends a group its last member leaves, so this join is
        // refused; the new membership was still prepared (a new epoch).
        assert!(
            rig.session.join("g1").now_or_never().is_some(),
            "the join runs to its end"
        );
        assert!(rig.session.inner.borrow().epoch > epoch);
        {
            let inner = rig.session.inner.borrow();
            assert!(inner.draining && inner.in_flight);
            assert_eq!(inner.commands.len(), 1, "the old seek is still queued");
        }
        rig.groups.release_buffering(0);
        pump(&rig.env, &mut live);
        assert!(
            calls_with(&rig, "seek").is_empty(),
            "the old seek is dropped"
        );
        assert_eq!(
            rig.env
                .logged("pharos: group command dropped kind=seek(5000)")
                .len(),
            1
        );
        let inner = rig.session.inner.borrow();
        assert!(!inner.draining && !inner.in_flight && inner.commands.is_empty());
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
        assert!(rig.session.tick().now_or_never().is_some());
        rig.env.settle();
        assert!(calls_with(&rig, "buffering").is_empty());

        rig.env.now.set(105_000);
        assert!(rig.session.tick().now_or_never().is_some());
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
        for now in [112_000, 114_000, 115_000, 125_000] {
            rig.env.now.set(now);
            assert!(rig.session.tick().now_or_never().is_some());
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

    /// A wait the video recovers from with no `canplay` or `playing` still
    /// ends: the group is not held paused while this member plays on.
    #[test]
    fn buffering_ends_once_the_playhead_moves_on_its_own() {
        let rig = playing_rig();
        rig.env.now.set(200_000);
        rig.player.position.set(26_761);
        rig.session.on_media(MediaEvent::Waiting);
        rig.env.settle();

        // Under the threshold: a gap nudge, not playback.
        rig.player.position.set(27_000);
        rig.session.on_media(MediaEvent::TimeUpdate);
        rig.env.settle();
        assert_eq!(calls_with(&rig, "buffering"), ["buffering(,26761,true)"]);

        rig.player.position.set(27_261);
        rig.session.on_media(MediaEvent::TimeUpdate);
        rig.session.on_media(MediaEvent::TimeUpdate);
        rig.env.settle();
        assert_eq!(
            calls_with(&rig, "buffering"),
            ["buffering(,26761,true)", "buffering(,27261,false)"]
        );
        assert_eq!(
            rig.env
                .logged("pharos: group buffering ended reason=playhead_moved from_ms=26761 position_ms=27261")
                .len(),
            1
        );
    }

    /// A member reported as buffering is left to load where it is: a drift
    /// seek moves it onto segments it has not fetched and it falls further
    /// behind.
    #[test]
    fn a_buffering_member_is_not_drift_seeked() {
        let rig = playing_rig();
        rig.env.now.set(200_000);
        rig.player.position.set(100_000);
        rig.session.on_media(MediaEvent::Waiting);
        rig.env.settle();
        rig.env.now.set(210_000);
        rig.session.check_drift();
        assert!(
            !rig.player
                .log
                .borrow()
                .iter()
                .any(|l| l.starts_with("seek(")),
            "got {:?}",
            rig.player.log.borrow()
        );
    }

    /// The group's own freeze pauses the video; a paused playhead that
    /// reports where it sits has not recovered.
    #[test]
    fn a_paused_video_does_not_end_buffering_by_time() {
        let rig = playing_rig();
        rig.env.now.set(200_000);
        rig.player.position.set(26_761);
        rig.session.on_media(MediaEvent::Waiting);
        rig.player.paused.set(true);
        rig.player.position.set(40_000);
        rig.session.on_media(MediaEvent::TimeUpdate);
        rig.env.settle();
        assert_eq!(calls_with(&rig, "buffering"), ["buffering(,26761,true)"]);
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
        assert!(rig.session.tick().now_or_never().is_some());
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

    /// B660: a driven player left paused while the group plays (a track
    /// change remounts the video, and the new one never starts) is put back
    /// where the group is and started, once the server's command has settled.
    #[test]
    fn a_player_paused_while_the_group_plays_is_resumed_at_the_group() {
        let rig = playing_rig();
        rig.player.paused.set(true);
        rig.player.position.set(5_000);
        // Inside the settle window the server's own command may still land.
        rig.env.now.set(102_999);
        rig.session.check_drift();
        assert!(
            rig.player.log.borrow().is_empty(),
            "{:?}",
            rig.player.log.borrow()
        );

        rig.env.now.set(110_000);
        rig.session.check_drift();
        assert_eq!(*rig.player.log.borrow(), ["seek(20000)", "play"]);
        assert_eq!(
            rig.env
                .logged("pharos: group resume reason=paused_while_group_plays position_ms=5000 expected_ms=20000")
                .len(),
            1
        );
    }

    /// A player that stays paused (the browser refused the `play()`) is not
    /// asked again every second: once per settle window.
    #[test]
    fn a_refused_resume_is_retried_once_per_settle_window() {
        let rig = playing_rig();
        rig.env.now.set(110_000);
        rig.player.paused.set(true);
        rig.session.check_drift();
        rig.player.paused.set(true);
        let plays = || {
            rig.player
                .log
                .borrow()
                .iter()
                .filter(|l| *l == "play")
                .count()
        };
        assert_eq!(plays(), 1);
        rig.env.now.set(112_999);
        rig.session.check_drift();
        assert_eq!(plays(), 1);
        rig.env.now.set(113_000);
        rig.session.check_drift();
        assert_eq!(plays(), 2);
    }

    /// A video that played to its end is not restarted at the group's
    /// position past it: the group's clock runs on until the next item.
    #[test]
    fn a_player_at_its_end_is_not_resumed() {
        let rig = playing_rig();
        rig.env.now.set(110_000);
        rig.player.paused.set(true);
        rig.player.ended.set(true);
        rig.session.check_drift();
        assert!(
            rig.player.log.borrow().is_empty(),
            "{:?}",
            rig.player.log.borrow()
        );
    }

    /// Paused because the group is paused, or while the video is buffering,
    /// the player is left alone.
    #[test]
    fn a_paused_player_is_left_alone_when_the_group_is_paused_or_it_buffers() {
        let rig = rig();
        rig.session.deliver(GroupUpdate::Pause {
            at_server_ms: 100_000,
            position_ms: 10_000,
            by: None,
        });
        rig.env.settle();
        rig.player.log.borrow_mut().clear();
        rig.player.paused.set(true);
        rig.env.now.set(110_000);
        rig.session.check_drift();
        assert!(
            rig.player.log.borrow().is_empty(),
            "{:?}",
            rig.player.log.borrow()
        );

        let rig = playing_rig();
        rig.env.now.set(110_000);
        rig.player.position.set(20_000);
        rig.session.on_media(MediaEvent::Waiting);
        rig.env.settle();
        rig.player.log.borrow_mut().clear();
        rig.player.paused.set(true);
        rig.session.check_drift();
        assert!(
            rig.player.log.borrow().is_empty(),
            "{:?}",
            rig.player.log.borrow()
        );
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

    /// B761: a group started from the player while the video plays must not
    /// seek it. The server strikes the group's play at the place the group
    /// was created, a moment after the video, which played on, has passed
    /// it; a seek back that far restarts the video's buffering, a visible
    /// stutter (worst on the HLS engines, which flush). Small gaps are the
    /// drift check's to close, by rate, not by seeking.
    #[test]
    fn a_group_started_while_the_video_plays_does_not_seek_it() {
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
        let sink = session.client.sinks().remove(0);
        sink(GroupUpdate::Item {
            item: ItemRef::Real(7),
            playlist_item_id: "p1".into(),
            position_ms: 42_000,
            playing: true,
        });
        env.settle();
        // The readiness gate resolves 400 ms later; the video played on.
        env.now.set(100_400);
        video.position.set(42_400);
        session.on_media(MediaEvent::TimeUpdate);
        sink(GroupUpdate::Play {
            at_server_ms: 100_400,
            position_ms: 42_000,
            by: None,
        });
        env.settle();
        assert!(
            video.log.borrow().iter().all(|l| !l.starts_with("seek(")),
            "no seek of a playing video for a 400 ms gap: {:?}",
            video.log.borrow()
        );
        assert!(!video.paused(), "{:?}", video.log.borrow());
    }

    /// The other side of B761: a paused member is still put exactly where
    /// the group plays from, and a playing one far away is still seeked.
    #[test]
    fn a_play_still_seeks_a_paused_or_far_away_video() {
        let rig = rig();
        rig.session.set_watching(Some(ItemRef::Real(7)));
        rig.session.deliver(GroupUpdate::Item {
            item: ItemRef::Real(7),
            playlist_item_id: "pl-7".into(),
            position_ms: 10_000,
            playing: false,
        });
        rig.env.settle();
        rig.player.paused.set(true);
        rig.player.position.set(10_400);
        rig.session.deliver(GroupUpdate::Play {
            at_server_ms: rig.env.now.get(),
            position_ms: 10_000,
            by: None,
        });
        rig.env.settle();
        assert_eq!(
            rig.player.position.get(),
            10_000,
            "{:?}",
            rig.player.log.borrow()
        );
        // A second off is too far to close by rate soon: seeked.
        rig.player.position.set(11_000);
        rig.session.deliver(GroupUpdate::Play {
            at_server_ms: rig.env.now.get(),
            position_ms: 10_000,
            by: None,
        });
        rig.env.settle();
        assert_eq!(
            rig.player.position.get(),
            10_000,
            "{:?}",
            rig.player.log.borrow()
        );
    }

    /// B761: the group's item for the video already playing, arriving a
    /// moment after it was made, re-states the group's place; a small gap is
    /// not seeked, and the skip says why.
    #[test]
    fn the_groups_item_for_a_playing_video_close_by_is_not_seeked() {
        let rig = rig();
        rig.session.set_watching(Some(ItemRef::Real(7)));
        rig.player.position.set(42_300);
        rig.session.deliver(GroupUpdate::Item {
            item: ItemRef::Real(7),
            playlist_item_id: "pl-7".into(),
            position_ms: 42_000,
            playing: true,
        });
        rig.env.settle();
        assert!(
            rig.player
                .log
                .borrow()
                .iter()
                .all(|l| !l.starts_with("seek(")),
            "{:?}",
            rig.player.log.borrow()
        );
        assert_eq!(
            rig.env
                .logged("pharos: group seek skipped reason=small_gap source=align")
                .len(),
            1
        );
    }

    /// The paused side of B761: a video still playing when it learns the
    /// group is paused is put exactly at the group's place.
    #[test]
    fn a_playing_video_in_a_paused_group_is_put_exactly() {
        let rig = rig();
        rig.session.set_watching(Some(ItemRef::Real(7)));
        rig.player.position.set(60_900);
        rig.session.deliver(GroupUpdate::Item {
            item: ItemRef::Real(7),
            playlist_item_id: "pl-7".into(),
            position_ms: 60_000,
            playing: false,
        });
        rig.env.settle();
        assert_eq!(
            rig.player.position.get(),
            60_000,
            "{:?}",
            rig.player.log.borrow()
        );
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

    fn on_entry(rig: &Rig, item: u64, playlist_item_id: &str) {
        rig.session.deliver(GroupUpdate::Item {
            item: ItemRef::Real(item),
            playlist_item_id: playlist_item_id.into(),
            position_ms: 0,
            playing: true,
        });
    }

    /// V986: cancelling "Up next" in a group holds the end for the group,
    /// naming the entry the viewer is on; out of a group it stays local.
    #[test]
    fn a_cancelled_up_next_holds_the_end_for_the_group() {
        let rig = rig();
        assert!(
            !rig.session.hold_next(),
            "no entry yet: the cancel stays here"
        );
        on_entry(&rig, 7, "pl-7");
        assert!(!rig.session.next_held());
        assert!(rig.session.hold_next());
        rig.env.settle();
        assert_eq!(calls_with(&rig, "hold_next"), ["hold_next(pl-7)"]);
        assert!(rig.session.next_held(), "the group's update came back");
        assert_eq!(rig.session.view().log.lines(), ["ME cancelled Up next"]);

        let alone = Session::new(FakeGroups::default(), TestEnv::default());
        assert!(!alone.hold_next());
        assert!(!alone.next_held());
    }

    /// V986: another member's hold stops this player's "Up next" on that
    /// entry only: once the group moves on, the next episode counts again.
    #[test]
    fn another_members_hold_lasts_until_the_group_moves_on() {
        let rig = rig();
        on_entry(&rig, 7, "pl-7");
        rig.session.deliver(GroupUpdate::NextHeld {
            playlist_item_id: "pl-6".into(),
            by: Some("me".into()),
        });
        assert!(!rig.session.next_held(), "a hold on an entry already left");
        rig.session.deliver(GroupUpdate::NextHeld {
            playlist_item_id: "pl-7".into(),
            by: Some("me".into()),
        });
        assert!(rig.session.next_held());
        on_entry(&rig, 8, "pl-8");
        assert!(!rig.session.next_held());
        assert_eq!(rig.session.playlist_item_id(), "pl-8");
        on_entry(&rig, 7, "pl-7");
        assert!(
            !rig.session.next_held(),
            "back on the held entry, it counts again"
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
