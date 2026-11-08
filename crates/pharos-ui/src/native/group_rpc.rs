//! The network [`GroupClient`] (spec 014's Together) over the session's
//! `Group` and `Membership` capabilities. The browser and the Android TV app
//! both reach the server through the same `NativeApi`, so both use this one
//! adapter; each platform only supplies its device id. The host build keeps
//! the in-memory [`FakeGroups`](super::together::FakeGroups).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use pharos_client::api::{ItemRef, NativeApi};
use pharos_client::group::{GroupUpdate, NativeGroup, NativeMembership};
use pharos_client::{CallInfo, ClientError};

use super::platform::{sleep_ms, warn};
use super::seen::MemberSeen;
use super::together::{self, ClockReply, GroupClient, GroupError, GroupSink, GroupSummary, Member};
use super::track_follow::{GroupTracksView, TrackChoice};

/// `Group` and `Membership` over the session: the [`GroupClient`] the Together
/// screens and the group store use. One membership at a time; joining or
/// creating replaces it.
#[derive(Clone)]
pub struct Groups {
    /// Shared by every clone, so a reconnect reaches the session's copy.
    group: Rc<RefCell<NativeGroup>>,
    member: Rc<RefCell<Option<NativeMembership>>>,
    /// Counts the creates and joins, so an update from an earlier one is not
    /// taken for the current membership's.
    generation: Rc<Cell<u64>>,
    call: CallInfo,
}

impl Groups {
    /// The groups of `api`'s session, as the member `device`: the server keys
    /// a member by user and device, so each tab or TV needs its own id.
    pub fn new(api: &NativeApi, device: String) -> Self {
        Self {
            group: Rc::new(RefCell::new(api.group().clone())),
            member: Rc::default(),
            generation: Rc::default(),
            call: CallInfo {
                ui_route: "together".into(),
                ui_action: "group".into(),
                client_version: env!("CARGO_PKG_VERSION").into(),
                device,
                ..CallInfo::default()
            },
        }
    }
}

/// What a create or join learns from its sink before the call answers.
struct Answer {
    /// The first `joined`.
    first: Rc<RefCell<Option<GroupSummary>>>,
    /// An `ended` came first.
    ended: Rc<Cell<bool>>,
}

/// How long a create or join waits for the group's `joined` update.
const JOINED_WAIT_MS: u32 = 5_000;
const JOINED_POLL_MS: u32 = 20;

fn group_error(e: ClientError) -> GroupError {
    match e {
        ClientError::Api {
            kind: "notFound", ..
        } => GroupError::gone(),
        ClientError::Api {
            kind: "notPermitted",
            ..
        } => GroupError("You can't watch together here".into()),
        other => GroupError(other.to_string()),
    }
}

fn ms(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn unms(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

fn member(m: pharos_client::group::GroupMember) -> Member {
    Member {
        id: m.id,
        name: m.name,
        buffering: m.buffering,
        own_tracks: m.own_tracks,
    }
}

fn tracks(t: &pharos_client::group::GroupTracks) -> GroupTracksView {
    GroupTracksView {
        audio: t.audio,
        subtitle: t.subtitle,
    }
}

/// The member id a command named; the wire's empty text means none.
fn by(id: Option<String>) -> Option<String> {
    id.filter(|id| !id.is_empty())
}

fn summary(g: pharos_client::group::GroupSummary) -> GroupSummary {
    GroupSummary {
        id: g.id,
        name: g.name,
        item: g.item,
        tracks: tracks(&g.tracks),
        members: g.members.into_iter().map(member).collect(),
        you: g.you,
    }
}

/// The store's update for the client's.
fn update(u: GroupUpdate) -> together::GroupUpdate {
    use together::GroupUpdate as U;
    match u {
        GroupUpdate::Joined(g) => U::Joined(summary(g)),
        GroupUpdate::Play {
            at_server_ms,
            position_ms,
            by: who,
        } => U::Play {
            at_server_ms: ms(at_server_ms),
            position_ms,
            by: by(who),
        },
        GroupUpdate::Pause {
            at_server_ms,
            position_ms,
            by: who,
        } => U::Pause {
            at_server_ms: ms(at_server_ms),
            position_ms,
            by: by(who),
        },
        GroupUpdate::Seek {
            at_server_ms,
            position_ms,
            by: who,
        } => U::Seek {
            at_server_ms: ms(at_server_ms),
            position_ms,
            by: by(who),
        },
        GroupUpdate::Member(m) => U::Member(member(m)),
        GroupUpdate::Left(id) => U::Left(id),
        GroupUpdate::Item {
            item,
            playlist_item_id,
            position_ms,
            playing,
        } => U::Item {
            item,
            playlist_item_id,
            position_ms,
            playing,
        },
        GroupUpdate::Tracks(t) => U::Tracks {
            tracks: tracks(&t),
            by: by(Some(t.by)),
        },
        GroupUpdate::Ended => U::Ended,
    }
}

impl Groups {
    /// The session's connection was replaced by `api`'s: later calls go over
    /// it. The membership held on the old one is dropped without a `leave`,
    /// which could not reach the server; the next join takes its seat back.
    pub fn reconnect(&self, api: &NativeApi) {
        *self.group.borrow_mut() = api.group().clone();
        self.member.borrow_mut().take();
    }

    fn group(&self) -> NativeGroup {
        self.group.borrow().clone()
    }

    fn membership(&self) -> Result<NativeMembership, GroupError> {
        self.member
            .borrow()
            .clone()
            .ok_or_else(|| GroupError("Not in a group".into()))
    }

    /// Leave the membership still held, if any: the server keys a member by
    /// user and device, so a new one made first would be taken for it.
    async fn leave_held(&self) {
        let held = self.member.borrow_mut().take();
        if let Some(m) = held {
            if let Err(e) = m.leave(&self.call).await {
                warn(&format!("pharos: leaving the earlier group failed: {e}"));
            }
        }
    }

    /// Wraps `sink` so the first `joined` is also kept for the caller, which
    /// needs the group's id: `create` and `join` answer with the membership
    /// only. An `ended` drops the membership held for this call, and is
    /// noted in the returned flag for a call it beats to the answer.
    fn tap(&self, sink: GroupSink) -> (impl Fn(GroupUpdate) + 'static, Answer) {
        let first = Rc::new(RefCell::new(None::<GroupSummary>));
        let keep = Rc::clone(&first);
        let ended = Rc::new(Cell::new(false));
        let ended_flag = Rc::clone(&ended);
        let member = Rc::clone(&self.member);
        let generation = Rc::clone(&self.generation);
        generation.set(generation.get() + 1);
        let mine = generation.get();
        (
            move |u: GroupUpdate| {
                let u = update(u);
                if let together::GroupUpdate::Joined(g) = &u {
                    let mut slot = keep.borrow_mut();
                    if slot.is_none() {
                        *slot = Some(g.clone());
                    }
                }
                if u == together::GroupUpdate::Ended {
                    ended_flag.set(true);
                    if generation.get() == mine {
                        member.borrow_mut().take();
                    }
                }
                sink(u);
            },
            Answer { first, ended },
        )
    }

    async fn settled(
        &self,
        made: Result<NativeMembership, ClientError>,
        answer: Answer,
    ) -> Result<GroupSummary, GroupError> {
        let membership = made.map_err(group_error)?;
        if answer.ended.get() {
            return Err(GroupError::gone());
        }
        *self.member.borrow_mut() = Some(membership);
        let mut waited = 0;
        loop {
            if let Some(g) = answer.first.borrow().clone() {
                return Ok(g);
            }
            if waited >= JOINED_WAIT_MS {
                self.leave_held().await;
                return Err(GroupError("The group did not answer".into()));
            }
            sleep_ms(JOINED_POLL_MS).await;
            waited += JOINED_POLL_MS;
        }
    }
}

impl GroupClient for Groups {
    async fn list(&self) -> Result<Vec<GroupSummary>, GroupError> {
        let groups = self.group().list(&self.call).await.map_err(group_error)?;
        Ok(groups.into_iter().map(summary).collect())
    }

    async fn create(
        &self,
        item: ItemRef,
        position_ms: u64,
        sink: GroupSink,
    ) -> Result<GroupSummary, GroupError> {
        self.leave_held().await;
        let (on_update, answer) = self.tap(sink);
        let made = self
            .group()
            .create(&self.call, item, position_ms, on_update)
            .await;
        self.settled(made, answer).await
    }

    async fn join(&self, id: &str, sink: GroupSink) -> Result<GroupSummary, GroupError> {
        self.leave_held().await;
        let (on_update, answer) = self.tap(sink);
        let made = self.group().join(&self.call, id, on_update).await;
        self.settled(made, answer).await
    }

    async fn clock(&self, client_ms: i64) -> Result<ClockReply, GroupError> {
        let s = self
            .group()
            .clock(&self.call, unms(client_ms))
            .await
            .map_err(group_error)?;
        Ok(ClockReply {
            client_ms_echo: ms(s.client_ms_echo),
            recv_ms: ms(s.recv_ms),
            send_ms: ms(s.send_ms),
        })
    }

    async fn ping(&self) -> Result<(), GroupError> {
        let m = self.membership()?;
        m.ping(&self.call).await.map_err(group_error)
    }

    async fn clock_report(&self, t1: i64, t2: i64, t3: i64, t4: i64) -> Result<(), GroupError> {
        let m = self.membership()?;
        m.clock_report(&self.call, unms(t1), unms(t2), unms(t3), unms(t4))
            .await
            .map_err(group_error)
    }

    async fn unpause(&self) -> Result<(), GroupError> {
        let m = self.membership()?;
        m.unpause(&self.call).await.map_err(group_error)
    }

    async fn pause(&self) -> Result<(), GroupError> {
        let m = self.membership()?;
        m.pause(&self.call).await.map_err(group_error)
    }

    async fn seek(&self, position_ms: u64) -> Result<(), GroupError> {
        let m = self.membership()?;
        m.seek(&self.call, position_ms).await.map_err(group_error)
    }

    async fn buffering(
        &self,
        playlist_item_id: String,
        position_ms: u64,
        buffering: bool,
    ) -> Result<(), GroupError> {
        let m = self.membership()?;
        m.buffering(&self.call, position_ms, buffering, &playlist_item_id)
            .await
            .map_err(group_error)
    }

    async fn ready(
        &self,
        playlist_item_id: String,
        position_ms: u64,
        playing: bool,
    ) -> Result<(), GroupError> {
        let m = self.membership()?;
        m.ready(&self.call, position_ms, playing, &playlist_item_id)
            .await
            .map_err(group_error)
    }

    async fn seen(&self, ids: Vec<ItemRef>) -> Result<Vec<MemberSeen>, GroupError> {
        let m = self.membership()?;
        m.seen(&self.call, &ids).await.map_err(group_error)
    }

    async fn set_tracks(
        &self,
        audio: TrackChoice,
        subtitle: TrackChoice,
        playlist_item_id: String,
    ) -> Result<(), GroupError> {
        let m = self.membership()?;
        m.set_tracks(&self.call, audio, subtitle, &playlist_item_id)
            .await
            .map_err(group_error)
    }

    async fn set_item(
        &self,
        item: ItemRef,
        position_ms: u64,
        playlist_item_id: String,
    ) -> Result<(), GroupError> {
        let m = self.membership()?;
        m.set_item(&self.call, item, position_ms, &playlist_item_id)
            .await
            .map_err(group_error)
    }

    async fn follow_tracks(&self, follow: bool) -> Result<(), GroupError> {
        let m = self.membership()?;
        m.follow_tracks(&self.call, follow)
            .await
            .map_err(group_error)
    }

    async fn leave(&self) -> Result<(), GroupError> {
        let Some(m) = self.member.borrow_mut().take() else {
            return Ok(());
        };
        m.leave(&self.call).await.map_err(group_error)
    }
}
