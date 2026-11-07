//! `Group` and `Membership`: a native client listing, creating, joining and
//! driving the same SyncPlay groups jellyfin-web uses (spec 014). The groups,
//! access rules, queue checks and command routing are the Jellyfin handlers'
//! own (`api::jellyfin::syncplay`, `group_ops`), so a native member and a
//! Jellyfin member can share a group.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use pharos_api::pharos_capnp::{
    done, group as wire, group_sink, group_summary, group_tracks, group_update,
    membership as member_wire, membership_result, seen_result, track_choice, EndReason,
};
use pharos_api::track_choice::{self as codec, TrackChoice as WireChoice};
use pharos_api::{server, Connection, Denial};
use pharos_jellyfin_api::dto::{wire_item_id, WireId};
use pharos_sync::group::{GroupHandle, GroupMsg};
use pharos_sync::messages::{GroupId, GroupTracks, ServerMsg, TrackChoice};
use pharos_sync::MemberId;
use tokio::sync::mpsc;

use super::library::{read_item_id, set_item_id};
use super::session::Session;
use crate::api::jellyfin::syncplay::{
    add_caller_to_group, end_owned_cast, listed_groups, may_queue, prewarm_seek,
    resolve_runtimes_ms, sync_play_allows, GroupHiddenRoute, ListedGroup, QueueVia, SyncPlayNeed,
};
use crate::group_ops::{route_command, CommandOutcome, SyncDeps};
use crate::services::group_seen::{self, MemberSeen, Outcome};

/// Updates queued for a member's `GroupSink` before the actor's delivery to
/// it backs up; the Jellyfin `/socket` queues the same.
const SINK_QUEUE: usize = 64;

pub(super) struct Group {
    session: Rc<Session>,
}

impl Group {
    pub(super) fn new(session: Rc<Session>) -> Self {
        Self { session }
    }

    /// The session's current user and the running SyncPlay machinery, or why
    /// neither is available.
    async fn open(&self) -> Result<(pharos_core::User, SyncDeps), Denial> {
        let user = self.session.current_user().await?;
        let deps = self
            .session
            .state
            .sync
            .get()
            .cloned()
            .ok_or(Denial::Unavailable {
                dependency: "syncplay",
            })?;
        Ok((user, deps))
    }

    /// The group `id` names that `user` may join: live, or held by a fresh
    /// persisted snapshot or another replica (then it is hydrated or routed,
    /// as a Jellyfin join does). `None` when the group is gone OR the user may
    /// not see it: the same decision as the other join transports
    /// (`joinable_group`), so a hidden group cannot be told from an absent
    /// one, and a refused probe hydrates nothing. Never creates a group.
    async fn joinable_group(
        &self,
        deps: &SyncDeps,
        user: &pharos_core::User,
        id: GroupId,
    ) -> Result<Option<GroupHandle>, Denial> {
        crate::sync_recovery::joinable_group(
            &self.session.state,
            &deps.registry,
            user,
            id,
            GroupHiddenRoute::RpcJoin,
        )
        .await
        .map_err(|_| Denial::Unavailable {
            dependency: "syncplay",
        })
    }

    /// Takes the call's device out of the group it is still in, when that is
    /// not `keep`: out of the roster, its old membership's sink told
    /// `ended(left)`. A device is in one group at a time.
    async fn leave_previous(
        &self,
        deps: &SyncDeps,
        user: &pharos_core::User,
        device: &str,
        keep: Option<GroupId>,
    ) {
        let key = member_key(user, device);
        let Some(sess) = deps.hub.resolve(&key) else {
            return;
        };
        let Some(old) = sess.group.clone() else {
            return;
        };
        if Some(old.group_id) == keep {
            return;
        }
        end_owned_cast(&self.session.state, user.id, sess.member_id).await;
        deps.hub.detach_group(&key);
        let _ = old
            .tx
            .send(GroupMsg::RemoveMember {
                member_id: sess.member_id,
            })
            .await;
        let _ = sess.sink.send(ServerMsg::GroupLeft).await;
        tracing::info!(device_id = %key, from = %old.group_id, "syncplay: native device left its previous group");
    }

    /// Registers the call's device in the hub as a member bound to `sink`,
    /// ready for `add_caller_to_group`.
    fn register(
        &self,
        deps: &SyncDeps,
        user: &pharos_core::User,
        device: &str,
        sink: group_sink::Client,
        group: &GroupHandle,
    ) -> Membership {
        let key = member_key(user, device);
        let (tx, rx) = mpsc::channel(SINK_QUEUE);
        let reg = deps
            .hub
            .register(key.clone(), user.name.clone(), tx.clone());
        deps.sinks.insert(reg.member_id, reg.gen, tx);
        let pump = actix_web::rt::spawn(pump(rx, sink, group.clone(), reg.member_id));
        // The capability is not always dropped when its connection closes
        // (observed live: the seat outlived the socket until the silence
        // TTL), so the close itself starts the same loss.
        let (hook_deps, hook_key) = (deps.clone(), key.clone());
        let (hook_id, hook_gen, hook_group) = (reg.member_id, reg.gen, group.tx.clone());
        let hook_pump = pump.abort_handle();
        self.session.on_connection_close(move || {
            hook_pump.abort();
            if hook_deps.hub.conn_gen(&hook_key) != Some(hook_gen) {
                return;
            }
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(lose_seat(
                    hook_deps, hook_key, hook_id, hook_gen, hook_group,
                ));
            }
        });
        Membership {
            session: Rc::clone(&self.session),
            deps: deps.clone(),
            key,
            member_id: reg.member_id,
            gen: reg.gen,
            group: group.clone(),
            left: Cell::new(false),
            pump: pump.abort_handle(),
        }
    }
}

/// A device's key in the hub: the user folded into `ctx.device` (as
/// `AuthSession::sync_key`), so a device's member id is stable across
/// reconnects (V25).
fn member_key(user: &pharos_core::User, device: &str) -> String {
    format!("{}:{device}", user.id.0.simple())
}

/// Server wall-clock, unix ms: the base `atServerMs` is expressed in.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// A group as the wire shows it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Summary {
    id: String,
    name: String,
    item: Option<u64>,
    members: Vec<Member>,
    /// What a late joiner starts on.
    tracks: GroupTracks,
    /// The receiving member's own id; set only in `joined`.
    you: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Member {
    id: String,
    name: String,
    buffering: bool,
    own_tracks: bool,
}

impl From<&ListedGroup> for Summary {
    fn from(group: &ListedGroup) -> Self {
        Self {
            id: group.id.clone(),
            name: group.name.clone(),
            item: real_item(group.item_id.as_deref()),
            // A listing knows no member's buffering.
            members: group
                .participant_ids
                .iter()
                .zip(&group.participants)
                .map(|(id, name)| Member {
                    id: id.clone(),
                    name: name.clone(),
                    buffering: false,
                    own_tracks: false,
                })
                .collect(),
            tracks: GroupTracks::default(),
            you: String::new(),
        }
    }
}

fn real_item(wire: Option<&str>) -> Option<u64> {
    match wire.and_then(WireId::parse) {
        Some(WireId::Real(id)) => Some(id),
        _ => None,
    }
}

fn write_member(mut out: pharos_api::pharos_capnp::group_member::Builder<'_>, m: &Member) {
    out.set_id(m.id.as_str());
    out.set_name(m.name.as_str());
    out.set_buffering(m.buffering);
    out.set_own_tracks(m.own_tracks);
}

fn write_choice(out: track_choice::Builder<'_>, choice: TrackChoice) {
    codec::write(
        out,
        match choice {
            TrackChoice::Unset => WireChoice::Unset,
            TrackChoice::Index(index) => WireChoice::Index(index),
            TrackChoice::Off => WireChoice::Off,
        },
    );
}

fn read_choice(choice: track_choice::Reader<'_>) -> TrackChoice {
    match codec::read(choice) {
        WireChoice::Unset => TrackChoice::Unset,
        WireChoice::Index(index) => TrackChoice::Index(index),
        WireChoice::Off => TrackChoice::Off,
    }
}

fn write_tracks(mut out: group_tracks::Builder<'_>, tracks: &GroupTracks) {
    write_choice(out.reborrow().init_audio(), tracks.audio);
    write_choice(out.reborrow().init_subtitle(), tracks.subtitle);
    out.set_by(
        tracks
            .by
            .map(|m| m.to_string())
            .unwrap_or_default()
            .as_str(),
    );
}

fn write_summary(mut out: group_summary::Builder<'_>, group: &Summary) {
    out.reborrow().init_id().set_value(group.id.as_str());
    out.set_name(group.name.as_str());
    if let Some(id) = group.item {
        set_item_id(out.reborrow().init_item(), WireId::Real(id));
    }
    write_tracks(out.reborrow().init_tracks(), &group.tracks);
    out.reborrow().set_you(group.you.as_str());
    let mut members = out.init_members(u32::try_from(group.members.len()).unwrap_or(u32::MAX));
    for (i, m) in (0u32..).zip(&group.members) {
        write_member(members.reborrow().get(i), m);
    }
}

fn write_seen(out: seen_result::Builder<'_>, members: &[MemberSeen]) {
    let mut out = out.init_members(u32::try_from(members.len()).unwrap_or(u32::MAX));
    for (i, m) in (0u32..).zip(members) {
        let mut entry = out.reborrow().get(i);
        write_member(
            entry.reborrow().init_member(),
            &Member {
                id: m.member_id.clone(),
                name: m.name.clone(),
                buffering: m.buffering,
                own_tracks: m.own_tracks,
            },
        );
        let mut items = entry.init_items(u32::try_from(m.items.len()).unwrap_or(u32::MAX));
        for (j, item) in (0u32..).zip(&m.items) {
            let mut out = items.reborrow().get(j);
            set_item_id(out.reborrow().init_id(), item.id);
            out.set_played(item.played);
            out.set_position_pct(item.position_pct);
            out.set_played_count(item.played_count);
            out.set_total_count(item.total_count);
        }
    }
}

/// What a group tells a native member, before it is written to the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Update {
    Joined(Summary),
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
    Member(Member),
    /// A member went onto, or came back from, its own tracks; the pump turns
    /// it into a `Member` with the name and buffering it already knows.
    MemberTracks {
        id: String,
        own_tracks: bool,
    },
    Tracks(GroupTracks),
    Left(String),
    Item {
        item: u64,
        playlist_item_id: String,
        position_ms: u64,
        playing: bool,
    },
    Ended(EndReason),
}

/// The native form of what the actor delivered, or `None` for a message the
/// native protocol has no use for (the clock, leader changes, state labels).
/// `epoch_unix_ms` turns the actor's monotonic times into unix ms, the base
/// `Group.clock` answers in.
fn translate(msg: &ServerMsg, group: GroupId, epoch_unix_ms: u64) -> Option<Update> {
    let at = |server_ms: u64| epoch_unix_ms.saturating_add(server_ms);
    Some(match msg {
        ServerMsg::Joined { members, .. } => Update::Joined(Summary {
            id: group.to_string(),
            members: members
                .iter()
                .map(|m| Member {
                    id: m.member_id.to_string(),
                    name: m.name.clone(),
                    buffering: false,
                    own_tracks: false,
                })
                .collect(),
            ..Summary::default()
        }),
        ServerMsg::Play {
            at_server_ms,
            position_ms,
            by,
        } => Update::Play {
            at_server_ms: at(*at_server_ms),
            position_ms: *position_ms,
            by: by.map(|m| m.to_string()),
        },
        ServerMsg::Pause {
            at_server_ms,
            position_ms,
            by,
        } => Update::Pause {
            at_server_ms: at(*at_server_ms),
            position_ms: *position_ms,
            by: by.map(|m| m.to_string()),
        },
        ServerMsg::Seek {
            at_server_ms,
            position_ms,
            by,
        } => Update::Seek {
            at_server_ms: at(*at_server_ms),
            position_ms: *position_ms,
            by: by.map(|m| m.to_string()),
        },
        ServerMsg::MemberJoined { member } => Update::Member(Member {
            id: member.member_id.to_string(),
            name: member.name.clone(),
            buffering: false,
            own_tracks: false,
        }),
        ServerMsg::Tracks { tracks } => Update::Tracks(tracks.clone()),
        ServerMsg::MemberTracks {
            member_id,
            own_tracks,
        } => Update::MemberTracks {
            id: member_id.to_string(),
            own_tracks: *own_tracks,
        },
        ServerMsg::MemberLeft { member_id, .. } => Update::Left(member_id.to_string()),
        ServerMsg::PlayQueue {
            items,
            playing_index,
            start_position_ms,
            is_playing,
            ..
        } => {
            let entry = items.get(*playing_index)?;
            Update::Item {
                item: real_item(Some(&entry.item_id))?,
                playlist_item_id: entry.playlist_item_id.clone(),
                position_ms: *start_position_ms,
                playing: *is_playing,
            }
        }
        ServerMsg::GroupLeft => Update::Ended(EndReason::Left),
        ServerMsg::NotInGroup | ServerMsg::GroupDoesNotExist => {
            Update::Ended(EndReason::NotInGroup)
        }
        ServerMsg::Welcome { .. }
        | ServerMsg::Pong { .. }
        | ServerMsg::LeaderChange { .. }
        | ServerMsg::StateUpdate { .. }
        | ServerMsg::Error { .. } => return None,
    })
}

fn write_update(mut out: group_update::Builder<'_>, update: &Update) {
    match update {
        Update::Joined(summary) => write_summary(out.init_joined(), summary),
        Update::Play {
            at_server_ms,
            position_ms,
            by,
        } => {
            let mut at = out.init_play();
            at.set_at_server_ms(*at_server_ms);
            at.set_position_ms(*position_ms);
            at.set_by(by.as_deref().unwrap_or_default());
        }
        Update::Pause {
            at_server_ms,
            position_ms,
            by,
        } => {
            let mut at = out.init_pause();
            at.set_at_server_ms(*at_server_ms);
            at.set_position_ms(*position_ms);
            at.set_by(by.as_deref().unwrap_or_default());
        }
        Update::Seek {
            at_server_ms,
            position_ms,
            by,
        } => {
            let mut at = out.init_seek();
            at.set_at_server_ms(*at_server_ms);
            at.set_position_ms(*position_ms);
            at.set_by(by.as_deref().unwrap_or_default());
        }
        Update::Member(member) => write_member(out.init_member(), member),
        Update::Tracks(tracks) => write_tracks(out.init_tracks(), tracks),
        // The pump turns it into a `Member` before anything is written.
        Update::MemberTracks { .. } => {}
        Update::Left(id) => out.set_left(id.as_str()),
        Update::Item {
            item,
            playlist_item_id,
            position_ms,
            playing,
        } => {
            let mut at = out.init_item();
            set_item_id(at.reborrow().init_item(), WireId::Real(*item));
            at.set_playlist_item_id(playlist_item_id.as_str());
            at.set_position_ms(*position_ms);
            at.set_playing(*playing);
        }
        Update::Ended(reason) => out.set_ended(*reason),
    }
}

/// How often the pump re-reads who is buffering. The actor announces no
/// buffering change itself, and while the group is paused none of the commands
/// the pump watches follows one, so a stall or recovery is picked up here.
const BUFFERING_POLL: std::time::Duration = std::time::Duration::from_secs(2);

/// Delivers a member's group messages to its `GroupSink` in order, until the
/// member's channel closes or the sink goes away.
///
/// The actor announces no member's buffering to the others, so the pump reads
/// it from the group's snapshot: after each message that follows a change in
/// it (a freeze or resume, a gate opening or closing) and every
/// [`BUFFERING_POLL`], and sends a `member` update for each member whose flag
/// moved.
async fn pump(
    mut rx: mpsc::Receiver<ServerMsg>,
    sink: group_sink::Client,
    group: GroupHandle,
    me: MemberId,
) {
    let mut roster: HashMap<String, String> = HashMap::new();
    let mut buffering: HashSet<String> = HashSet::new();
    let mut own_tracks: HashSet<String> = HashSet::new();
    let mut poll =
        tokio::time::interval_at(tokio::time::Instant::now() + BUFFERING_POLL, BUFFERING_POLL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let mut updates = Vec::new();
        let msg = tokio::select! {
            msg = rx.recv() => {
                let Some(msg) = msg else { return };
                Some(msg)
            }
            _ = poll.tick() => None,
        };
        if let Some(msg) = &msg {
            if let Some(mut update) = translate(msg, group.group_id, group.epoch_unix_ms) {
                let mut member_update = None;
                match &mut update {
                    Update::Joined(summary) => {
                        summary.you = me.to_string();
                        // `Joined` carries the roster only; the name, the
                        // item playing, who is buffering and the group's
                        // tracks (what a late joiner starts on) are the
                        // group's own.
                        if let Some(snap) = group.snapshot().await {
                            summary.name = snap.group_name;
                            summary.tracks = snap.tracks;
                            own_tracks = snap
                                .own_tracks_members
                                .iter()
                                .map(ToString::to_string)
                                .collect();
                            summary.item = real_item(
                                snap.current_item_id
                                    .as_deref()
                                    .or_else(|| snap.queue_item_ids.first().map(String::as_str)),
                            );
                            buffering = snap
                                .buffering_members
                                .iter()
                                .map(ToString::to_string)
                                .collect();
                            for m in &mut summary.members {
                                m.buffering = buffering.contains(&m.id);
                                m.own_tracks = own_tracks.contains(&m.id);
                            }
                        }
                        roster = summary
                            .members
                            .iter()
                            .map(|m| (m.id.clone(), m.name.clone()))
                            .collect();
                    }
                    Update::Member(m) => {
                        roster.insert(m.id.clone(), m.name.clone());
                    }
                    Update::Left(id) => {
                        roster.remove(id);
                        buffering.remove(id);
                        own_tracks.remove(id);
                    }
                    Update::MemberTracks {
                        id,
                        own_tracks: own,
                    } => {
                        if *own {
                            own_tracks.insert(id.clone());
                        } else {
                            own_tracks.remove(id);
                        }
                        member_update = Some(Update::Member(Member {
                            id: id.clone(),
                            name: roster.get(id).cloned().unwrap_or_default(),
                            buffering: buffering.contains(id),
                            own_tracks: *own,
                        }));
                    }
                    _ => {}
                }
                if let Some(member) = member_update {
                    update = member;
                }
                updates.push(update);
            }
        }
        if msg.as_ref().map_or(true, changes_buffering) {
            if let Some(snap) = group.snapshot().await {
                let now: HashSet<String> = snap
                    .buffering_members
                    .iter()
                    .map(ToString::to_string)
                    .collect();
                for id in now.symmetric_difference(&buffering) {
                    updates.push(Update::Member(Member {
                        id: id.clone(),
                        name: roster.get(id).cloned().unwrap_or_default(),
                        buffering: now.contains(id),
                        own_tracks: own_tracks.contains(id),
                    }));
                }
                buffering = now;
            }
        }
        for update in updates {
            let mut req = sink.update_request();
            req.get().init_ctx();
            write_update(req.get().init_update(), &update);
            if let Err(e) = req.send().promise.await {
                tracing::info!(group = %group.group_id, error = %e, "syncplay: native sink gone — stopping its updates");
                return;
            }
        }
    }
}

/// Whether `msg` follows a change in who is buffering: the group froze or
/// resumed, or a gate opened or closed.
fn changes_buffering(msg: &ServerMsg) -> bool {
    matches!(
        msg,
        ServerMsg::Play { .. }
            | ServerMsg::Pause { .. }
            | ServerMsg::Seek { .. }
            | ServerMsg::StateUpdate { .. }
            | ServerMsg::PlayQueue { .. }
    )
}

/// Answers `create`/`join`: the membership, or a refused one whose every call
/// fails with the same denial, so a pipelined call chain never dangles.
fn answer(
    mut out: membership_result::Builder<'_>,
    conn: &Rc<Connection>,
    made: Result<Membership, Denial>,
) {
    match made {
        Ok(member) => {
            out.reborrow().init_status().set_ok(());
            out.set_membership(server::membership::client(member, conn));
        }
        Err(denial) => {
            denial.write(out.reborrow().init_status().init_error());
            out.set_membership(server::membership::client(
                server::membership::Refused(Rc::clone(conn), Rc::new(denial)),
                conn,
            ));
        }
    }
}

impl server::group::Handler for Group {
    async fn list(
        self: Rc<Self>,
        _params: wire::ListParams,
        results: &mut wire::ListResults,
    ) -> Result<(), capnp::Error> {
        let out = results.get().init_result();
        let listed = async {
            let (user, deps) = self.open().await?;
            if !sync_play_allows(&user, SyncPlayNeed::Join, "rpc_list") {
                return Err(Denial::NotPermitted { what: "syncplay" });
            }
            Ok(listed_groups(
                &self.session.state,
                &deps.registry,
                &user,
                GroupHiddenRoute::RpcList,
            )
            .await)
        }
        .await;
        match listed {
            Ok(groups) => {
                let mut list = out.init_groups(u32::try_from(groups.len()).unwrap_or(u32::MAX));
                for (i, group) in (0u32..).zip(&groups) {
                    write_summary(list.reborrow().get(i), &Summary::from(group));
                }
            }
            Err(denial) => denial.write(out.init_error()),
        }
        Ok(())
    }

    async fn create(
        self: Rc<Self>,
        params: wire::CreateParams,
        results: &mut wire::CreateResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let device = params.get_ctx()?.get_device()?.get_value()?.to_string()?;
        let item = read_item_id(params.get_item()?)?;
        let position_ms = params.get_position_ms();
        let sink = params.get_sink()?;
        let made = async {
            let (user, deps) = self.open().await?;
            if device.is_empty() {
                return Err(Denial::Invalid { field: "ctx.device" });
            }
            let WireId::Real(item) = item else {
                return Err(Denial::Invalid { field: "item" });
            };
            if !sync_play_allows(&user, SyncPlayNeed::Create, "rpc_create") {
                return Err(Denial::NotPermitted { what: "syncplay" });
            }
            let handle = deps.registry.create().await.map_err(|_| {
                tracing::error!("syncplay: group registry unreachable on native create");
                Denial::Unavailable {
                    dependency: "syncplay",
                }
            })?;
            self.leave_previous(&deps, &user, &device, None).await;
            let member = self.register(&deps, &user, &device, sink, &handle);
            add_caller_to_group(&deps.hub, &deps.sinks, &member.key, user.id, handle.clone())
                .await;
            let name = format!("{}'s Group", user.name);
            let _ = handle.tx.send(GroupMsg::SetGroupName { name }).await;
            let queued = vec![wire_item_id(item)];
            let item_runtimes_ms = resolve_runtimes_ms(&self.session.state, &queued).await;
            let outcome = route_command(
                &deps.hub,
                &deps.registry,
                &self.session.state.stores,
                Some(&member.key),
                "setnewqueue",
                async |group| {
                    may_queue(
                        &self.session.state,
                        &user,
                        group,
                        &queued,
                        QueueVia::SetNewQueue,
                    )
                    .await
                },
                |sender| GroupMsg::SetNewQueue {
                    sender,
                    item_ids: queued.clone(),
                    item_runtimes_ms,
                    playing_index: 0,
                    start_position_ms: position_ms,
                },
            )
            .await;
            match outcome {
                CommandOutcome::Dispatched | CommandOutcome::Recovered => {
                    tracing::info!(device_id = %member.key, group = %handle.group_id, "syncplay: native group created");
                    Ok(member)
                }
                _ => {
                    member.teardown().await;
                    Err(Denial::NotPermitted { what: "queueing" })
                }
            }
        }
        .await;
        answer(results.get().init_result(), &self.session.conn(), made);
        Ok(())
    }

    async fn join(
        self: Rc<Self>,
        params: wire::JoinParams,
        results: &mut wire::JoinResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let device = params.get_ctx()?.get_device()?.get_value()?.to_string()?;
        let id = params.get_id()?.get_value()?.to_string()?;
        let sink = params.get_sink()?;
        let made = async {
            let (user, deps) = self.open().await?;
            if device.is_empty() {
                return Err(Denial::Invalid { field: "ctx.device" });
            }
            let Ok(gid) = uuid::Uuid::parse_str(&id).map(GroupId) else {
                return Err(Denial::Invalid { field: "id" });
            };
            if !sync_play_allows(&user, SyncPlayNeed::Join, "rpc_join") {
                return Err(Denial::NotPermitted { what: "syncplay" });
            }
            // A group the caller may not see answers as a missing one.
            let Some(handle) = self.joinable_group(&deps, &user, gid).await? else {
                return Err(Denial::NotFound { what: "group" });
            };
            // Only now does the device move: a refused join leaves the
            // device's existing membership, if any, as it was.
            self.leave_previous(&deps, &user, &device, Some(handle.group_id))
                .await;
            let member = self.register(&deps, &user, &device, sink, &handle);
            tracing::info!(device_id = %member.key, group = %handle.group_id, "syncplay: native join");
            add_caller_to_group(&deps.hub, &deps.sinks, &member.key, user.id, handle).await;
            Ok(member)
        }
        .await;
        answer(results.get().init_result(), &self.session.conn(), made);
        Ok(())
    }

    async fn clock(
        self: Rc<Self>,
        params: wire::ClockParams,
        results: &mut wire::ClockResults,
    ) -> Result<(), capnp::Error> {
        let client_ms = params.get()?.get_client_ms();
        let recv = now_ms();
        let out = results.get().init_result();
        match self.session.current_user().await {
            Ok(_) => {
                let mut sample = out.init_sample();
                sample.set_client_ms_echo(client_ms);
                sample.set_recv_ms(recv);
                sample.set_send_ms(now_ms());
            }
            Err(denial) => denial.write(out.init_error()),
        }
        Ok(())
    }
}

/// One native member of one group: the device registered in the hub, the
/// pump feeding its sink, and the commands it may send.
pub(super) struct Membership {
    session: Rc<Session>,
    deps: SyncDeps,
    /// The device's key in the hub (user and `ctx.device`).
    key: String,
    member_id: MemberId,
    /// The hub generation this registration holds; a newer registration of
    /// the same device (a reconnect) supersedes it.
    gen: u64,
    group: GroupHandle,
    /// Set by `leave`: the member is out of the group and the hub.
    left: Cell<bool>,
    pump: tokio::task::AbortHandle,
}

impl Membership {
    /// Undoes `register`: no more updates, out of the roster, out of the hub.
    async fn teardown(&self) {
        self.pump.abort();
        if let Some(group) = self.deps.hub.remove_if_current_gen(&self.key, self.gen) {
            let _ = group
                .tx
                .send(GroupMsg::RemoveMember {
                    member_id: self.member_id,
                })
                .await;
        }
        self.deps.sinks.remove(self.member_id, self.gen);
    }
}

impl Drop for Membership {
    /// Dropping the capability, which is how a closed tab or a lost
    /// connection looks from here, is a socket loss and not a leave: the
    /// group keeps the seat for the grace so a reconnect (the same device
    /// joining again) takes it over, and removes it if none comes. After
    /// `leave` there is nothing to lose; the pump is left to deliver the last
    /// `ended` and end when its channel closes.
    fn drop(&mut self) {
        if self.left.get() {
            return;
        }
        self.pump.abort();
        if self.deps.hub.conn_gen(&self.key) != Some(self.gen) {
            return;
        }
        // The capability can be dropped as the runtime goes down.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(lose_seat(
            self.deps.clone(),
            self.key.clone(),
            self.member_id,
            self.gen,
            self.group.tx.clone(),
        ));
    }
}

/// A member's connection is gone: the group stops waiting on it at once and
/// keeps the seat through the grace, so the same device joining again takes
/// it over; if none does, the member is removed. Safe to run twice for one
/// registration (a dropped capability and a closed connection both run it):
/// the generation fence lets only the first removal act.
async fn lose_seat(
    deps: SyncDeps,
    key: String,
    member_id: MemberId,
    gen: u64,
    group: mpsc::Sender<GroupMsg>,
) {
    let _ = group.send(GroupMsg::MemberSocketLost { member_id }).await;
    tokio::time::sleep(deps.socket_loss_grace).await;
    // A drain leaves the membership for the next process (B26).
    if crate::state::is_shutting_down() {
        return;
    }
    // A newer registration of the device bumped the generation.
    if let Some(group) = deps.hub.remove_if_current_gen(&key, gen) {
        let _ = group.tx.send(GroupMsg::RemoveMember { member_id }).await;
        deps.sinks.remove(member_id, gen);
        metrics::counter!(
            "pharos_syncplay_member_pruned_total",
            "connected" => "false",
            "cause" => "connection_closed",
        )
        .increment(1);
        tracing::info!(device_id = %key, %member_id, group = %group.group_id, "syncplay: native member removed after its connection stayed gone");
    }
}

fn write_done(out: done::Builder<'_>, result: Result<(), Denial>) {
    match result {
        Ok(()) => {
            let mut out = out;
            out.set_ok(());
        }
        Err(denial) => denial.write(out.init_error()),
    }
}

impl Membership {
    /// Sends the group a command from this member through the routing the
    /// Jellyfin handlers use, so the command metric and persisted-membership
    /// recovery apply to it too.
    async fn command(
        &self,
        label: &str,
        make: impl FnOnce(MemberId) -> GroupMsg,
    ) -> Result<(), Denial> {
        if self.left.get() {
            return Err(Denial::NotFound { what: "group" });
        }
        self.session.current_user().await?;
        // The hub entry is the device's, not this membership's: a later
        // create or join on the device moved it to another group.
        if let Some(sess) = self.deps.hub.resolve(&self.key) {
            if sess
                .group
                .is_some_and(|g| g.group_id != self.group.group_id)
            {
                return Err(Denial::NotFound { what: "membership" });
            }
        }
        let outcome = route_command(
            &self.deps.hub,
            &self.deps.registry,
            &self.session.state.stores,
            Some(&self.key),
            label,
            async |_| true,
            make,
        )
        .await;
        match outcome {
            CommandOutcome::Dispatched | CommandOutcome::Recovered => Ok(()),
            CommandOutcome::NotInGroup | CommandOutcome::ActorDown => {
                Err(Denial::NotFound { what: "group" })
            }
            CommandOutcome::Refused => Err(Denial::NotPermitted { what: "command" }),
            CommandOutcome::NoDevice => Err(Denial::Invalid {
                field: "ctx.device",
            }),
            CommandOutcome::DroppedNoSocket => Err(Denial::Unavailable {
                dependency: "syncplay",
            }),
        }
    }

    /// Each member's state on `ids` (US6), for the group this membership is
    /// in and only while the caller is still in it. `asked` is how many ids
    /// the caller sent, which can exceed `ids.len()`: the handler decodes at
    /// most one past the cap, and the log and the refusal report the real
    /// count.
    async fn seen_for(&self, ids: &[WireId], asked: usize) -> Result<Vec<MemberSeen>, Denial> {
        let user = self.session.current_user().await?;
        let group = self.group.group_id.to_string();
        let record = |outcome: Outcome, answered: usize, members: usize| {
            group_seen::record(outcome, &user.name, &group, asked, answered, members);
        };
        let in_group = !self.left.get()
            && self.deps.hub.resolve(&self.key).map_or(true, |s| {
                s.group
                    .as_ref()
                    .map_or(true, |g| g.group_id == self.group.group_id)
            });
        if !in_group {
            record(Outcome::NotInGroup, 0, 0);
            return Err(Denial::NotFound { what: "group" });
        }
        if asked > group_seen::MAX_IDS || ids.len() > group_seen::MAX_IDS {
            record(Outcome::OverLimit, 0, 0);
            return Err(Denial::Limit {
                name: "seen_ids",
                value: u64::try_from(asked.max(ids.len())).unwrap_or(u64::MAX),
                max: group_seen::MAX_IDS as u64,
            });
        }
        let Some(snapshot) = self.group.snapshot().await else {
            record(Outcome::Unavailable, 0, 0);
            return Err(Denial::Unavailable {
                dependency: "syncplay",
            });
        };
        if !snapshot.participant_ids.contains(&self.member_id) {
            record(Outcome::NotInGroup, 0, 0);
            return Err(Denial::NotFound { what: "group" });
        }
        match group_seen::seen(&self.session.state, &user, &snapshot, ids).await {
            Ok(members) => {
                let answered = members.iter().map(|m| m.items.len()).max().unwrap_or(0);
                record(Outcome::Answered, answered, members.len());
                Ok(members)
            }
            Err(group_seen::SeenError::Unavailable) => {
                record(Outcome::Unavailable, 0, 0);
                Err(Denial::Unavailable {
                    dependency: "media store",
                })
            }
        }
    }

    /// `leave_group`'s work: out of the roster, the leaver told `GroupLeft`
    /// whether or not the server still had it in a group (B25), then out of
    /// the hub. Idempotent.
    async fn leave_group(&self) -> Result<(), Denial> {
        let user = self.session.current_user().await?;
        if self.left.replace(true) {
            return Ok(());
        }
        let hub = &self.deps.hub;
        // A newer registration of this device (a reconnect) owns the hub entry.
        if hub.conn_gen(&self.key) != Some(self.gen) {
            return Ok(());
        }
        end_owned_cast(&self.session.state, user.id, self.member_id).await;
        if let Some(group) = hub.detach_group(&self.key) {
            let _ = group
                .tx
                .send(GroupMsg::RemoveMember {
                    member_id: self.member_id,
                })
                .await;
        }
        if let Some(sess) = hub.resolve(&self.key) {
            let _ = sess.sink.send(ServerMsg::GroupLeft).await;
        }
        hub.remove_if_current_gen(&self.key, self.gen);
        self.deps.sinks.remove(self.member_id, self.gen);
        Ok(())
    }
}

impl server::membership::Handler for Membership {
    async fn unpause(
        self: Rc<Self>,
        _params: member_wire::UnpauseParams,
        results: &mut member_wire::UnpauseResults,
    ) -> Result<(), capnp::Error> {
        let done = self
            .command("unpause", |sender| GroupMsg::Unpause { sender })
            .await;
        write_done(results.get().init_result(), done);
        Ok(())
    }

    async fn pause(
        self: Rc<Self>,
        _params: member_wire::PauseParams,
        results: &mut member_wire::PauseResults,
    ) -> Result<(), capnp::Error> {
        let done = self
            .command("pause", |sender| GroupMsg::PauseShared { sender })
            .await;
        write_done(results.get().init_result(), done);
        Ok(())
    }

    async fn seek(
        self: Rc<Self>,
        params: member_wire::SeekParams,
        results: &mut member_wire::SeekResults,
    ) -> Result<(), capnp::Error> {
        let position_ms = params.get()?.get_position_ms();
        if !self.left.get() {
            prewarm_seek(&self.session.state, self.group.clone(), position_ms);
        }
        let done = self
            .command("seek", |sender| GroupMsg::SeekTo {
                sender,
                position_ms,
            })
            .await;
        write_done(results.get().init_result(), done);
        Ok(())
    }

    async fn buffering(
        self: Rc<Self>,
        params: member_wire::BufferingParams,
        results: &mut member_wire::BufferingResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let position_ms = params.get_position_ms();
        let buffering = params.get_buffering();
        let playlist_item_id = playlist_item_id(params.get_playlist_item_id()?.to_string()?);
        let done = self
            .command("buffering", |member_id| {
                if buffering {
                    GroupMsg::BufferingStart {
                        member_id,
                        position_ms,
                        playlist_item_id,
                    }
                } else {
                    GroupMsg::BufferingEnd { member_id }
                }
            })
            .await;
        write_done(results.get().init_result(), done);
        Ok(())
    }

    async fn ready(
        self: Rc<Self>,
        params: member_wire::ReadyParams,
        results: &mut member_wire::ReadyResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let position_ms = params.get_position_ms();
        let is_playing = params.get_playing();
        let playlist_item_id = playlist_item_id(params.get_playlist_item_id()?.to_string()?);
        let done = self
            .command("ready", |member_id| GroupMsg::MemberReady {
                member_id,
                position_ms,
                playlist_item_id,
                is_playing,
            })
            .await;
        write_done(results.get().init_result(), done);
        Ok(())
    }

    async fn leave(
        self: Rc<Self>,
        _params: member_wire::LeaveParams,
        results: &mut member_wire::LeaveResults,
    ) -> Result<(), capnp::Error> {
        let done = self.leave_group().await;
        write_done(results.get().init_result(), done);
        Ok(())
    }

    async fn ping(
        self: Rc<Self>,
        _params: member_wire::PingParams,
        results: &mut member_wire::PingResults,
    ) -> Result<(), capnp::Error> {
        let done = self
            .command("ping", |member_id| GroupMsg::MemberPing { member_id })
            .await;
        write_done(results.get().init_result(), done);
        Ok(())
    }

    async fn set_tracks(
        self: Rc<Self>,
        params: member_wire::SetTracksParams,
        results: &mut member_wire::SetTracksResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let audio = read_choice(params.get_audio()?);
        let subtitle = read_choice(params.get_subtitle()?);
        let playlist_item_id = playlist_item_id(params.get_playlist_item_id()?.to_string()?);
        let done = self
            .command("set_tracks", |sender| GroupMsg::SetTracks {
                sender,
                audio,
                subtitle,
                playlist_item_id,
            })
            .await;
        write_done(results.get().init_result(), done);
        Ok(())
    }

    async fn follow_tracks(
        self: Rc<Self>,
        params: member_wire::FollowTracksParams,
        results: &mut member_wire::FollowTracksResults,
    ) -> Result<(), capnp::Error> {
        let follow = params.get()?.get_follow();
        let done = self
            .command("follow_tracks", |member_id| GroupMsg::FollowTracks {
                member_id,
                follow,
            })
            .await;
        write_done(results.get().init_result(), done);
        Ok(())
    }

    async fn seen(
        self: Rc<Self>,
        params: member_wire::SeenParams,
        results: &mut member_wire::SeenResults,
    ) -> Result<(), capnp::Error> {
        let wire_ids = params.get()?.get_ids()?;
        // One past the cap is enough for `seen_for` to refuse the call, so an
        // oversized list is never decoded whole.
        let keep = group_seen::MAX_IDS + 1;
        let mut ids = Vec::with_capacity((wire_ids.len() as usize).min(keep));
        for id in wire_ids.iter().take(keep) {
            ids.push(read_item_id(id)?);
        }
        match self.seen_for(&ids, wire_ids.len() as usize).await {
            Ok(members) => write_seen(results.get().init_result(), &members),
            Err(denial) => denial.write(results.get().init_result().init_error()),
        }
        Ok(())
    }

    async fn clock_report(
        self: Rc<Self>,
        params: member_wire::ClockReportParams,
        results: &mut member_wire::ClockReportResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let (t1, t2, t3, t4) = (
            params.get_t1(),
            params.get_t2(),
            params.get_t3(),
            params.get_t4(),
        );
        let done = self
            .command("clockreport", |member_id| GroupMsg::ObserveClock {
                member_id,
                t1,
                t2,
                t3,
                t4,
            })
            .await;
        write_done(results.get().init_result(), done);
        Ok(())
    }
}

/// The queue entry a report is for: empty names none.
fn playlist_item_id(wire: String) -> Option<String> {
    (!wire.is_empty()).then_some(wire)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use pharos_sync::messages::{GroupPlayState, QueueItemInfo};

    use super::*;

    fn gid() -> GroupId {
        GroupId::new()
    }

    /// The actor's monotonic times reach the client as unix ms.
    #[test]
    fn scheduled_times_are_offset_by_the_group_epoch() {
        let at = translate(
            &ServerMsg::Pause {
                by: None,
                at_server_ms: 250,
                position_ms: 9_000,
            },
            gid(),
            1_000_000,
        );
        assert_eq!(
            at,
            Some(Update::Pause {
                at_server_ms: 1_000_250,
                position_ms: 9_000,
                by: None,
            })
        );
    }

    /// V395: the member who caused a play, pause or seek reaches the client
    /// as its id; a change the group made has none.
    #[test]
    fn a_play_pause_or_seek_names_who_caused_it() {
        let who = MemberId::new();
        let named = |msg: ServerMsg| match translate(&msg, gid(), 0) {
            Some(Update::Play { by, .. } | Update::Pause { by, .. } | Update::Seek { by, .. }) => {
                by
            }
            other => panic!("not a transport update: {other:?}"),
        };
        for msg in [
            ServerMsg::Play {
                at_server_ms: 1,
                position_ms: 2,
                by: Some(who),
            },
            ServerMsg::Pause {
                at_server_ms: 1,
                position_ms: 2,
                by: Some(who),
            },
            ServerMsg::Seek {
                at_server_ms: 1,
                position_ms: 2,
                by: Some(who),
            },
        ] {
            assert_eq!(named(msg), Some(who.to_string()));
        }
        assert_eq!(
            named(ServerMsg::Play {
                at_server_ms: 1,
                position_ms: 2,
                by: None
            }),
            None
        );
    }

    /// V395: the catch-up marker reaches the client as the id it skips.
    #[test]
    fn the_catch_up_marker_reaches_the_client_as_the_id_it_skips() {
        let update = translate(
            &ServerMsg::Pause {
                by: Some(MemberId::CATCH_UP),
                at_server_ms: 1,
                position_ms: 2,
            },
            gid(),
            0,
        );
        let Some(Update::Pause { by, .. }) = update else {
            panic!("not a pause: {update:?}")
        };
        assert_eq!(by.as_deref(), Some(pharos_client::group::CATCH_UP_BY));
    }

    #[test]
    fn the_playing_queue_entry_becomes_the_item_update() {
        let msg = ServerMsg::PlayQueue {
            reason: "NewPlaylist".into(),
            items: vec![
                QueueItemInfo {
                    item_id: wire_item_id(7),
                    playlist_item_id: "p0".into(),
                },
                QueueItemInfo {
                    item_id: wire_item_id(9),
                    playlist_item_id: "p1".into(),
                },
            ],
            playing_index: 1,
            start_position_ms: 4_000,
            is_playing: true,
            repeat_mode: "RepeatNone".into(),
            shuffle_mode: "Sorted".into(),
            last_update_unix_ms: 0,
        };
        assert_eq!(
            translate(&msg, gid(), 0),
            Some(Update::Item {
                item: 9,
                playlist_item_id: "p1".into(),
                position_ms: 4_000,
                playing: true
            })
        );
    }

    /// Messages the native protocol has no use for are skipped, not sent as
    /// something else.
    #[test]
    fn clock_and_label_messages_are_not_forwarded() {
        for msg in [
            ServerMsg::Pong {
                client_ms_echo: 1,
                server_ms: 2,
            },
            ServerMsg::StateUpdate {
                state: GroupPlayState::Waiting,
                reason: "x".into(),
            },
            ServerMsg::LeaderChange {
                leader: MemberId::new(),
            },
        ] {
            assert_eq!(translate(&msg, gid(), 0), None, "{msg:?}");
        }
    }

    /// The removal after a lost connection is counted with its cause and
    /// leaves the hub, so `pharos_syncplay_member_pruned_total{cause=
    /// "connection_closed"}` shows it apart from the silence TTL's.
    #[tokio::test]
    async fn a_seat_lost_with_its_connection_is_removed_and_counted() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        use pharos_sync::{GroupRegistry, LocalDelivery, MemberSinks, SessionHub};
        use std::sync::Arc;

        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _metrics = metrics::set_default_local_recorder(&recorder);

        let sinks = MemberSinks::new();
        let deps = SyncDeps {
            hub: SessionHub::new(),
            registry: GroupRegistry::spawn(Arc::new(LocalDelivery::new(sinks.clone()))),
            sinks,
            socket_loss_grace: std::time::Duration::from_millis(10),
        };
        let handle = deps.registry.create().await.unwrap();
        let (tx, _rx) = mpsc::channel(4);
        let key = "user:dev".to_string();
        let reg = deps.hub.register(key.clone(), "n".into(), tx);
        deps.hub.attach_group(&key, handle.clone());

        lose_seat(
            deps.clone(),
            key.clone(),
            reg.member_id,
            reg.gen,
            handle.tx.clone(),
        )
        .await;

        assert_eq!(deps.hub.conn_gen(&key), None, "out of the hub");
        let count = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .find_map(|(ck, _, _, v)| {
                let labelled = ck.key().name() == "pharos_syncplay_member_pruned_total"
                    && ck
                        .key()
                        .labels()
                        .any(|l| l.key() == "cause" && l.value() == "connection_closed");
                match (labelled, v) {
                    (true, DebugValue::Counter(n)) => Some(n),
                    _ => None,
                }
            });
        assert_eq!(count, Some(1));
    }

    #[test]
    fn leaving_and_losing_the_group_end_it_for_the_member() {
        assert_eq!(
            translate(&ServerMsg::GroupLeft, gid(), 0),
            Some(Update::Ended(EndReason::Left))
        );
        assert_eq!(
            translate(&ServerMsg::NotInGroup, gid(), 0),
            Some(Update::Ended(EndReason::NotInGroup))
        );
    }
}
