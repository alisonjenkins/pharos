//! Per-user event streams for the native API's `Events.subscribe` (017 US2,
//! data-model §Events, ADR-0094).
//!
//! Services already announce every change on [`AppState::bus`], the fan-out
//! `/socket` reads. [`EventLog::start`] adds one reader of that bus which
//! turns each announcement into typed events, gives each one the next `seq`
//! of the user stream it belongs to, and keeps the newest of them in a bounded
//! replay window. A subscriber reads the window from its `afterSeq`, then
//! follows live events, and catches up from the window again if it falls
//! behind the live channel.
//!
//! A stream exists only for a user who has subscribed since the server
//! started, so a server nobody subscribes to keeps nothing. A stream's first
//! `seq` is the wall clock in microseconds when it was made, so a `seq` from
//! before a restart is always below the new stream's floor and resumes as one
//! `resync` rather than replaying unrelated events.
//!
//! [`AppState::bus`]: crate::state::AppState::bus

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, Once, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pharos_core::{MediaId, UserId};
use pharos_jellyfin_api::dto::WireId;
use tokio::sync::broadcast;

use crate::state::SocketBroadcast;

/// Events a user stream keeps for replay (data-model §Events).
pub const REPLAY_EVENTS: usize = 10_000;
/// How long a user stream keeps an event for replay (data-model §Events).
pub const REPLAY_AGE: Duration = Duration::from_secs(15 * 60);
/// Live events a user's channel buffers before that user's slowest
/// subscriber lags. A lagged subscriber loses nothing: it catches up from the
/// replay window. Each subscribed user's channel holds this many slots.
const LIVE_CAPACITY: usize = 256;

/// `pharos_rpc_events_total{topic,outcome}`.
pub const EVENTS_TOTAL: &str = "pharos_rpc_events_total";
/// `pharos_rpc_subscriptions{topic}`.
pub const SUBSCRIPTIONS: &str = "pharos_rpc_subscriptions";
/// `pharos_rpc_event_lag_seconds{topic}`: from the event entering its stream
/// to the subscriber's sink acknowledging it.
pub const EVENT_LAG_SECONDS: &str = "pharos_rpc_event_lag_seconds";

/// What a subscriber may ask for. Resync is not a topic: every subscriber
/// receives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Topic {
    UserData,
    Library,
    Task,
}

impl Topic {
    pub const ALL: [Topic; 3] = [Topic::UserData, Topic::Library, Topic::Task];

    /// The name a client passes in `subscribe`'s `topics`.
    pub fn wire(self) -> &'static str {
        match self {
            Topic::UserData => "userData",
            Topic::Library => "library",
            Topic::Task => "task",
        }
    }

    pub fn from_wire(name: &str) -> Option<Topic> {
        Topic::ALL.into_iter().find(|t| t.wire() == name)
    }

    /// The metric label.
    pub fn label(self) -> &'static str {
        match self {
            Topic::UserData => "user_data",
            Topic::Library => "library",
            Topic::Task => "task",
        }
    }
}

/// How an event ended up, for `pharos_rpc_events_total`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The subscriber's sink acknowledged it.
    Delivered,
    /// The sink went away or failed before acknowledging it.
    Dropped,
    /// The sink answered it with an error.
    Rejected,
    /// A resync was decided: a subscriber's `afterSeq` was outside the
    /// window, or the bus outran this log (once for all streams). Its push
    /// counts under `topic="resync"` with the push's own outcome.
    Resync,
    /// A bus announcement this log could not read (it names the value in a
    /// WARN).
    Undecodable,
}

impl Outcome {
    pub const ALL: [Outcome; 5] = [
        Outcome::Delivered,
        Outcome::Dropped,
        Outcome::Rejected,
        Outcome::Resync,
        Outcome::Undecodable,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Outcome::Delivered => "delivered",
            Outcome::Dropped => "dropped",
            Outcome::Rejected => "rejected",
            Outcome::Resync => "resync",
            Outcome::Undecodable => "undecodable",
        }
    }
}

/// The `topic` label of an event: its topic, or `resync`.
pub fn topic_label(payload: &Payload) -> &'static str {
    payload.topic().map_or("resync", Topic::label)
}

pub fn count(topic: &'static str, outcome: Outcome, n: u64) {
    metrics::counter!(EVENTS_TOTAL, "topic" => topic, "outcome" => outcome.label()).increment(n);
}

/// A user's state on one item, as `UserState` carries it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserState {
    pub played: bool,
    pub favorite: bool,
    pub position_ms: u64,
    pub play_count: u32,
}

/// What an event says.
#[derive(Clone, Debug, PartialEq)]
pub enum Payload {
    UserData {
        id: WireId,
        state: UserState,
    },
    /// Both lists empty means "something changed": refetch library views.
    Library {
        added: Vec<MediaId>,
        removed: Vec<MediaId>,
    },
    /// `progress` is 0.0 to 1.0. `result` is what a finished library scan
    /// did; `None` while it runs and for other tasks.
    Task {
        id: String,
        progress: f32,
        state: &'static str,
        result: Option<crate::scan_tasks::ScanTally>,
    },
    /// Events were lost: refetch everything cached.
    Resync,
}

impl Payload {
    pub fn topic(&self) -> Option<Topic> {
        match self {
            Payload::UserData { .. } => Some(Topic::UserData),
            Payload::Library { .. } => Some(Topic::Library),
            Payload::Task { .. } => Some(Topic::Task),
            Payload::Resync => None,
        }
    }
}

/// One event in one user's stream. The payload is shared: a library change
/// is stored once however many streams hold it.
#[derive(Clone, Debug)]
pub struct Event {
    pub seq: u64,
    pub at: Instant,
    pub payload: Arc<Payload>,
}

struct Stream {
    /// The last `seq` given out.
    head: u64,
    /// Events up to and including this `seq` can no longer be replayed.
    pruned_through: u64,
    window: VecDeque<Event>,
    /// This user's live events; one channel per user, so a burst for one
    /// user never wakes or lags another's subscribers.
    live: broadcast::Sender<Event>,
}

impl Stream {
    fn new(floor: u64) -> Self {
        Self {
            head: floor,
            pruned_through: floor,
            window: VecDeque::new(),
            live: broadcast::channel(LIVE_CAPACITY).0,
        }
    }

    fn push(&mut self, payload: Arc<Payload>, now: Instant, bounds: Bounds) {
        self.head = self.head.saturating_add(1);
        let event = Event {
            seq: self.head,
            at: now,
            payload,
        };
        self.window.push_back(event.clone());
        self.prune(now, bounds);
        let _ = self.live.send(event);
    }

    fn prune(&mut self, now: Instant, bounds: Bounds) {
        while let Some(front) = self.window.front() {
            let full = self.window.len() > bounds.events;
            let old = now.saturating_duration_since(front.at) > bounds.age;
            if !(full || old) {
                break;
            }
            self.pruned_through = front.seq;
            self.window.pop_front();
        }
    }

    /// What a subscriber resuming after `after` must be sent first.
    fn since(&self, after: u64) -> Catchup {
        if after == 0 {
            return Catchup::Live { head: self.head };
        }
        if after < self.pruned_through || after > self.head {
            return Catchup::Resync { head: self.head };
        }
        Catchup::Replay(
            self.window
                .iter()
                .filter(|e| e.seq > after)
                .cloned()
                .collect(),
        )
    }
}

/// Where a subscriber starts.
#[derive(Debug)]
pub enum Catchup {
    /// Nothing to replay; follow live events after `head`.
    Live { head: u64 },
    /// Every event after the subscriber's `afterSeq`, oldest first.
    Replay(Vec<Event>),
    /// The `afterSeq` is outside the window: send one resync carrying `head`,
    /// then follow live events.
    Resync { head: u64 },
}

#[derive(Clone, Copy, Debug)]
struct Bounds {
    events: usize,
    age: Duration,
}

/// Every user stream on this server, and the live channel subscribers follow.
pub struct EventLog {
    streams: Mutex<HashMap<UserId, Stream>>,
    bounds: Bounds,
    started: Once,
}

impl Default for EventLog {
    fn default() -> Self {
        Self::with_window(REPLAY_EVENTS, REPLAY_AGE)
    }
}

impl std::fmt::Debug for EventLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventLog")
            .field("users", &self.lock().len())
            .finish_non_exhaustive()
    }
}

/// Wall-clock microseconds: a new stream's floor, above any `seq` an earlier
/// run of this server gave out unless the clock went backwards.
fn wall_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(1, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX / 2))
}

impl EventLog {
    /// A log whose streams keep `events` events for at most `age`.
    pub fn with_window(events: usize, age: Duration) -> Self {
        Self {
            streams: Mutex::new(HashMap::new()),
            bounds: Bounds { events, age },
            started: Once::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<UserId, Stream>> {
        self.streams.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts reading `bus` into this log, once; later calls do nothing. The
    /// bus is subscribed before this returns, so nothing announced after it is
    /// missed.
    pub fn start(self: &Arc<Self>, bus: &broadcast::Sender<SocketBroadcast>) {
        self.started.call_once(|| {
            let rx = bus.subscribe();
            let log = Arc::clone(self);
            tokio::spawn(async move { log.follow(rx).await });
            tracing::info!("native events: following the server's announcements");
        });
    }

    async fn follow(self: Arc<Self>, mut rx: broadcast::Receiver<SocketBroadcast>) {
        loop {
            match rx.recv().await {
                Ok(announcement) => self.translate(announcement),
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::warn!(
                        missed,
                        "native events: fell behind the server's announcements; every stream resyncs"
                    );
                    count("resync", Outcome::Resync, 1);
                    self.publish_all(Payload::Resync);
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    }

    /// Turns one bus announcement into events. Those `/socket` alone needs
    /// (sessions, remote control, activity) are not events here.
    pub fn translate(&self, announcement: SocketBroadcast) {
        match announcement {
            SocketBroadcast::UserDataChanged { user_id, entries } => {
                let Some(user) = parse_user(&user_id) else {
                    tracing::warn!(user_id = %user_id, "native events: user data change for an unreadable user id");
                    count(Topic::UserData.label(), Outcome::Undecodable, 1);
                    return;
                };
                let payloads = entries
                    .iter()
                    .filter_map(|entry| {
                        let payload = user_data_payload(entry);
                        if payload.is_none() {
                            tracing::warn!(entry = %entry, "native events: unreadable user data entry");
                            count(Topic::UserData.label(), Outcome::Undecodable, 1);
                        }
                        payload
                    })
                    .collect();
                self.publish(user, payloads);
            }
            SocketBroadcast::LibraryChanged { added, removed } => {
                let ids = |list: &[String]| -> Vec<MediaId> {
                    list.iter()
                        .filter_map(|s| {
                            let id = s.parse().ok();
                            if id.is_none() {
                                tracing::warn!(id = %s, "native events: unreadable library item id");
                                count(Topic::Library.label(), Outcome::Undecodable, 1);
                            }
                            id
                        })
                        .collect()
                };
                self.publish_all(Payload::Library {
                    added: ids(&added),
                    removed: ids(&removed),
                });
            }
            SocketBroadcast::RefreshProgress { item_id, progress } => {
                let done = progress >= 100.0;
                #[allow(clippy::cast_possible_truncation)]
                let fraction = (progress / 100.0).clamp(0.0, 1.0) as f32;
                self.publish_all(Payload::Task {
                    id: item_id,
                    progress: fraction,
                    state: if done { "done" } else { "running" },
                    result: None,
                });
            }
            SocketBroadcast::ScanProgress { progress, end } => {
                #[allow(clippy::cast_possible_truncation)]
                let fraction = (progress / 100.0).clamp(0.0, 1.0) as f32;
                self.publish_all(Payload::Task {
                    id: crate::scan_tasks::TASK_REFRESH_LIBRARY.to_owned(),
                    progress: if end.is_some() { 1.0 } else { fraction },
                    state: end.map_or("running", |(outcome, _)| outcome.task_state()),
                    result: end.map(|(_, tally)| tally),
                });
            }
            SocketBroadcast::SessionCommand { .. }
            | SocketBroadcast::DeviceCommand { .. }
            | SocketBroadcast::CastChanged
            | SocketBroadcast::SessionsChanged
            | SocketBroadcast::PlaybackProgress { .. }
            | SocketBroadcast::ActivityLogEntry { .. } => {}
        }
    }

    /// Appends `payloads` to `user`'s stream, if they have one, and sends them
    /// live.
    pub fn publish(&self, user: UserId, payloads: Vec<Payload>) {
        if payloads.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut streams = self.lock();
        let Some(stream) = streams.get_mut(&user) else {
            return;
        };
        for payload in payloads {
            stream.push(Arc::new(payload), now, self.bounds);
        }
    }

    /// Appends `payload` to every stream and sends it live.
    pub fn publish_all(&self, payload: Payload) {
        let now = Instant::now();
        let payload = Arc::new(payload);
        let mut streams = self.lock();
        for stream in streams.values_mut() {
            stream.push(Arc::clone(&payload), now, self.bounds);
        }
    }

    /// Where `user` resumes after `after`, and the live channel from that
    /// point on. Both are taken under one lock, so no event falls between
    /// them. Makes the user's stream if they have none.
    pub fn subscribe(&self, user: UserId, after: u64) -> (Catchup, broadcast::Receiver<Event>) {
        let mut streams = self.lock();
        let stream = streams
            .entry(user)
            .or_insert_with(|| Stream::new(wall_micros()));
        stream.prune(Instant::now(), self.bounds);
        let rx = stream.live.subscribe();
        (counted(stream.since(after)), rx)
    }

    /// Where `user` resumes after `after`, for a subscriber that lagged its
    /// live channel. What its receiver still holds is skipped by `seq`.
    pub fn catch_up(&self, user: UserId, after: u64) -> Catchup {
        let mut streams = self.lock();
        let stream = streams
            .entry(user)
            .or_insert_with(|| Stream::new(wall_micros()));
        stream.prune(Instant::now(), self.bounds);
        counted(stream.since(after))
    }

    /// The last `seq` of `user`'s stream, `None` when they have none.
    pub fn head(&self, user: UserId) -> Option<u64> {
        self.lock().get(&user).map(|s| s.head)
    }
}

/// Counts a resync where it is decided: once per subscriber told to refetch.
fn counted(catchup: Catchup) -> Catchup {
    if matches!(catchup, Catchup::Resync { .. }) {
        count("resync", Outcome::Resync, 1);
    }
    catchup
}

fn parse_user(simple: &str) -> Option<UserId> {
    uuid::Uuid::parse_str(simple).ok().map(UserId)
}

/// One serialized `UserItemDataDto` (the bus's shape) as a payload.
fn user_data_payload(entry: &serde_json::Value) -> Option<Payload> {
    let id = WireId::parse(entry.get("ItemId")?.as_str()?)?;
    let flag = |k: &str| entry.get(k).and_then(serde_json::Value::as_bool);
    let ticks = entry
        .get("PlaybackPositionTicks")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let play_count = entry
        .get("PlayCount")
        .and_then(serde_json::Value::as_u64)
        .map_or(0, |n| u32::try_from(n).unwrap_or(u32::MAX));
    Some(Payload::UserData {
        id,
        state: UserState {
            played: flag("Played")?,
            favorite: flag("IsFavorite")?,
            position_ms: ticks / 10_000,
            play_count,
        },
    })
}

/// Zeroes every `pharos_rpc_events_total` and `pharos_rpc_subscriptions`
/// series, so a dashboard shows 0 rather than no data before the first event.
pub fn register_metrics() {
    let topics = Topic::ALL.map(Topic::label);
    for topic in topics.iter().copied().chain(["resync"]) {
        for outcome in Outcome::ALL {
            metrics::counter!(EVENTS_TOTAL, "topic" => topic, "outcome" => outcome.label())
                .absolute(0);
        }
    }
    for topic in topics {
        metrics::gauge!(SUBSCRIPTIONS, "topic" => topic).set(0.0);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn user(n: u8) -> UserId {
        UserId(uuid::Uuid::from_bytes([n; 16]))
    }

    fn played(id: MediaId) -> Payload {
        Payload::UserData {
            id: WireId::Real(id),
            state: UserState {
                played: true,
                favorite: false,
                position_ms: 0,
                play_count: 1,
            },
        }
    }

    fn seqs(c: &Catchup) -> Vec<u64> {
        match c {
            Catchup::Replay(events) => events.iter().map(|e| e.seq).collect(),
            other => panic!("expected a replay, got {other:?}"),
        }
    }

    #[test]
    fn seq_is_per_user_and_counts_up_by_one() {
        let log = EventLog::default();
        let (Catchup::Live { head: a0 }, _rx) = log.subscribe(user(1), 0) else {
            panic!("fresh subscribe is live")
        };
        let (Catchup::Live { head: b0 }, _rx2) = log.subscribe(user(2), 0) else {
            panic!("fresh subscribe is live")
        };
        log.publish(user(1), vec![played(1), played(2)]);
        log.publish(user(2), vec![played(3)]);
        assert_eq!(seqs(&log.catch_up(user(1), a0)), vec![a0 + 1, a0 + 2]);
        assert_eq!(seqs(&log.catch_up(user(2), b0)), vec![b0 + 1]);
    }

    #[test]
    fn replays_only_what_follows_after_seq() {
        let log = EventLog::default();
        let (Catchup::Live { head }, _rx) = log.subscribe(user(1), 0) else {
            panic!()
        };
        log.publish(user(1), vec![played(1), played(2), played(3)]);
        let (resumed, _rx) = log.subscribe(user(1), head + 1);
        assert_eq!(seqs(&resumed), vec![head + 2, head + 3]);
        let (resumed, _rx) = log.subscribe(user(1), head + 3);
        assert_eq!(seqs(&resumed), Vec::<u64>::new());
    }

    #[test]
    fn a_seq_older_than_the_window_resyncs() {
        let log = EventLog::with_window(2, REPLAY_AGE);
        let (Catchup::Live { head }, _rx) = log.subscribe(user(1), 0) else {
            panic!()
        };
        log.publish(user(1), vec![played(1), played(2), played(3)]);
        // head + 1 was pruned, so resuming after head would miss it.
        assert!(
            matches!(log.catch_up(user(1), head), Catchup::Resync { head: h } if h == head + 3)
        );
        assert_eq!(
            seqs(&log.catch_up(user(1), head + 1)),
            vec![head + 2, head + 3]
        );
    }

    #[test]
    fn an_event_older_than_the_age_bound_is_not_replayed() {
        let log = EventLog::with_window(REPLAY_EVENTS, Duration::ZERO);
        let (Catchup::Live { head }, _rx) = log.subscribe(user(1), 0) else {
            panic!()
        };
        log.publish(user(1), vec![played(1)]);
        std::thread::sleep(Duration::from_millis(2));
        assert!(matches!(
            log.catch_up(user(1), head),
            Catchup::Resync { .. }
        ));
    }

    #[test]
    fn a_seq_from_another_run_resyncs() {
        let log = EventLog::default();
        // A stream made now: a seq from before it, or ahead of it, is foreign.
        let (_, _rx) = log.subscribe(user(1), 0);
        let head = log.head(user(1)).unwrap();
        assert!(matches!(log.catch_up(user(1), 5), Catchup::Resync { .. }));
        assert!(matches!(
            log.catch_up(user(1), head + 10),
            Catchup::Resync { .. }
        ));
        // And a user with no stream yet resyncs too.
        assert!(matches!(
            log.subscribe(user(2), 5).0,
            Catchup::Resync { .. }
        ));
    }

    #[test]
    fn a_users_live_channel_carries_only_their_events() {
        let log = EventLog::default();
        let (_, mut mine) = log.subscribe(user(1), 0);
        let (_, _theirs) = log.subscribe(user(2), 0);
        log.publish(user(2), vec![played(1)]);
        assert!(
            mine.try_recv().is_err(),
            "another user's event woke this one"
        );
        log.publish(user(1), vec![played(2)]);
        assert!(mine.try_recv().is_ok());
    }

    #[test]
    fn a_user_without_a_stream_keeps_nothing() {
        let log = EventLog::default();
        log.publish(user(1), vec![played(1)]);
        log.publish_all(Payload::Resync);
        assert_eq!(log.head(user(1)), None);
    }

    #[test]
    fn a_library_change_reaches_every_stream_once_stored() {
        let log = EventLog::default();
        let (_, _a) = log.subscribe(user(1), 0);
        let (_, _b) = log.subscribe(user(2), 0);
        log.translate(SocketBroadcast::LibraryChanged {
            added: vec!["7".into()],
            removed: vec![],
        });
        let streams = log.lock();
        let a = &streams[&user(1)].window[0].payload;
        let b = &streams[&user(2)].window[0].payload;
        assert!(Arc::ptr_eq(a, b), "one payload shared by both streams");
        assert_eq!(
            **a,
            Payload::Library {
                added: vec![7],
                removed: vec![]
            }
        );
    }

    #[test]
    fn a_user_data_announcement_becomes_typed_events() {
        let log = EventLog::default();
        let u = user(1);
        let (Catchup::Live { head }, _rx) = log.subscribe(u, 0) else {
            panic!()
        };
        let dto = pharos_jellyfin_api::dto::UserItemDataDto::from_domain_with_runtime(
            42,
            pharos_core::UserItemData {
                played: true,
                is_favorite: true,
                play_count: 3,
                last_played_position_ticks: 50_000_000,
                ..Default::default()
            },
            0,
        );
        log.translate(SocketBroadcast::UserDataChanged {
            user_id: u.0.simple().to_string(),
            entries: vec![
                serde_json::to_value(&dto).unwrap(),
                serde_json::json!({"ItemId": "not an id"}),
            ],
        });
        let Catchup::Replay(events) = log.catch_up(u, head) else {
            panic!()
        };
        assert_eq!(events.len(), 1, "the unreadable entry is skipped");
        assert_eq!(
            *events[0].payload,
            Payload::UserData {
                id: WireId::Real(42),
                state: UserState {
                    played: true,
                    favorite: true,
                    position_ms: 5_000,
                    play_count: 3
                }
            }
        );
    }

    #[test]
    fn refresh_progress_is_a_task_fraction() {
        let log = EventLog::default();
        let (Catchup::Live { head }, _rx) = log.subscribe(user(1), 0) else {
            panic!()
        };
        log.translate(SocketBroadcast::RefreshProgress {
            item_id: "lib".into(),
            progress: 50.0,
        });
        log.translate(SocketBroadcast::RefreshProgress {
            item_id: "lib".into(),
            progress: 100.0,
        });
        let Catchup::Replay(events) = log.catch_up(user(1), head) else {
            panic!()
        };
        let tasks: Vec<_> = events.iter().map(|e| (*e.payload).clone()).collect();
        assert_eq!(
            tasks,
            vec![
                Payload::Task {
                    id: "lib".into(),
                    progress: 0.5,
                    state: "running",
                    result: None,
                },
                Payload::Task {
                    id: "lib".into(),
                    progress: 1.0,
                    state: "done",
                    result: None,
                },
            ]
        );
    }

    #[test]
    fn a_library_scan_is_a_task_that_ends_with_its_counts() {
        use crate::scan_tasks::{ScanOutcome, ScanTally, TASK_REFRESH_LIBRARY};
        let log = EventLog::default();
        let (Catchup::Live { head }, _rx) = log.subscribe(user(1), 0) else {
            panic!()
        };
        let tally = ScanTally {
            roots: 3,
            failed_roots: 1,
            incomplete_roots: 0,
            added: 4,
            changed: 2,
            removed: 1,
        };
        log.translate(SocketBroadcast::ScanProgress {
            progress: 25.0,
            end: None,
        });
        log.translate(SocketBroadcast::ScanProgress {
            progress: 100.0,
            end: Some((ScanOutcome::Partial, tally)),
        });
        let Catchup::Replay(events) = log.catch_up(user(1), head) else {
            panic!()
        };
        let tasks: Vec<_> = events.iter().map(|e| (*e.payload).clone()).collect();
        assert_eq!(
            tasks,
            vec![
                Payload::Task {
                    id: TASK_REFRESH_LIBRARY.into(),
                    progress: 0.25,
                    state: "running",
                    result: None,
                },
                Payload::Task {
                    id: TASK_REFRESH_LIBRARY.into(),
                    progress: 1.0,
                    state: "partial",
                    result: Some(tally),
                },
            ]
        );
    }

    #[test]
    fn labels_are_distinct() {
        let topics: std::collections::HashSet<_> = Topic::ALL.map(Topic::label).into();
        assert_eq!(topics.len(), Topic::ALL.len());
        assert!(!topics.contains("resync"));
        let outcomes: std::collections::HashSet<_> = Outcome::ALL.map(Outcome::label).into();
        assert_eq!(outcomes.len(), Outcome::ALL.len());
        for t in Topic::ALL {
            assert_eq!(Topic::from_wire(t.wire()), Some(t));
        }
    }
}
