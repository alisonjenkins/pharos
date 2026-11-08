//! Watch together (014): the client half of a SyncPlay group that does not
//! need the server. Pure functions and models here; the screens and the
//! network seam come in later commits.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use pharos_client::api::{ItemKind, ItemRef, NativeApi};

use super::seen::MemberSeen;
use super::track_follow::{GroupTracksView, TrackChoice};

/// How many clock samples the estimator keeps; older ones fall off.
pub const CLOCK_WINDOW: usize = 9;

/// One round trip to `Group.clock` (NTP style, all milliseconds): the client
/// sent at `t1`, the server received at `t2` and replied at `t3`, the client
/// got the reply at `t4`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockSample {
    pub t1: i64,
    pub t2: i64,
    pub t3: i64,
    pub t4: i64,
}

impl ClockSample {
    /// `((t2 - t1) + (t3 - t4)) / 2`: add it to a client clock reading to get
    /// server time.
    fn offset_ms(&self) -> i64 {
        ((self.t2 - self.t1) + (self.t3 - self.t4)).div_euclid(2)
    }

    /// `(t4 - t1) - (t3 - t2)`: the time on the wire, without the server's
    /// own handling time.
    fn rtt_ms(&self) -> i64 {
        (self.t4 - self.t1) - (self.t3 - self.t2)
    }
}

/// The offset between the client and server clocks, from the last
/// [`CLOCK_WINDOW`] samples. The median ignores the odd slow round trip that
/// a mean would follow.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClockEstimator {
    samples: Vec<ClockSample>,
}

impl ClockEstimator {
    pub fn push(&mut self, sample: ClockSample) {
        self.samples.push(sample);
        if self.samples.len() > CLOCK_WINDOW {
            self.samples.remove(0);
        }
    }

    /// Median offset; `None` before the first sample.
    pub fn offset_ms(&self) -> Option<i64> {
        median(self.samples.iter().map(ClockSample::offset_ms).collect())
    }

    /// Median round-trip time; `None` before the first sample.
    pub fn rtt_ms(&self) -> Option<i64> {
        median(self.samples.iter().map(ClockSample::rtt_ms).collect())
    }

    /// Server time for a client clock reading; `None` before the first sample.
    pub fn server_now(&self, client_ms: i64) -> Option<i64> {
        self.offset_ms().map(|o| client_ms + o)
    }
}

/// The middle value; for an even count, the mean of the two middle values.
fn median(mut values: Vec<i64>) -> Option<i64> {
    values.sort_unstable();
    let n = values.len();
    if n == 0 {
        return None;
    }
    let mid = n / 2;
    if n % 2 == 1 {
        values.get(mid).copied()
    } else {
        let (a, b) = (values.get(mid - 1)?, values.get(mid)?);
        Some((a + b).div_euclid(2))
    }
}

/// Inside this drift the player is left alone.
pub const HOLD_BELOW_MS: u64 = 150;
/// A player already nudging goes back to normal speed under this drift.
pub const RELEASE_BELOW_MS: u64 = 50;
/// Past this drift the player seeks (the oracle's `SYNC_TOL_MS`).
pub const SEEK_ABOVE_MS: u64 = 1_500;
/// `playbackRate` while ahead of the group.
pub const RATE_AHEAD: f64 = 0.95;
/// `playbackRate` while behind the group.
pub const RATE_BEHIND: f64 = 1.05;
/// After a server command (play, pause, seek, item) the client leaves the
/// player alone this long: the command owns the position until it settles.
pub const SETTLE_AFTER_COMMAND_MS: u64 = 3_000;
/// After this session seeks the player, the video's `waiting` is held for
/// this long before it counts as the group buffering: a seek makes the video
/// wait, and reporting that wait freezes the group, which seeks again.
pub const SELF_SEEK_GRACE_MS: u64 = 5_000;

/// What to do about the gap between where the player is and where the group
/// says it should be.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Correction {
    /// Leave it; a player that was nudging goes back to rate 1.0.
    Hold,
    /// Set `playbackRate` to this.
    Rate(f64),
    /// Seek to this position (ms).
    Seek(u64),
}

/// The drift policy (plan clarification 8). Drift is `actual - expected`:
/// positive means ahead. `nudging` is whether a rate nudge is in force, which
/// gives hysteresis: a nudge starts at 150 ms and ends under 50 ms.
/// `since_last_command_ms` is the time since the last server command; inside
/// [`SETTLE_AFTER_COMMAND_MS`] the server's command wins and nothing is done.
pub fn correct(
    expected_ms: i64,
    actual_ms: i64,
    since_last_command_ms: u64,
    nudging: bool,
) -> Correction {
    if since_last_command_ms < SETTLE_AFTER_COMMAND_MS {
        return Correction::Hold;
    }
    let drift = actual_ms - expected_ms;
    let size = drift.unsigned_abs();
    if size > SEEK_ABOVE_MS {
        return Correction::Seek(u64::try_from(expected_ms).unwrap_or(0));
    }
    let floor = if nudging {
        RELEASE_BELOW_MS
    } else {
        HOLD_BELOW_MS
    };
    if size < floor {
        Correction::Hold
    } else if drift > 0 {
        Correction::Rate(RATE_AHEAD)
    } else {
        Correction::Rate(RATE_BEHIND)
    }
}

/// Runs [`correct`] over time: remembers whether a nudge is in force and
/// counts hard seeks, for the `groupHardSeek` event.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DriftTracker {
    nudging: bool,
    hard_seeks: u32,
}

impl DriftTracker {
    pub fn check(
        &mut self,
        expected_ms: i64,
        actual_ms: i64,
        since_last_command_ms: u64,
    ) -> Correction {
        let c = correct(expected_ms, actual_ms, since_last_command_ms, self.nudging);
        self.nudging = matches!(c, Correction::Rate(_));
        if matches!(c, Correction::Seek(_)) {
            self.hard_seeks += 1;
        }
        c
    }

    /// Hard seeks so far.
    pub fn hard_seeks(&self) -> u32 {
        self.hard_seeks
    }
}

/// Where the group's playback is now, from an anchor the server stated
/// (`position` at `anchor_server_ms`). Paused, it stays at the anchor; a
/// `now` before the anchor does not run backwards.
pub fn expected_position(
    anchor_position_ms: u64,
    anchor_server_ms: i64,
    now_server_ms: i64,
    playing: bool,
) -> u64 {
    if !playing {
        return anchor_position_ms;
    }
    anchor_position_ms + u64::try_from(now_server_ms - anchor_server_ms).unwrap_or(0)
}

/// Whether a group can watch this kind together: films and episodes. Music
/// plays alone.
pub fn can_watch_together(kind: ItemKind) -> bool {
    matches!(kind, ItemKind::Movie | ItemKind::Episode)
}

/// A group's id, as the server minted it.
pub type GroupId = String;

/// Someone in a group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub id: String,
    pub name: String,
    pub buffering: bool,
    /// On their own audio and subtitles, not the group's.
    pub own_tracks: bool,
}

/// A group as the Together page lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupSummary {
    pub id: GroupId,
    pub name: String,
    /// What the group is watching; `None` before it has an item.
    pub item: Option<ItemRef>,
    pub members: Vec<Member>,
    /// What a late joiner starts on.
    pub tracks: GroupTracksView,
    /// The receiving member's own id, in the `joined` update; empty in a listing.
    pub you: String,
}

/// A refused or failed group call, in words for the viewer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupError(pub String);

/// The server's reply to `clock`: the client's timestamp echoed, then when
/// the server received the request and when it replied (`t1`, `t2`, `t3`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockReply {
    pub client_ms_echo: i64,
    pub recv_ms: i64,
    pub send_ms: i64,
}

impl ClockReply {
    /// The estimator's sample, once the reply arrives at `t4`.
    pub fn sample(&self, t4: i64) -> ClockSample {
        ClockSample {
            t1: self.client_ms_echo,
            t2: self.recv_ms,
            t3: self.send_ms,
            t4,
        }
    }
}

/// What the server pushes to a member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupUpdate {
    Joined(GroupSummary),
    Play {
        at_server_ms: i64,
        position_ms: u64,
        /// The member who caused it; `None` when the group did.
        by: Option<String>,
    },
    Pause {
        at_server_ms: i64,
        position_ms: u64,
        by: Option<String>,
    },
    Seek {
        at_server_ms: i64,
        position_ms: u64,
        by: Option<String>,
    },
    /// A member joined, or its buffering changed.
    Member(Member),
    /// A member left (its id).
    Left(String),
    /// The group moved to another item.
    Item {
        item: ItemRef,
        playlist_item_id: String,
        position_ms: u64,
        playing: bool,
    },
    /// The group's audio or subtitle changed, set by `by`.
    Tracks {
        tracks: GroupTracksView,
        by: Option<String>,
    },
    /// The group is over for this member, for whatever reason.
    Ended,
}

/// Where a membership's pushed updates go.
pub type GroupSink = Rc<dyn Fn(GroupUpdate)>;

/// The seam to the server's native `Group` and `Membership` interfaces. A
/// thin adapter over `pharos-client` implements it; [`FakeGroups`] stands in
/// until then. Async without `Send`: the browser runs on one thread.
#[allow(async_fn_in_trait)]
pub trait GroupClient {
    async fn list(&self) -> Result<Vec<GroupSummary>, GroupError>;
    /// Start a group on `item` at `position_ms`; updates go to `sink`.
    async fn create(
        &self,
        item: ItemRef,
        position_ms: u64,
        sink: GroupSink,
    ) -> Result<GroupSummary, GroupError>;
    async fn join(&self, id: &str, sink: GroupSink) -> Result<GroupSummary, GroupError>;
    async fn clock(&self, client_ms: i64) -> Result<ClockReply, GroupError>;
    /// Keep-alive, every 30 s.
    async fn ping(&self) -> Result<(), GroupError>;
    /// Report a finished round trip, so the server knows this member's offset.
    async fn clock_report(&self, t1: i64, t2: i64, t3: i64, t4: i64) -> Result<(), GroupError>;
    async fn unpause(&self) -> Result<(), GroupError>;
    async fn pause(&self) -> Result<(), GroupError>;
    async fn seek(&self, position_ms: u64) -> Result<(), GroupError>;
    async fn buffering(
        &self,
        playlist_item_id: String,
        position_ms: u64,
        buffering: bool,
    ) -> Result<(), GroupError>;
    async fn ready(
        &self,
        playlist_item_id: String,
        position_ms: u64,
        playing: bool,
    ) -> Result<(), GroupError>;
    async fn leave(&self) -> Result<(), GroupError>;
    /// Change the group's audio and subtitle; an unset half stays as it is.
    /// `playlist_item_id` is the queue entry the pick was made on: the group
    /// drops it if it has moved on.
    async fn set_tracks(
        &self,
        audio: TrackChoice,
        subtitle: TrackChoice,
        playlist_item_id: String,
    ) -> Result<(), GroupError>;
    /// `false` puts this member on their own tracks ("Just me").
    async fn follow_tracks(&self, follow: bool) -> Result<(), GroupError>;
    /// Move the group to `item` from `position_ms` (014 US9).
    /// `playlist_item_id` is the entry this member moves from: the group
    /// drops the move if it has already left it.
    async fn set_item(
        &self,
        item: ItemRef,
        position_ms: u64,
        playlist_item_id: String,
    ) -> Result<(), GroupError>;
    /// What each member has seen of `ids` (at most 200).
    async fn seen(&self, ids: Vec<ItemRef>) -> Result<Vec<MemberSeen>, GroupError>;
}

/// The seam's one place of construction: the screens ask for the client
/// here. In the browser and the Android TV app it is the network adapter over
/// the session's `Group`; on the host, the in-memory [`FakeGroups`].
pub fn group_api(api: &NativeApi) -> Groups {
    super::platform::groups(api)
}

/// The [`GroupClient`] this build talks to.
pub use super::platform::Groups;

/// A held `buffering` call: whether it may go, and who to wake when it may.
type BufferingGate = Rc<(Cell<bool>, RefCell<Option<std::task::Waker>>)>;

/// An in-memory [`GroupClient`] for tests and the pre-network screens. It
/// keeps its groups and a log of the calls it received.
#[derive(Clone, Default)]
pub struct FakeGroups {
    groups: Rc<RefCell<Vec<GroupSummary>>>,
    calls: Rc<RefCell<Vec<String>>>,
    sinks: Rc<RefCell<Vec<GroupSink>>>,
    joined: Rc<RefCell<Option<GroupId>>>,
    seen: Rc<RefCell<Vec<MemberSeen>>>,
    refuse_follow: Rc<Cell<bool>>,
    refuse_tracks: Rc<Cell<bool>>,
    refuse_set_item: Rc<Cell<bool>>,
    hold_buffering: Rc<Cell<bool>>,
    buffering_gates: Rc<RefCell<Vec<BufferingGate>>>,
    join_pushes: Rc<RefCell<Vec<GroupUpdate>>>,
}

impl FakeGroups {
    /// The next `join` sends `updates` to its sink and yields once before it
    /// answers, as the server's pushes can reach a member before the answer
    /// to its join does.
    pub fn push_on_join(&self, updates: Vec<GroupUpdate>) {
        *self.join_pushes.borrow_mut() = updates;
    }

    /// Makes every `buffering` wait for [`Self::release_buffering`] before it
    /// is logged, as a slow server would; the gates open in any order.
    pub fn hold_buffering(&self) {
        self.hold_buffering.set(true);
    }

    /// Opens the `n`th held `buffering` call (counted from 0 in the order the
    /// calls started); a call that has not started yet is ignored.
    pub fn release_buffering(&self, n: usize) {
        if let Some(gate) = self.buffering_gates.borrow().get(n) {
            gate.0.set(true);
            if let Some(waker) = gate.1.borrow_mut().take() {
                waker.wake();
            }
        }
    }

    /// Makes every `set_tracks` fail, as a dropped connection would.
    pub fn refuse_tracks(&self) {
        self.refuse_tracks.set(true);
    }

    /// Makes every `set_item` fail, as a server before schemaVersion 23 would.
    pub fn refuse_set_item(&self) {
        self.refuse_set_item.set(true);
    }

    /// Makes every `follow_tracks` fail, as a dropped connection would.
    pub fn refuse_follow(&self) {
        self.refuse_follow.set(true);
    }

    /// What `seen` answers.
    pub fn with_seen(seen: Vec<MemberSeen>) -> Self {
        Self {
            seen: Rc::new(RefCell::new(seen)),
            ..Self::default()
        }
    }

    pub fn with_groups(groups: Vec<GroupSummary>) -> Self {
        Self {
            groups: Rc::new(RefCell::new(groups)),
            ..Self::default()
        }
    }

    /// The calls received, as `name` or `name(args)`.
    pub fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }

    /// The sinks the fake holds, oldest first.
    pub fn sinks(&self) -> Vec<GroupSink> {
        self.sinks.borrow().clone()
    }

    /// Push an update to every member the fake holds a sink for.
    pub fn push(&self, update: &GroupUpdate) {
        for sink in self.sinks.borrow().iter() {
            sink(update.clone());
        }
    }

    fn log(&self, call: String) {
        self.calls.borrow_mut().push(call);
    }

    fn me() -> Member {
        Member {
            id: "me".into(),
            name: "You".into(),
            buffering: false,
            own_tracks: false,
        }
    }

    /// The group this member is in, mutably.
    fn with_joined<R>(&self, f: impl FnOnce(&mut GroupSummary) -> R) -> Option<R> {
        let id = self.joined.borrow().clone()?;
        let mut groups = self.groups.borrow_mut();
        groups.iter_mut().find(|g| g.id == id).map(f)
    }
}

impl GroupClient for FakeGroups {
    async fn list(&self) -> Result<Vec<GroupSummary>, GroupError> {
        self.log("list".into());
        Ok(self.groups.borrow().clone())
    }

    async fn create(
        &self,
        item: ItemRef,
        position_ms: u64,
        sink: GroupSink,
    ) -> Result<GroupSummary, GroupError> {
        self.log(format!("create({item},{position_ms})"));
        let mut groups = self.groups.borrow_mut();
        let group = GroupSummary {
            id: format!("g{}", groups.len() + 1),
            name: "Watch together".into(),
            item: Some(item),
            members: vec![Self::me()],
            tracks: GroupTracksView::default(),
            you: String::new(),
        };
        groups.push(group.clone());
        self.sinks.borrow_mut().push(sink);
        *self.joined.borrow_mut() = Some(group.id.clone());
        Ok(group)
    }

    async fn join(&self, id: &str, sink: GroupSink) -> Result<GroupSummary, GroupError> {
        self.log(format!("join({id})"));
        let answer = {
            let mut groups = self.groups.borrow_mut();
            let Some(group) = groups.iter_mut().find(|g| g.id == id) else {
                return Err(GroupError("This group has ended".into()));
            };
            group.members.push(Self::me());
            group.clone()
        };
        self.sinks.borrow_mut().push(Rc::clone(&sink));
        *self.joined.borrow_mut() = Some(answer.id.clone());
        let pushes = std::mem::take(&mut *self.join_pushes.borrow_mut());
        if !pushes.is_empty() {
            for update in pushes {
                sink(update);
            }
            let mut yielded = false;
            std::future::poll_fn(|cx| {
                if std::mem::replace(&mut yielded, true) {
                    std::task::Poll::Ready(())
                } else {
                    cx.waker().wake_by_ref();
                    std::task::Poll::Pending
                }
            })
            .await;
        }
        Ok(answer)
    }

    async fn clock(&self, client_ms: i64) -> Result<ClockReply, GroupError> {
        self.log(format!("clock({client_ms})"));
        Ok(ClockReply {
            client_ms_echo: client_ms,
            recv_ms: client_ms,
            send_ms: client_ms,
        })
    }

    async fn ping(&self) -> Result<(), GroupError> {
        self.log("ping".into());
        Ok(())
    }

    async fn clock_report(&self, t1: i64, t2: i64, t3: i64, t4: i64) -> Result<(), GroupError> {
        self.log(format!("clock_report({t1},{t2},{t3},{t4})"));
        Ok(())
    }

    async fn unpause(&self) -> Result<(), GroupError> {
        self.log("unpause".into());
        Ok(())
    }

    async fn pause(&self) -> Result<(), GroupError> {
        self.log("pause".into());
        Ok(())
    }

    async fn seek(&self, position_ms: u64) -> Result<(), GroupError> {
        self.log(format!("seek({position_ms})"));
        Ok(())
    }

    async fn buffering(
        &self,
        playlist_item_id: String,
        position_ms: u64,
        buffering: bool,
    ) -> Result<(), GroupError> {
        if self.hold_buffering.get() {
            let gate: BufferingGate = Rc::new((Cell::new(false), RefCell::new(None)));
            self.buffering_gates.borrow_mut().push(gate.clone());
            std::future::poll_fn(|cx| {
                if gate.0.get() {
                    std::task::Poll::Ready(())
                } else {
                    *gate.1.borrow_mut() = Some(cx.waker().clone());
                    std::task::Poll::Pending
                }
            })
            .await;
        }
        self.log(format!(
            "buffering({playlist_item_id},{position_ms},{buffering})"
        ));
        Ok(())
    }

    async fn ready(
        &self,
        playlist_item_id: String,
        position_ms: u64,
        playing: bool,
    ) -> Result<(), GroupError> {
        self.log(format!("ready({playlist_item_id},{position_ms},{playing})"));
        Ok(())
    }

    async fn seen(&self, ids: Vec<ItemRef>) -> Result<Vec<MemberSeen>, GroupError> {
        self.log(format!("seen({})", ids.len()));
        Ok(self.seen.borrow().clone())
    }

    async fn set_tracks(
        &self,
        audio: TrackChoice,
        subtitle: TrackChoice,
        playlist_item_id: String,
    ) -> Result<(), GroupError> {
        self.log(format!(
            "set_tracks({audio:?},{subtitle:?},{playlist_item_id})"
        ));
        if self.refuse_tracks.get() {
            return Err(GroupError("Refused".into()));
        }
        let Some(tracks) = self.with_joined(|g| {
            if audio != TrackChoice::Unset {
                g.tracks.audio = audio;
            }
            if subtitle != TrackChoice::Unset {
                g.tracks.subtitle = subtitle;
            }
            g.tracks
        }) else {
            return Err(GroupError("Not in a group".into()));
        };
        self.push(&GroupUpdate::Tracks {
            tracks,
            by: Some(Self::me().id),
        });
        Ok(())
    }

    async fn set_item(
        &self,
        item: ItemRef,
        position_ms: u64,
        playlist_item_id: String,
    ) -> Result<(), GroupError> {
        self.log(format!("set_item({item},{position_ms},{playlist_item_id})"));
        if self.refuse_set_item.get() {
            return Err(GroupError("Refused".into()));
        }
        let Some(()) = self.with_joined(|g| g.item = Some(item)) else {
            return Err(GroupError("Not in a group".into()));
        };
        self.push(&GroupUpdate::Item {
            item,
            playlist_item_id: format!("pli-{item}"),
            position_ms,
            playing: true,
        });
        Ok(())
    }

    async fn follow_tracks(&self, follow: bool) -> Result<(), GroupError> {
        self.log(format!("follow_tracks({follow})"));
        if self.refuse_follow.get() {
            return Err(GroupError("Refused".into()));
        }
        let me = Member {
            own_tracks: !follow,
            ..Self::me()
        };
        let Some(()) = self.with_joined(|g| {
            if let Some(m) = g.members.iter_mut().find(|m| m.id == me.id) {
                m.own_tracks = me.own_tracks;
            }
        }) else {
            return Err(GroupError("Not in a group".into()));
        };
        self.push(&GroupUpdate::Member(me));
        Ok(())
    }

    async fn leave(&self) -> Result<(), GroupError> {
        self.log("leave".into());
        let Some(id) = self.joined.borrow_mut().take() else {
            return Ok(());
        };
        let mut groups = self.groups.borrow_mut();
        if let Some(g) = groups.iter_mut().find(|g| g.id == id) {
            g.members.retain(|m| m.id != "me");
        }
        groups.retain(|g| !g.members.is_empty());
        self.sinks.borrow_mut().clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sample whose server clock is `offset` ahead and whose trip took
    /// `rtt` split evenly, sent at `at`; the server answers at once.
    fn sample(at: i64, offset: i64, rtt: i64) -> ClockSample {
        let server = at + rtt / 2 + offset;
        ClockSample {
            t1: at,
            t2: server,
            t3: server,
            t4: at + rtt,
        }
    }

    fn estimator(samples: &[ClockSample]) -> ClockEstimator {
        let mut e = ClockEstimator::default();
        for s in samples {
            e.push(*s);
        }
        e
    }

    #[test]
    fn no_samples_gives_nothing() {
        let e = ClockEstimator::default();
        assert_eq!(e.offset_ms(), None);
        assert_eq!(e.rtt_ms(), None);
        assert_eq!(e.server_now(1_000), None);
    }

    #[test]
    fn one_sample_gives_its_offset_and_rtt() {
        let e = estimator(&[sample(1_000, 250, 40)]);
        assert_eq!(e.offset_ms(), Some(250));
        assert_eq!(e.rtt_ms(), Some(40));
        assert_eq!(e.server_now(2_000), Some(2_250));
    }

    #[test]
    fn the_servers_handling_time_is_not_round_trip() {
        // 20 ms out, 30 ms handling, 20 ms back; server 100 ms ahead.
        let s = ClockSample {
            t1: 1_000,
            t2: 1_120,
            t3: 1_150,
            t4: 1_070,
        };
        let e = estimator(&[s]);
        assert_eq!(e.rtt_ms(), Some(40));
        assert_eq!(e.offset_ms(), Some(100));
    }

    #[test]
    fn an_odd_count_takes_the_middle_and_an_even_count_the_mean_of_two() {
        let odd = estimator(&[
            sample(0, 100, 20),
            sample(1_000, 300, 40),
            sample(2_000, 200, 30),
        ]);
        assert_eq!(odd.offset_ms(), Some(200));
        assert_eq!(odd.rtt_ms(), Some(30));
        let even = estimator(&[sample(0, 100, 20), sample(1_000, 200, 40)]);
        assert_eq!(even.offset_ms(), Some(150));
        assert_eq!(even.rtt_ms(), Some(30));
    }

    #[test]
    fn a_slow_outlier_does_not_move_the_median() {
        let mut samples = vec![sample(0, 100, 20); 4];
        samples.push(sample(5_000, 900, 2_000));
        let e = estimator(&samples);
        assert_eq!(e.offset_ms(), Some(100));
        assert_eq!(e.rtt_ms(), Some(20));
    }

    #[test]
    fn only_the_last_nine_samples_count() {
        let mut e = ClockEstimator::default();
        for i in 0..CLOCK_WINDOW as i64 {
            e.push(sample(i * 1_000, 10_000, 20));
        }
        assert_eq!(e.offset_ms(), Some(10_000));
        for i in 0..CLOCK_WINDOW as i64 {
            e.push(sample(100_000 + i * 1_000, 50, 20));
        }
        assert_eq!(e.offset_ms(), Some(50));
        assert_eq!(e.samples.len(), CLOCK_WINDOW);
    }

    #[test]
    fn a_negative_offset_is_kept() {
        let e = estimator(&[sample(10_000, -400, 60)]);
        assert_eq!(e.offset_ms(), Some(-400));
    }

    const LONG_AGO: u64 = SETTLE_AFTER_COMMAND_MS;

    #[test]
    fn within_150_ms_either_way_holds() {
        for d in [0, 149, -149] {
            assert_eq!(
                correct(10_000, 10_000 + d, LONG_AGO, false),
                Correction::Hold
            );
        }
    }

    #[test]
    fn from_150_to_1500_nudges_against_the_drift() {
        for d in [150, 1_500] {
            assert_eq!(
                correct(10_000, 10_000 + d, LONG_AGO, false),
                Correction::Rate(0.95)
            );
            assert_eq!(
                correct(10_000, 10_000 - d, LONG_AGO, false),
                Correction::Rate(1.05)
            );
        }
    }

    #[test]
    fn past_1500_seeks_to_the_expected_position() {
        assert_eq!(
            correct(10_000, 11_501, LONG_AGO, false),
            Correction::Seek(10_000)
        );
        assert_eq!(
            correct(10_000, 8_499, LONG_AGO, true),
            Correction::Seek(10_000)
        );
        assert_eq!(correct(-5, 9_000, LONG_AGO, false), Correction::Seek(0));
    }

    #[test]
    fn a_nudge_holds_on_until_under_50_ms() {
        assert_eq!(correct(10_000, 10_100, LONG_AGO, false), Correction::Hold);
        assert_eq!(
            correct(10_000, 10_100, LONG_AGO, true),
            Correction::Rate(0.95)
        );
        assert_eq!(
            correct(10_000, 10_050, LONG_AGO, true),
            Correction::Rate(0.95)
        );
        assert_eq!(correct(10_000, 10_049, LONG_AGO, true), Correction::Hold);
        assert_eq!(correct(10_000, 9_951, LONG_AGO, true), Correction::Hold);
        assert_eq!(
            correct(10_000, 9_950, LONG_AGO, true),
            Correction::Rate(1.05)
        );
    }

    #[test]
    fn a_fresh_server_command_suppresses_every_correction() {
        for since in [0, SETTLE_AFTER_COMMAND_MS - 1] {
            assert_eq!(correct(10_000, 50_000, since, false), Correction::Hold);
            assert_eq!(correct(10_000, 10_500, since, true), Correction::Hold);
        }
        assert_eq!(
            correct(10_000, 50_000, SETTLE_AFTER_COMMAND_MS, false),
            Correction::Seek(10_000)
        );
    }

    #[test]
    fn the_tracker_remembers_the_nudge_and_counts_hard_seeks() {
        let mut t = DriftTracker::default();
        assert_eq!(t.check(10_000, 10_100, LONG_AGO), Correction::Hold);
        assert_eq!(t.check(10_000, 10_200, LONG_AGO), Correction::Rate(0.95));
        assert_eq!(t.check(10_000, 10_100, LONG_AGO), Correction::Rate(0.95));
        assert_eq!(t.check(10_000, 10_010, LONG_AGO), Correction::Hold);
        assert_eq!(t.check(10_000, 10_100, LONG_AGO), Correction::Hold);
        assert_eq!(t.hard_seeks(), 0);
        assert_eq!(t.check(10_000, 20_000, LONG_AGO), Correction::Seek(10_000));
        assert_eq!(t.hard_seeks(), 1);
    }

    #[test]
    fn expected_position_runs_with_the_server_clock_only_while_playing() {
        assert_eq!(expected_position(5_000, 100, 1_100, true), 6_000);
        assert_eq!(expected_position(5_000, 100, 1_100, false), 5_000);
        assert_eq!(expected_position(5_000, 100, 50, true), 5_000);
    }

    fn now<T>(f: impl std::future::Future<Output = T>) -> Option<T> {
        use futures_util::FutureExt as _;
        f.now_or_never()
    }

    #[test]
    fn a_clock_reply_becomes_a_sample() {
        let r = ClockReply {
            client_ms_echo: 1_000,
            recv_ms: 1_120,
            send_ms: 1_150,
        };
        let e = estimator(&[r.sample(1_070)]);
        assert_eq!(e.offset_ms(), Some(100));
        assert_eq!(e.rtt_ms(), Some(40));
    }

    #[test]
    fn the_fake_creates_and_ends_a_group() {
        let api = FakeGroups::default();
        let sink: GroupSink = Rc::new(|_| {});
        assert_eq!(now(api.list()), Some(Ok(vec![])));
        let made = now(api.create(ItemRef::Real(7), 5_000, sink.clone()));
        assert_eq!(
            made.and_then(Result::ok).map(|g| g.item),
            Some(Some(ItemRef::Real(7)))
        );
        assert_eq!(
            now(api.list()).and_then(Result::ok).map(|g| g.len()),
            Some(1)
        );
        assert_eq!(
            now(api.join("nope", sink)),
            Some(Err(GroupError("This group has ended".into())))
        );
        assert_eq!(now(api.leave()), Some(Ok(())));
        assert_eq!(now(api.list()), Some(Ok(vec![])));
        assert_eq!(
            api.calls(),
            [
                "list",
                "create(7,5000)",
                "list",
                "join(nope)",
                "leave",
                "list"
            ]
        );
    }

    #[test]
    fn the_fake_pushes_updates_to_its_sinks() {
        let api = FakeGroups::with_groups(vec![GroupSummary {
            id: "g1".into(),
            name: "Friday".into(),
            item: None,
            members: vec![],
            tracks: GroupTracksView::default(),
            you: String::new(),
        }]);
        let seen = Rc::new(RefCell::new(vec![]));
        let sink: GroupSink = {
            let seen = Rc::clone(&seen);
            Rc::new(move |u| seen.borrow_mut().push(u))
        };
        assert!(matches!(now(api.join("g1", sink)), Some(Ok(_))));
        api.push(&GroupUpdate::Ended);
        assert_eq!(*seen.borrow(), [GroupUpdate::Ended]);
    }

    fn joined_fake() -> (FakeGroups, Rc<RefCell<Vec<GroupUpdate>>>) {
        let api = FakeGroups::with_groups(vec![GroupSummary {
            id: "g1".into(),
            name: "Friday".into(),
            item: None,
            members: vec![],
            tracks: GroupTracksView::default(),
            you: String::new(),
        }]);
        let seen = Rc::new(RefCell::new(vec![]));
        let sink: GroupSink = {
            let seen = Rc::clone(&seen);
            Rc::new(move |u| seen.borrow_mut().push(u))
        };
        assert!(matches!(now(api.join("g1", sink)), Some(Ok(_))));
        (api, seen)
    }

    #[test]
    fn set_tracks_broadcasts_and_keeps_the_unset_half() {
        let (api, seen) = joined_fake();
        let _ = now(api.set_tracks(TrackChoice::Index(2), TrackChoice::Unset, "p1".into()));
        let _ = now(api.set_tracks(TrackChoice::Unset, TrackChoice::Off, "p1".into()));
        let both = GroupTracksView {
            audio: TrackChoice::Index(2),
            subtitle: TrackChoice::Off,
        };
        let last = seen.borrow().last().cloned();
        assert_eq!(
            last,
            Some(GroupUpdate::Tracks {
                tracks: both,
                by: Some("me".into())
            })
        );
        assert_eq!(seen.borrow().len(), 2);
        let listed = now(api.list()).and_then(Result::ok).unwrap_or_default();
        assert_eq!(listed.first().map(|g| g.tracks), Some(both));
    }

    #[test]
    fn follow_tracks_marks_the_member_and_broadcasts_it() {
        let (api, seen) = joined_fake();
        let _ = now(api.follow_tracks(false));
        let own = Member {
            own_tracks: true,
            ..FakeGroups::me()
        };
        assert_eq!(*seen.borrow(), [GroupUpdate::Member(own)]);
        let _ = now(api.follow_tracks(true));
        let listed = now(api.list()).and_then(Result::ok).unwrap_or_default();
        let marks: Vec<bool> = listed
            .first()
            .map(|g| g.members.iter().map(|m| m.own_tracks).collect())
            .unwrap_or_default();
        assert_eq!(marks, [false]);
    }
}
