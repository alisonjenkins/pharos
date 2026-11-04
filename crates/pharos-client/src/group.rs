//! Watch together (014) over the native API: `Session.group` as plain Rust
//! types. [`NativeGroup`] lists, creates and joins groups and reads the
//! server clock; [`NativeMembership`] is one joined member's commands; a
//! [`GroupUpdate`] is what the server pushes to the callback handed to
//! `create` or `join`. Times are unix milliseconds, as on the wire.

use std::rc::Rc;

use pharos_api::pharos_capnp::{
    clock_result, group, group_member, group_sink, group_summary, group_tracks, group_update,
    groups_result, member_seen, membership, membership_result, seen_result, session,
};
use pharos_api::{server, Connection};

use crate::api::{read_item_ref, set_item_ref, text, ItemRef};
use crate::{read_done, CallInfo, ClientError};

/// Someone in a group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupMember {
    pub id: String,
    pub name: String,
    pub buffering: bool,
    /// On their own audio and subtitles, not the group's (US7).
    pub own_tracks: bool,
}

pub use pharos_api::track_choice::TrackChoice;

/// The group's audio and subtitle choice, and the member who made it last.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupTracks {
    pub audio: TrackChoice,
    pub subtitle: TrackChoice,
    /// Member id of the last change; empty before any.
    pub by: String,
}

/// A group as the server lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupSummary {
    pub id: String,
    pub name: String,
    /// What the group is watching; `None` before it has an item.
    pub item: Option<ItemRef>,
    pub members: Vec<GroupMember>,
    /// What a late joiner starts on.
    pub tracks: GroupTracks,
}

/// The server's half of one clock round trip: it received the client's
/// `client_ms_echo` at `recv_ms` and replied at `send_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockSample {
    pub client_ms_echo: u64,
    pub recv_ms: u64,
    pub send_ms: u64,
}

/// Ids one `seen` call may ask about.
pub use pharos_api::MAX_SEEN_IDS;

/// One item as one member has seen it. A film or episode has `played` and
/// `position_pct`; a series or season has `played_count` of `total_count`
/// episodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemSeen {
    pub id: ItemRef,
    pub played: bool,
    pub position_pct: u8,
    pub played_count: u32,
    pub total_count: u32,
}

/// What one group member has seen of the items asked about. An item the
/// caller may not see is missing from every member's `items`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberSeen {
    pub member: GroupMember,
    pub items: Vec<ItemSeen>,
}

/// What the server pushes to a member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupUpdate {
    Joined(GroupSummary),
    Play {
        at_server_ms: u64,
        position_ms: u64,
        /// The member who caused it; `None` when the group did.
        by: Option<String>,
    },
    Pause {
        at_server_ms: u64,
        position_ms: u64,
        /// The member who caused it; `None` when the group did.
        by: Option<String>,
    },
    Seek {
        at_server_ms: u64,
        position_ms: u64,
        /// The member who caused it; `None` when the group did.
        by: Option<String>,
    },
    /// A member joined, or its buffering changed.
    Member(GroupMember),
    /// A member left (its id).
    Left(String),
    /// The group moved to another item.
    Item {
        item: ItemRef,
        playlist_item_id: String,
        position_ms: u64,
        playing: bool,
    },
    /// The group's audio or subtitle changed.
    Tracks(GroupTracks),
    /// The group is over for this member, whatever the reason.
    Ended,
}

/// `Session.group`.
#[derive(Clone)]
pub struct NativeGroup(group::Client);

impl NativeGroup {
    /// Pipelined: no round trip until the first call.
    pub fn new(session: &session::Client) -> Self {
        Self(session.group_request().send().pipeline.get_group())
    }

    /// The groups the signed-in user may join.
    pub async fn list(&self, call: &CallInfo) -> Result<Vec<GroupSummary>, ClientError> {
        let mut req = self.0.list_request();
        call.write(req.get().init_ctx());
        let reply = req.send().promise.await?;
        match reply
            .get()?
            .get_result()?
            .which()
            .map_err(capnp::Error::from)?
        {
            groups_result::Groups(list) => {
                Ok(list?.iter().map(read_summary).collect::<Result<_, _>>()?)
            }
            groups_result::Error(e) => Err(ClientError::from_api(e?)),
        }
    }

    /// Starts a group on `item` at `position_ms`, with this caller as its
    /// first member. `on_update` runs for each pushed update, in order.
    pub async fn create(
        &self,
        call: &CallInfo,
        item: ItemRef,
        position_ms: u64,
        on_update: impl Fn(GroupUpdate) + 'static,
    ) -> Result<NativeMembership, ClientError> {
        let mut req = self.0.create_request();
        {
            let mut p = req.get();
            call.write(p.reborrow().init_ctx());
            set_item_ref(p.reborrow().init_item(), item);
            p.set_position_ms(position_ms);
            p.set_sink(group_sink_client(on_update));
        }
        let reply = req.send().promise.await?;
        read_membership(reply.get()?.get_result()?)
    }

    /// Joins the group `id`; `notFound` when it has ended.
    pub async fn join(
        &self,
        call: &CallInfo,
        id: &str,
        on_update: impl Fn(GroupUpdate) + 'static,
    ) -> Result<NativeMembership, ClientError> {
        let mut req = self.0.join_request();
        {
            let mut p = req.get();
            call.write(p.reborrow().init_ctx());
            p.reborrow().init_id().set_value(id);
            p.set_sink(group_sink_client(on_update));
        }
        let reply = req.send().promise.await?;
        read_membership(reply.get()?.get_result()?)
    }

    /// One clock round trip: send `client_ms` (T1), get the server's T2 and T3.
    pub async fn clock(&self, call: &CallInfo, client_ms: u64) -> Result<ClockSample, ClientError> {
        let mut req = self.0.clock_request();
        {
            let mut p = req.get();
            call.write(p.reborrow().init_ctx());
            p.set_client_ms(client_ms);
        }
        let reply = req.send().promise.await?;
        match reply
            .get()?
            .get_result()?
            .which()
            .map_err(capnp::Error::from)?
        {
            clock_result::Sample(s) => {
                let s = s?;
                Ok(ClockSample {
                    client_ms_echo: s.get_client_ms_echo(),
                    recv_ms: s.get_recv_ms(),
                    send_ms: s.get_send_ms(),
                })
            }
            clock_result::Error(e) => Err(ClientError::from_api(e?)),
        }
    }
}

/// One joined member of one group. Dropping it is a lost connection to the
/// server, not a leave: call [`NativeMembership::leave`].
#[derive(Clone)]
pub struct NativeMembership(membership::Client);

macro_rules! command {
    ($self:ident, $call:ident, $request:ident) => {
        command!($self, $call, $request, |_p| {})
    };
    ($self:ident, $call:ident, $request:ident, |$p:ident| $fill:block) => {{
        let mut req = $self.0.$request();
        {
            #[allow(unused_mut)]
            let mut $p = req.get();
            $call.write($p.reborrow().init_ctx());
            $fill
        }
        let reply = req.send().promise.await?;
        read_done(reply.get()?.get_result()?)
    }};
}

impl NativeMembership {
    pub async fn unpause(&self, call: &CallInfo) -> Result<(), ClientError> {
        command!(self, call, unpause_request)
    }

    pub async fn pause(&self, call: &CallInfo) -> Result<(), ClientError> {
        command!(self, call, pause_request)
    }

    pub async fn seek(&self, call: &CallInfo, position_ms: u64) -> Result<(), ClientError> {
        command!(self, call, seek_request, |p| {
            p.set_position_ms(position_ms);
        })
    }

    /// `playlist_item_id` is the queue entry the report is for (from the
    /// last `Item` update); the group drops a stale one.
    pub async fn buffering(
        &self,
        call: &CallInfo,
        position_ms: u64,
        buffering: bool,
        playlist_item_id: &str,
    ) -> Result<(), ClientError> {
        command!(self, call, buffering_request, |p| {
            p.set_position_ms(position_ms);
            p.set_buffering(buffering);
            p.set_playlist_item_id(playlist_item_id);
        })
    }

    pub async fn ready(
        &self,
        call: &CallInfo,
        position_ms: u64,
        playing: bool,
        playlist_item_id: &str,
    ) -> Result<(), ClientError> {
        command!(self, call, ready_request, |p| {
            p.set_position_ms(position_ms);
            p.set_playing(playing);
            p.set_playlist_item_id(playlist_item_id);
        })
    }

    pub async fn leave(&self, call: &CallInfo) -> Result<(), ClientError> {
        command!(self, call, leave_request)
    }

    /// Liveness beacon, every 30 s.
    pub async fn ping(&self, call: &CallInfo) -> Result<(), ClientError> {
        command!(self, call, ping_request)
    }

    /// What each member of the group has seen of `ids` (at most 200).
    pub async fn seen(
        &self,
        call: &CallInfo,
        ids: &[ItemRef],
    ) -> Result<Vec<MemberSeen>, ClientError> {
        let mut req = self.0.seen_request();
        {
            let mut p = req.get();
            call.write(p.reborrow().init_ctx());
            let mut list = p.init_ids(u32::try_from(ids.len()).unwrap_or(u32::MAX));
            for (i, id) in (0u32..).zip(ids) {
                set_item_ref(list.reborrow().get(i), *id);
            }
        }
        let reply = req.send().promise.await?;
        match reply
            .get()?
            .get_result()?
            .which()
            .map_err(capnp::Error::from)?
        {
            seen_result::Members(members) => Ok(members?
                .iter()
                .map(read_member_seen)
                .collect::<Result<_, _>>()?),
            seen_result::Error(e) => Err(ClientError::from_api(e?)),
        }
    }

    /// Closes a clock round trip for the server's own estimate of the RTT.
    pub async fn clock_report(
        &self,
        call: &CallInfo,
        t1: u64,
        t2: u64,
        t3: u64,
        t4: u64,
    ) -> Result<(), ClientError> {
        command!(self, call, clock_report_request, |p| {
            p.set_t1(t1);
            p.set_t2(t2);
            p.set_t3(t3);
            p.set_t4(t4);
        })
    }
}

fn read_membership(result: membership_result::Reader<'_>) -> Result<NativeMembership, ClientError> {
    read_done(result.get_status()?)?;
    Ok(NativeMembership(result.get_membership()?))
}

fn read_member_seen(m: member_seen::Reader<'_>) -> Result<MemberSeen, capnp::Error> {
    Ok(MemberSeen {
        member: read_member(m.get_member()?)?,
        items: m
            .get_items()?
            .iter()
            .map(|i| {
                Ok(ItemSeen {
                    id: read_item_ref(i.get_id()?)?,
                    played: i.get_played(),
                    position_pct: i.get_position_pct(),
                    played_count: i.get_played_count(),
                    total_count: i.get_total_count(),
                })
            })
            .collect::<Result<_, capnp::Error>>()?,
    })
}

fn read_member(m: group_member::Reader<'_>) -> Result<GroupMember, capnp::Error> {
    Ok(GroupMember {
        id: text(m.get_id())?,
        name: text(m.get_name())?,
        buffering: m.get_buffering(),
        own_tracks: m.get_own_tracks(),
    })
}

fn read_summary(g: group_summary::Reader<'_>) -> Result<GroupSummary, capnp::Error> {
    Ok(GroupSummary {
        id: g.get_id()?.get_value()?.to_string()?,
        name: text(g.get_name())?,
        item: match g.has_item() {
            true => Some(read_item_ref(g.get_item()?)?),
            false => None,
        },
        members: g
            .get_members()?
            .iter()
            .map(read_member)
            .collect::<Result<_, _>>()?,
        tracks: read_tracks(g.get_tracks()?)?,
    })
}

fn read_tracks(t: group_tracks::Reader<'_>) -> Result<GroupTracks, capnp::Error> {
    Ok(GroupTracks {
        audio: pharos_api::track_choice::read(t.get_audio()?),
        subtitle: pharos_api::track_choice::read(t.get_subtitle()?),
        by: text(t.get_by())?,
    })
}

fn by(a: &group_update::at::Reader<'_>) -> Result<Option<String>, capnp::Error> {
    let by = text(a.get_by())?;
    Ok((!by.is_empty()).then_some(by))
}

fn read_update(update: group_update::Reader<'_>) -> Result<GroupUpdate, capnp::Error> {
    Ok(match update.which()? {
        group_update::Joined(g) => GroupUpdate::Joined(read_summary(g?)?),
        group_update::Play(a) => {
            let a = a?;
            GroupUpdate::Play {
                at_server_ms: a.get_at_server_ms(),
                position_ms: a.get_position_ms(),
                by: by(&a)?,
            }
        }
        group_update::Pause(a) => {
            let a = a?;
            GroupUpdate::Pause {
                at_server_ms: a.get_at_server_ms(),
                position_ms: a.get_position_ms(),
                by: by(&a)?,
            }
        }
        group_update::Seek(a) => {
            let a = a?;
            GroupUpdate::Seek {
                at_server_ms: a.get_at_server_ms(),
                position_ms: a.get_position_ms(),
                by: by(&a)?,
            }
        }
        group_update::Member(m) => GroupUpdate::Member(read_member(m?)?),
        group_update::Left(id) => GroupUpdate::Left(text(id)?),
        group_update::Item(i) => {
            let i = i?;
            GroupUpdate::Item {
                item: read_item_ref(i.get_item()?)?,
                playlist_item_id: text(i.get_playlist_item_id())?,
                position_ms: i.get_position_ms(),
                playing: i.get_playing(),
            }
        }
        group_update::Tracks(t) => GroupUpdate::Tracks(read_tracks(t?)?),
        group_update::Ended(_) => GroupUpdate::Ended,
    })
}

/// The `GroupSink` capability a join or create hands the server: each update
/// it pushes is read and passed to `on_update`.
fn group_sink_client(on_update: impl Fn(GroupUpdate) + 'static) -> group_sink::Client {
    server::group_sink::client(UpdateSink(on_update), &Connection::new())
}

struct UpdateSink<F>(F);

impl<F: Fn(GroupUpdate) + 'static> server::group_sink::Handler for UpdateSink<F> {
    async fn update(
        self: Rc<Self>,
        params: group_sink::UpdateParams,
        results: &mut group_sink::UpdateResults,
    ) -> Result<(), capnp::Error> {
        let update = read_update(params.get()?.get_update()?)?;
        (self.0)(update);
        results.get().init_result().set_ok(());
        Ok(())
    }
}
