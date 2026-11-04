//! Spec 014: a native client listing, joining and driving the SyncPlay groups
//! jellyfin-web uses. The groups, access rules and queue checks are the
//! Jellyfin handlers' own, so a stand-in Jellyfin member here is a hub-
//! registered sink exactly as `/socket` makes one.

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use pharos_api::pharos_capnp::{
    clock_result, error, group_sink, group_update, groups_result, membership, membership_result,
    seen_result, session, EndReason,
};
use pharos_api::{server, Connection};
use pharos_core::{UserPolicy, UserStore as _};
use pharos_jellyfin_api::dto::{wire_item_id, WireId};
use pharos_server::group_ops::SyncDeps;
use pharos_sync::group::{GroupHandle, GroupMsg};
use pharos_sync::messages::ServerMsg;
use pharos_sync::{GroupRegistry, LocalDelivery, MemberSinks, SessionHub};
use tokio::sync::{mpsc, oneshot};

use crate::common::Fixture;
use crate::playback::{seed_playable, MOVIE};

pub(crate) struct Rig {
    pub(crate) server: actix_test::TestServer,
    pub(crate) fx: Fixture,
    pub(crate) deps: SyncDeps,
}

pub(crate) async fn rig() -> Rig {
    let (server, fx) = crate::revocation::start().await;
    seed_playable(&fx).await;
    let sinks = MemberSinks::new();
    let deps = SyncDeps {
        hub: SessionHub::new(),
        registry: GroupRegistry::spawn(Arc::new(LocalDelivery::new(sinks.clone()))),
        sinks,
        socket_loss_grace: Duration::from_millis(300),
    };
    fx.state.sync.install(deps.clone());
    Rig { server, fx, deps }
}

impl Rig {
    pub(crate) async fn session(&self, token: &str) -> session::Client {
        let (client, _closed) = crate::revocation::connect(&self.server).await;
        crate::revocation::signed_in(&client, token).await
    }

    /// A group a Jellyfin client made: one member whose socket is a hub-
    /// registered sink, a name, and `MOVIE` queued.
    async fn jellyfin_group(&self, name: &str) -> (GroupHandle, mpsc::Receiver<ServerMsg>) {
        let handle = self.deps.registry.create().await.unwrap();
        let rx = self.jellyfin_member(&handle, "jelly").await;
        handle
            .tx
            .send(GroupMsg::SetGroupName { name: name.into() })
            .await
            .unwrap();
        handle
            .tx
            .send(GroupMsg::SeedQueue {
                item_id: wire_item_id(MOVIE),
                runtime_ms: Some(60_000),
                position_ms: 0,
                is_paused: true,
            })
            .await
            .unwrap();
        (handle, rx)
    }

    async fn jellyfin_member(&self, handle: &GroupHandle, name: &str) -> mpsc::Receiver<ServerMsg> {
        let key = format!("{}:dev-{name}", self.fx.admin_id.0.simple());
        let (tx, rx) = mpsc::channel(256);
        let reg = self.deps.hub.register(key.clone(), name.into(), tx.clone());
        self.deps.sinks.insert(reg.member_id, reg.gen, tx);
        self.deps.hub.attach_group(&key, handle.clone());
        let (reply, joined) = oneshot::channel();
        handle
            .tx
            .send(GroupMsg::AddMember {
                member_id: reg.member_id,
                name: name.into(),
                user: Some(self.fx.admin_id),
                reply,
            })
            .await
            .unwrap();
        joined.await.unwrap();
        rx
    }
}

fn denial(e: error::Reader<'_>) -> String {
    let text = |t: capnp::Result<capnp::text::Reader<'_>>| t.unwrap().to_str().unwrap().to_owned();
    match e.get_kind().which().unwrap() {
        error::kind::NotFound(what) => format!("not found: {}", text(what)),
        error::kind::NotPermitted(what) => format!("not permitted: {}", text(what)),
        error::kind::Unavailable(what) => format!("unavailable: {}", text(what)),
        error::kind::Invalid(i) => format!("invalid: {}", text(i.unwrap().get_field())),
        error::kind::Revoked(()) => "revoked".to_owned(),
        error::kind::Limit(l) => format!("limit: {}", text(l.unwrap().get_name())),
        _ => "other".to_owned(),
    }
}

/// A listed group as the wire carries it.
#[derive(Debug, PartialEq)]
struct Listed {
    id: String,
    name: String,
    item: Option<u64>,
    members: Vec<String>,
    member_ids: Vec<String>,
}

async fn list(session: &session::Client) -> Result<Vec<Listed>, String> {
    let group = session.group_request().send().pipeline.get_group();
    let mut req = group.list_request();
    req.get().init_ctx();
    let reply = req.send().promise.await.expect("list");
    match reply.get().unwrap().get_result().unwrap().which().unwrap() {
        groups_result::Groups(groups) => Ok(groups
            .unwrap()
            .iter()
            .map(|g| Listed {
                id: g
                    .get_id()
                    .unwrap()
                    .get_value()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .into(),
                name: g.get_name().unwrap().to_str().unwrap().into(),
                item: match g.get_item().unwrap().which().unwrap() {
                    pharos_api::pharos_capnp::item_id::Real(id) => Some(id),
                    pharos_api::pharos_capnp::item_id::Synth(_) => None,
                },
                members: g
                    .get_members()
                    .unwrap()
                    .iter()
                    .map(|m| m.get_name().unwrap().to_str().unwrap().into())
                    .collect(),
                member_ids: g
                    .get_members()
                    .unwrap()
                    .iter()
                    .map(|m| m.get_id().unwrap().to_str().unwrap().into())
                    .collect(),
            })
            .collect()),
        groups_result::Error(e) => Err(denial(e.unwrap())),
    }
}

/// US1: a group a Jellyfin client made shows in the native listing with its
/// name, what it plays and who is in it.
/// Disarm check: with `list` returning no groups, this fails.
#[actix_web::test]
async fn list_shows_a_group_a_jellyfin_client_made() {
    let rig = rig().await;
    let (handle, _rx) = rig.jellyfin_group("Movie night").await;
    let session = rig.session(&rig.fx.user_token).await;
    let groups = list(&session).await.unwrap();
    assert_eq!(
        groups,
        [Listed {
            id: handle.group_id.to_string(),
            name: "Movie night".into(),
            item: Some(MOVIE),
            members: vec!["jelly".into()],
            member_ids: vec![pharos_sync::hub::member_id_for_device(&format!(
                "{}:dev-jelly",
                rig.fx.admin_id.0.simple()
            ))
            .to_string()],
        }]
    );
}

#[actix_web::test]
async fn clock_answers_the_server_time_around_the_call() {
    let rig = rig().await;
    let session = rig.session(&rig.fx.user_token).await;
    let group = session.group_request().send().pipeline.get_group();
    let mut req = group.clock_request();
    req.get().init_ctx();
    req.get().set_client_ms(1234);
    let before = unix_ms();
    let reply = req.send().promise.await.expect("clock");
    let after = unix_ms();
    match reply.get().unwrap().get_result().unwrap().which().unwrap() {
        clock_result::Sample(s) => {
            let s = s.unwrap();
            assert_eq!(s.get_client_ms_echo(), 1234);
            assert!(before <= s.get_recv_ms() && s.get_recv_ms() <= s.get_send_ms());
            assert!(s.get_send_ms() <= after);
        }
        clock_result::Error(e) => panic!("clock refused: {}", denial(e.unwrap())),
    }
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// V201: a user whose `SyncPlayAccess` is `None` is refused the listing, as
/// the Jellyfin route answers 403.
/// Disarm check: with `list` skipping `sync_play_allows`, this fails.
#[actix_web::test]
async fn list_is_refused_without_syncplay_access() {
    let rig = rig().await;
    let (_handle, _rx) = rig.jellyfin_group("Movie night").await;
    let session = rig.session(&rig.fx.user_token).await;
    rig.fx
        .state
        .stores
        .set_policy(
            rig.fx.user_id,
            UserPolicy {
                sync_play_access: "None".into(),
                ..UserPolicy::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(list(&session).await.unwrap_err(), "not permitted: syncplay");
}

/// What a native member's sink was told, as plain values.
#[derive(Debug, Clone, PartialEq)]
enum Seen {
    Joined {
        name: String,
        item: Option<u64>,
        members: Vec<String>,
        member_ids: Vec<String>,
        buffering: Vec<String>,
        /// Names of the members on their own tracks.
        own_tracks: Vec<String>,
        /// The receiving member's own id.
        you: String,
        /// The group's audio, subtitle and who set them.
        tracks: (String, String, String),
    },
    Play {
        at: u64,
        position_ms: u64,
        by: String,
    },
    Pause {
        at: u64,
        position_ms: u64,
        by: String,
    },
    Seek {
        at: u64,
        position_ms: u64,
        by: String,
    },
    Member {
        name: String,
        buffering: bool,
        own_tracks: bool,
    },
    Left(String),
    Item {
        item: u64,
        playing: bool,
        position_ms: u64,
    },
    Ended(&'static str),
    Tracks {
        audio: String,
        subtitle: String,
        by: String,
    },
}

/// A track choice as "unset", "off" or the stream index.
fn choice(c: pharos_api::pharos_capnp::track_choice::Reader<'_>) -> String {
    match (c.get_set(), c.get_off()) {
        (false, _) => "unset".into(),
        (true, true) => "off".into(),
        (true, false) => c.get_index().to_string(),
    }
}

struct TestSink(mpsc::UnboundedSender<Seen>);

impl server::group_sink::Handler for TestSink {
    async fn update(
        self: Rc<Self>,
        params: group_sink::UpdateParams,
        results: &mut group_sink::UpdateResults,
    ) -> Result<(), capnp::Error> {
        let update = params.get()?.get_update()?;
        let text =
            |t: capnp::Result<capnp::text::Reader<'_>>| t.unwrap().to_str().unwrap().to_owned();
        let seen = match update.which()? {
            group_update::Joined(g) => {
                let g = g?;
                Seen::Joined {
                    name: text(g.get_name()),
                    item: match g.get_item()?.which()? {
                        pharos_api::pharos_capnp::item_id::Real(id) if id != 0 => Some(id),
                        _ => None,
                    },
                    members: g
                        .get_members()?
                        .iter()
                        .map(|m| text(m.get_name()))
                        .collect(),
                    member_ids: g.get_members()?.iter().map(|m| text(m.get_id())).collect(),
                    you: text(g.get_you()),
                    buffering: g
                        .get_members()?
                        .iter()
                        .filter(|m| m.get_buffering())
                        .map(|m| text(m.get_name()))
                        .collect(),
                    own_tracks: g
                        .get_members()?
                        .iter()
                        .filter(|m| m.get_own_tracks())
                        .map(|m| text(m.get_name()))
                        .collect(),
                    tracks: {
                        let t = g.get_tracks()?;
                        (
                            choice(t.get_audio()?),
                            choice(t.get_subtitle()?),
                            text(t.get_by()),
                        )
                    },
                }
            }
            group_update::Play(a) => {
                let a = a?;
                Seen::Play {
                    at: a.get_at_server_ms(),
                    position_ms: a.get_position_ms(),
                    by: text(a.get_by()),
                }
            }
            group_update::Pause(a) => {
                let a = a?;
                Seen::Pause {
                    at: a.get_at_server_ms(),
                    position_ms: a.get_position_ms(),
                    by: text(a.get_by()),
                }
            }
            group_update::Seek(a) => {
                let a = a?;
                Seen::Seek {
                    at: a.get_at_server_ms(),
                    position_ms: a.get_position_ms(),
                    by: text(a.get_by()),
                }
            }
            group_update::Member(m) => {
                let m = m?;
                Seen::Member {
                    name: text(m.get_name()),
                    buffering: m.get_buffering(),
                    own_tracks: m.get_own_tracks(),
                }
            }
            group_update::Left(id) => Seen::Left(text(id)),
            group_update::Item(i) => {
                let i = i?;
                let pharos_api::pharos_capnp::item_id::Real(item) = i.get_item()?.which()? else {
                    panic!("synthetic item in a group");
                };
                Seen::Item {
                    item,
                    playing: i.get_playing(),
                    position_ms: i.get_position_ms(),
                }
            }
            group_update::Ended(reason) => Seen::Ended(match reason? {
                EndReason::Left => "left",
                EndReason::NotInGroup => "notInGroup",
            }),
            group_update::Tracks(t) => {
                let t = t?;
                Seen::Tracks {
                    audio: choice(t.get_audio()?),
                    subtitle: choice(t.get_subtitle()?),
                    by: text(t.get_by()),
                }
            }
        };
        let _ = self.0.send(seen);
        let _ = results;
        Ok(())
    }
}

fn sink() -> (group_sink::Client, mpsc::UnboundedReceiver<Seen>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (
        server::group_sink::client(TestSink(tx), &Connection::new()),
        rx,
    )
}

/// The next update on `rx`, or a panic after a second.
async fn next(rx: &mut mpsc::UnboundedReceiver<Seen>) -> Seen {
    tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("an update within a second")
        .expect("sink open")
}

/// Updates until `want` returns Some, skipping those it does not want.
async fn until<T>(rx: &mut mpsc::UnboundedReceiver<Seen>, want: impl Fn(&Seen) -> Option<T>) -> T {
    loop {
        let seen = next(rx).await;
        if let Some(t) = want(&seen) {
            return t;
        }
    }
}

/// Lets the updates still in flight from setting a group up arrive and drops
/// them, so the next one is a reaction to what the test does next.
async fn settle(rx: &mut mpsc::UnboundedReceiver<Seen>) {
    tokio::time::sleep(Duration::from_millis(500)).await;
    while rx.try_recv().is_ok() {}
}

/// Like [`until`], for an update that waits on the pump's poll.
async fn until_polled<T>(
    rx: &mut mpsc::UnboundedReceiver<Seen>,
    want: impl Fn(&Seen) -> Option<T>,
) -> T {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let seen = rx.recv().await.expect("sink open");
            if let Some(t) = want(&seen) {
                return t;
            }
        }
    })
    .await
    .expect("an update within 3 s")
}

fn set_device(session_ctx: pharos_api::pharos_capnp::call_context::Builder<'_>, device: &str) {
    let mut ctx = session_ctx;
    ctx.reborrow().init_device().set_value(device);
}

async fn create(
    session: &session::Client,
    device: &str,
    item: u64,
    position_ms: u64,
    sink: group_sink::Client,
) -> Result<membership::Client, String> {
    let group = session.group_request().send().pipeline.get_group();
    let mut req = group.create_request();
    {
        let mut p = req.get();
        set_device(p.reborrow().init_ctx(), device);
        p.reborrow().init_item().set_real(item);
        p.set_position_ms(position_ms);
        p.set_sink(sink);
    }
    read_membership(
        req.send()
            .promise
            .await
            .expect("create")
            .get()
            .unwrap()
            .get_result()
            .unwrap(),
    )
}

async fn join(
    session: &session::Client,
    device: &str,
    id: &str,
    sink: group_sink::Client,
) -> Result<membership::Client, String> {
    let group = session.group_request().send().pipeline.get_group();
    let mut req = group.join_request();
    {
        let mut p = req.get();
        set_device(p.reborrow().init_ctx(), device);
        p.reborrow().init_id().set_value(id);
        p.set_sink(sink);
    }
    read_membership(
        req.send()
            .promise
            .await
            .expect("join")
            .get()
            .unwrap()
            .get_result()
            .unwrap(),
    )
}

fn read_membership(result: membership_result::Reader<'_>) -> Result<membership::Client, String> {
    match result.get_status().unwrap().which().unwrap() {
        pharos_api::pharos_capnp::done::Ok(()) => Ok(result.get_membership().unwrap()),
        pharos_api::pharos_capnp::done::Error(e) => Err(denial(e.unwrap())),
    }
}

/// The id of the one group in the registry.
async fn only_group(rig: &Rig) -> String {
    let groups = rig.deps.registry.list().await.unwrap();
    assert_eq!(groups.len(), 1, "exactly one group");
    groups[0].group_id.to_string()
}

/// US1: native A creates a group on an item, native B joins it and is told
/// who is in it, what it plays and its name.
/// Disarm check: with `join` registering the device but never calling
/// `add_caller_to_group`, B's sink never sees `joined` and this times out.
#[actix_web::test]
async fn a_native_member_joins_a_native_group_and_sees_who_is_in_it() {
    let rig = rig().await;
    let a = rig.session(&rig.fx.admin_token).await;
    let b = rig.session(&rig.fx.user_token).await;
    let (a_sink, mut a_seen) = sink();
    let _a_member = create(&a, "dev-a", MOVIE, 5_000, a_sink).await.unwrap();
    let gid = only_group(&rig).await;
    let (b_sink, mut b_seen) = sink();
    let _b_member = join(&b, "dev-b", &gid, b_sink).await.unwrap();

    let joined = until(&mut b_seen, |s| {
        matches!(s, Seen::Joined { .. }).then(|| s.clone())
    })
    .await;
    let Seen::Joined {
        name,
        item,
        members,
        ..
    } = joined
    else {
        unreachable!()
    };
    assert_eq!(name, "ali's Group");
    assert_eq!(item, Some(MOVIE));
    assert!(members.contains(&"ali".to_owned()), "{members:?}");
    // A is told B arrived.
    let arrived = until(&mut a_seen, |s| match s {
        Seen::Member { name, .. } => Some(name.clone()),
        _ => None,
    })
    .await;
    assert_eq!(arrived, "guest");
}

/// A listed member carries the same id the group's updates name it by, so a
/// client can match the two.
/// Disarm check: with the listing leaving the ids empty, the sets differ.
#[actix_web::test]
async fn listed_member_ids_are_the_ids_updates_use() {
    let rig = rig().await;
    let a = rig.session(&rig.fx.admin_token).await;
    let b = rig.session(&rig.fx.user_token).await;
    let (a_sink, _a_seen) = sink();
    let _a_member = create(&a, "dev-a", MOVIE, 0, a_sink).await.unwrap();
    let gid = only_group(&rig).await;
    let (b_sink, mut b_seen) = sink();
    let _b_member = join(&b, "dev-b", &gid, b_sink).await.unwrap();
    let Seen::Joined { mut member_ids, .. } = until(&mut b_seen, |s| {
        matches!(s, Seen::Joined { .. }).then(|| s.clone())
    })
    .await
    else {
        unreachable!()
    };
    let mut listed = list(&b).await.unwrap().remove(0).member_ids;
    member_ids.sort();
    listed.sort();
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().all(|id| !id.is_empty()));
    assert_eq!(listed, member_ids);
}

/// `joined` names the receiving member, so a client can tell which roster
/// entry is itself.
#[actix_web::test]
async fn joined_names_the_member_it_is_sent_to() {
    let rig = rig().await;
    let a = rig.session(&rig.fx.admin_token).await;
    let b = rig.session(&rig.fx.user_token).await;
    let (a_sink, mut a_seen) = sink();
    let _a_member = create(&a, "dev-a", MOVIE, 0, a_sink).await.unwrap();
    let gid = only_group(&rig).await;
    let (b_sink, mut b_seen) = sink();
    let _b_member = join(&b, "dev-b", &gid, b_sink).await.unwrap();
    let mut you = Vec::new();
    for seen in [&mut a_seen, &mut b_seen] {
        let Seen::Joined {
            you: me,
            member_ids,
            ..
        } = until(seen, |s| {
            matches!(s, Seen::Joined { .. }).then(|| s.clone())
        })
        .await
        else {
            unreachable!()
        };
        assert!(member_ids.contains(&me), "{me} is in {member_ids:?}");
        you.push(me);
    }
    assert_ne!(you[0], you[1]);
}

/// Joining a group that does not exist answers notFound and creates nothing
/// (spec edge case): the registry still holds no group afterwards.
/// Disarm check: with `join` using `get_or_create`, a group appears and this
/// fails.
#[actix_web::test]
async fn joining_a_missing_group_is_not_found_and_creates_nothing() {
    let rig = rig().await;
    let b = rig.session(&rig.fx.user_token).await;
    let (b_sink, _seen) = sink();
    let missing = pharos_sync::messages::GroupId::new().to_string();
    let err = join(&b, "dev-b", &missing, b_sink).await.err().unwrap();
    assert_eq!(err, "not found: group");
    assert!(rig.deps.registry.list().await.unwrap().is_empty());
}

#[actix_web::test]
async fn a_call_with_no_device_is_invalid() {
    let rig = rig().await;
    let a = rig.session(&rig.fx.admin_token).await;
    let (a_sink, _seen) = sink();
    let err = create(&a, "", MOVIE, 0, a_sink).await.err().unwrap();
    assert_eq!(err, "invalid: ctx.device");
    assert!(rig.deps.registry.list().await.unwrap().is_empty());
}

/// One `Membership` command: `Ok` or the denial it answered.
macro_rules! command {
    ($member:expr, $method:ident $(, $setter:ident($value:expr))* $(,)?) => {{
        let mut req = $member.$method();
        {
            let mut p = req.get();
            set_device(p.reborrow().init_ctx(), "dev");
            $(p.$setter($value);)*
        }
        let reply = req.send().promise.await.expect("call");
        match reply.get().unwrap().get_result().unwrap().which().unwrap() {
            pharos_api::pharos_capnp::done::Ok(()) => Ok(()),
            pharos_api::pharos_capnp::done::Error(e) => Err(denial(e.unwrap())),
        }
    }};
}

/// A group of two native members, past its readiness gate and playing.
/// Returns the members and their sinks' receivers.
struct Pair {
    a: membership::Client,
    b: membership::Client,
    a_seen: mpsc::UnboundedReceiver<Seen>,
    b_seen: mpsc::UnboundedReceiver<Seen>,
}

async fn playing_pair(rig: &Rig) -> Pair {
    let a_session = rig.session(&rig.fx.admin_token).await;
    let b_session = rig.session(&rig.fx.user_token).await;
    let (a_sink, mut a_seen) = sink();
    let a = create(&a_session, "dev-a", MOVIE, 5_000, a_sink)
        .await
        .unwrap();
    let gid = only_group(rig).await;
    let (b_sink, mut b_seen) = sink();
    let b = join(&b_session, "dev-b", &gid, b_sink).await.unwrap();
    // Both ack the queue entry, which opens the readiness gate.
    for (member, seen) in [(&a, &mut a_seen), (&b, &mut b_seen)] {
        let _ = until(seen, |s| matches!(s, Seen::Item { .. }).then_some(())).await;
        command!(
            member,
            ready_request,
            set_position_ms(5_000),
            set_playing(false)
        )
        .unwrap();
    }
    Pair {
        a,
        b,
        a_seen,
        b_seen,
    }
}

/// US1: A unpauses and both hear it; A pauses at a position and B's sink
/// is told, with the time the group scheduled it for.
/// Disarm check: with `pause` answering ok without calling `route_command`,
/// B's sink never sees `pause` and this times out.
#[actix_web::test]
async fn a_pause_by_one_native_member_reaches_the_other() {
    let rig = rig().await;
    let mut pair = playing_pair(&rig).await;
    command!(pair.a, unpause_request).unwrap();
    let played = until(&mut pair.b_seen, |s| {
        matches!(s, Seen::Play { .. }).then(|| s.clone())
    })
    .await;
    assert!(
        matches!(
            played,
            Seen::Play {
                position_ms: 5_000..,
                ..
            }
        ),
        "{played:?}"
    );
    command!(pair.a, pause_request).unwrap();
    let Seen::Pause { at, .. } = until(&mut pair.b_seen, |s| {
        matches!(s, Seen::Pause { .. }).then(|| s.clone())
    })
    .await
    else {
        unreachable!()
    };
    assert!(
        at.abs_diff(unix_ms()) < 60_000,
        "scheduled in unix ms: {at}"
    );
    assert!(matches!(
        next_pause(&mut pair.a_seen).await,
        Seen::Pause { .. }
    ));
}

/// V395: a group play, pause and seek name the member whose command caused
/// them, as the id the roster lists.
/// Disarm check: with `translate` writing no `by`, the ids come back empty.
#[actix_web::test]
async fn a_group_play_pause_and_seek_name_the_member_who_caused_them() {
    let rig = rig().await;
    let mut pair = playing_pair(&rig).await;
    let a_id =
        pharos_sync::hub::member_id_for_device(&format!("{}:dev-a", rig.fx.admin_id.0.simple()))
            .to_string();
    let next_play = async |seen: &mut mpsc::UnboundedReceiver<Seen>| {
        let Seen::Play { by, .. } =
            until(seen, |s| matches!(s, Seen::Play { .. }).then(|| s.clone())).await
        else {
            unreachable!()
        };
        by
    };
    // The start the readiness gate made once both had acked is the group's.
    command!(pair.a, unpause_request).unwrap();
    assert_eq!(next_play(&mut pair.b_seen).await, "", "gate start");
    command!(pair.a, pause_request).unwrap();
    let Seen::Pause { by, .. } = next_pause(&mut pair.b_seen).await else {
        unreachable!()
    };
    assert_eq!(by, a_id, "pause");
    command!(pair.a, unpause_request).unwrap();
    assert_eq!(next_play(&mut pair.b_seen).await, a_id, "play");
    command!(pair.a, seek_request, set_position_ms(9_000)).unwrap();
    let Seen::Seek { by, .. } = until(&mut pair.b_seen, |s| {
        matches!(s, Seen::Seek { .. }).then(|| s.clone())
    })
    .await
    else {
        unreachable!()
    };
    assert_eq!(by, a_id, "seek");
}

/// `Membership.setTracks` with an audio and a subtitle choice, each one of
/// "unset", "off" or a stream index.
async fn set_tracks(
    member: &membership::Client,
    audio: &str,
    subtitle: &str,
) -> Result<(), String> {
    set_tracks_on(member, audio, subtitle, "").await
}

/// [`set_tracks`] made on the queue entry `playlist_item_id` (empty: any).
async fn set_tracks_on(
    member: &membership::Client,
    audio: &str,
    subtitle: &str,
    playlist_item_id: &str,
) -> Result<(), String> {
    fn fill(mut out: pharos_api::pharos_capnp::track_choice::Builder<'_>, choice: &str) {
        match choice {
            "unset" => {}
            "off" => {
                out.set_set(true);
                out.set_off(true);
            }
            index => {
                out.set_set(true);
                out.set_index(index.parse().unwrap());
            }
        }
    }
    let mut req = member.set_tracks_request();
    {
        let mut p = req.get();
        set_device(p.reborrow().init_ctx(), "dev");
        fill(p.reborrow().init_audio(), audio);
        fill(p.reborrow().init_subtitle(), subtitle);
        p.set_playlist_item_id(playlist_item_id);
    }
    let reply = req.send().promise.await.expect("call");
    match reply.get().unwrap().get_result().unwrap().which().unwrap() {
        pharos_api::pharos_capnp::done::Ok(()) => Ok(()),
        pharos_api::pharos_capnp::done::Error(e) => Err(denial(e.unwrap())),
    }
}

/// V396: one member's audio and subtitle pick reaches the others naming who
/// made it, and a late joiner starts on it in its `joined`.
/// Disarm check: with `translate` dropping `ServerMsg::Tracks`, B's sink never
/// sees it and this times out.
#[actix_web::test]
async fn a_tracks_pick_reaches_the_group_and_a_late_joiner() {
    let rig = rig().await;
    let mut pair = playing_pair(&rig).await;
    let a_id =
        pharos_sync::hub::member_id_for_device(&format!("{}:dev-a", rig.fx.admin_id.0.simple()))
            .to_string();
    set_tracks(&pair.a, "2", "off").await.unwrap();
    let tracks = until(&mut pair.b_seen, |s| match s {
        Seen::Tracks {
            audio,
            subtitle,
            by,
        } => Some((audio.clone(), subtitle.clone(), by.clone())),
        _ => None,
    })
    .await;
    assert_eq!(tracks, ("2".into(), "off".into(), a_id.clone()));

    // The other half is left as it was.
    set_tracks(&pair.a, "unset", "5").await.unwrap();
    let tracks = until(&mut pair.b_seen, |s| match s {
        Seen::Tracks {
            audio,
            subtitle,
            by,
        } => Some((audio.clone(), subtitle.clone(), by.clone())),
        _ => None,
    })
    .await;
    assert_eq!(tracks, ("2".into(), "5".into(), a_id.clone()));

    let gid = only_group(&rig).await;
    let c_session = rig.session(&rig.fx.user_token).await;
    let (c_sink, mut c_seen) = sink();
    let _c = join(&c_session, "dev-c", &gid, c_sink).await.unwrap();
    let Seen::Joined { tracks, .. } = until(&mut c_seen, |s| {
        matches!(s, Seen::Joined { .. }).then(|| s.clone())
    })
    .await
    else {
        unreachable!()
    };
    assert_eq!(tracks, ("2".into(), "5".into(), a_id));
}

/// V396: a pick made on a queue entry the group has left is answered ok and
/// dropped, so the next valid pick is the first the group hears.
/// Disarm check: with the stale check removed, B hears the stale pick first.
#[actix_web::test]
async fn a_tracks_pick_from_another_queue_entry_is_dropped() {
    let rig = rig().await;
    let mut pair = playing_pair(&rig).await;
    set_tracks_on(&pair.a, "9", "unset", "not-the-playing-entry")
        .await
        .unwrap();
    set_tracks(&pair.a, "3", "unset").await.unwrap();
    let audio = until(&mut pair.b_seen, |s| match s {
        Seen::Tracks { audio, .. } => Some(audio.clone()),
        _ => None,
    })
    .await;
    assert_eq!(audio, "3");
}

/// V396: "Just me" is announced to the group as a member update, shows in the
/// roster a later joiner gets, and "Follow group" clears it.
/// Disarm check: with `followTracks` answering ok without sending the
/// `FollowTracks` message, A never sees the update and this times out.
#[actix_web::test]
async fn a_member_on_their_own_tracks_is_marked_in_the_roster() {
    let rig = rig().await;
    let mut pair = playing_pair(&rig).await;
    command!(pair.b, follow_tracks_request, set_follow(false)).unwrap();
    let marked = until(&mut pair.a_seen, |s| match s {
        Seen::Member {
            name,
            own_tracks: true,
            ..
        } => Some(name.clone()),
        _ => None,
    })
    .await;
    assert_eq!(marked, "guest");

    let gid = only_group(&rig).await;
    let c_session = rig.session(&rig.fx.user_token).await;
    let (c_sink, mut c_seen) = sink();
    let _c = join(&c_session, "dev-c", &gid, c_sink).await.unwrap();
    let Seen::Joined { own_tracks, .. } = until(&mut c_seen, |s| {
        matches!(s, Seen::Joined { .. }).then(|| s.clone())
    })
    .await
    else {
        unreachable!()
    };
    assert_eq!(own_tracks, ["guest"]);

    command!(pair.b, follow_tracks_request, set_follow(true)).unwrap();
    let cleared = until(&mut pair.a_seen, |s| match s {
        Seen::Member {
            name,
            own_tracks: false,
            ..
        } if name == "guest" => Some(()),
        _ => None,
    })
    .await;
    assert_eq!(cleared, ());
}

/// A member that has left cannot change the group's tracks.
#[actix_web::test]
async fn a_member_who_left_cannot_set_tracks() {
    let rig = rig().await;
    let pair = playing_pair(&rig).await;
    command!(pair.a, leave_request).unwrap();
    assert_eq!(
        set_tracks(&pair.a, "1", "unset").await.unwrap_err(),
        "not found: group"
    );
}

async fn next_pause(rx: &mut mpsc::UnboundedReceiver<Seen>) -> Seen {
    until(rx, |s| matches!(s, Seen::Pause { .. }).then(|| s.clone())).await
}

/// V20: native and Jellyfin members share one group. A Jellyfin client's
/// group is joined by a native one; a native pause is delivered to the
/// Jellyfin member's socket sink as a `Pause`, and a Jellyfin pause (the
/// message `/SyncPlay/Pause` sends) reaches the native member's sink.
/// Disarm check: with `join` registering a private registry instead of the
/// shared one, the Jellyfin member never sees the pause and this times out.
#[actix_web::test]
async fn native_and_jellyfin_members_command_each_other() {
    let rig = rig().await;
    let (handle, mut jelly_rx) = rig.jellyfin_group("Movie night").await;
    let jelly = pharos_sync::hub::member_id_for_device(&format!(
        "{}:dev-jelly",
        rig.fx.admin_id.0.simple()
    ));
    let session = rig.session(&rig.fx.user_token).await;
    let (sink, mut seen) = sink();
    let native = join(&session, "dev-a", &handle.group_id.to_string(), sink)
        .await
        .unwrap();
    let _ = until(&mut seen, |s| {
        matches!(s, Seen::Joined { .. }).then_some(())
    })
    .await;

    command!(native, unpause_request).unwrap();
    command!(native, pause_request).unwrap();
    let paused = loop {
        let msg = tokio::time::timeout(Duration::from_secs(1), jelly_rx.recv())
            .await
            .expect("the Jellyfin member hears the pause")
            .unwrap();
        if let ServerMsg::Pause { .. } = msg {
            break msg;
        }
    };
    assert!(matches!(paused, ServerMsg::Pause { .. }));

    handle
        .tx
        .send(GroupMsg::PauseShared { sender: jelly })
        .await
        .unwrap();
    let _ = until(&mut seen, |s| matches!(s, Seen::Pause { .. }).then_some(())).await;
}

/// `leave` ends the membership: the leaver's sink is told `ended(left)`, the
/// group's other member is told it left, and the leaver's later commands are
/// refused.
/// Disarm check: with `leave_group` skipping the `GroupLeft` send, the sink
/// never sees `ended` and this times out.
#[actix_web::test]
async fn leave_ends_the_membership_and_tells_the_group() {
    let rig = rig().await;
    let mut pair = playing_pair(&rig).await;
    command!(pair.b, leave_request).unwrap();
    let ended = until(&mut pair.b_seen, |s| {
        matches!(s, Seen::Ended(_)).then(|| s.clone())
    })
    .await;
    assert_eq!(ended, Seen::Ended("left"));
    let gone = until(&mut pair.a_seen, |s| match s {
        Seen::Left(id) => Some(id.clone()),
        _ => None,
    })
    .await;
    assert!(!gone.is_empty());
    assert_eq!(
        command!(pair.b, pause_request).unwrap_err(),
        "not found: group"
    );
    // A second leave is a no-op.
    command!(pair.b, leave_request).unwrap();
}

/// V201: a user restricted away from the library an item is in neither sees
/// a group playing it, nor joins it, nor can start one on it. The refused
/// create leaves no group behind.
/// Disarm check: with `listed_groups` or `may_join` skipping the queue check
/// for the native route, the restricted user lists and joins the group.
#[actix_web::test]
async fn a_restricted_user_neither_sees_nor_joins_nor_starts_a_group_on_a_hidden_item() {
    let rig = rig().await;
    let (handle, _rx) = rig.jellyfin_group("Movie night").await;
    let session = rig.session(&rig.fx.user_token).await;
    rig.fx
        .state
        .stores
        .set_policy(
            rig.fx.user_id,
            UserPolicy {
                enable_all_folders: false,
                enabled_folders: vec![rig.fx.lib_b_wire.clone()],
                ..UserPolicy::default()
            },
        )
        .await
        .unwrap();
    assert!(list(&session).await.unwrap().is_empty());
    let (sink_b, _seen) = sink();
    let err = join(&session, "dev-b", &handle.group_id.to_string(), sink_b)
        .await
        .err()
        .unwrap();
    assert_eq!(err, "not permitted: group");
    let (sink_c, _seen) = sink();
    let err = create(&session, "dev-b", MOVIE, 0, sink_c)
        .await
        .err()
        .unwrap();
    assert_eq!(err, "not permitted: queueing");
    assert_eq!(
        rig.deps.registry.list().await.unwrap().len(),
        1,
        "only the Jellyfin group exists"
    );
}

async fn member_count(rig: &Rig) -> usize {
    let groups = rig.deps.registry.list().await.unwrap();
    groups[0].snapshot().await.unwrap().member_count
}

/// V65: dropping the Membership is a lost connection, not a leave. The seat
/// is kept through the grace, nobody is told the member left, and when no
/// reconnect comes the group removes it.
/// Disarm check: with `Drop` sending `RemoveMember` at once, the member count
/// is already 1 inside the grace and this fails.
#[actix_web::test]
async fn dropping_the_membership_keeps_the_seat_through_the_grace() {
    let rig = rig().await;
    let Pair {
        a: _a,
        b,
        mut a_seen,
        b_seen: _b_seen,
    } = playing_pair(&rig).await;
    assert_eq!(member_count(&rig).await, 2);
    drop(b);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(member_count(&rig).await, 2, "still seated inside the grace");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(member_count(&rig).await, 1, "removed once the grace passed");
    let _ = until(&mut a_seen, |s| matches!(s, Seen::Left(_)).then_some(())).await;
}

/// A reconnect inside the grace (the same device joining again) takes the
/// seat over: the old drop's removal does not remove the new membership.
/// Disarm check: with the removal skipping the generation fence, the member
/// count falls to 1 after the grace and this fails.
#[actix_web::test]
async fn a_rejoin_inside_the_grace_keeps_the_member() {
    let rig = rig().await;
    let pair = playing_pair(&rig).await;
    let gid = only_group(&rig).await;
    drop(pair.b);
    let b_session = rig.session(&rig.fx.user_token).await;
    let (sink, _seen) = sink();
    let _again = join(&b_session, "dev-b", &gid, sink).await.unwrap();
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(member_count(&rig).await, 2);
}

/// US2: when a member buffers, the group's other members are told which one,
/// and told again when it recovers.
/// Disarm check: with `changes_buffering` returning false, no `member` update
/// arrives and this times out.
#[actix_web::test]
async fn a_members_buffering_reaches_the_others_as_a_member_update() {
    let rig = rig().await;
    let mut pair = playing_pair(&rig).await;
    command!(pair.a, unpause_request).unwrap();
    let _ = until(&mut pair.b_seen, |s| {
        matches!(s, Seen::Play { .. }).then_some(())
    })
    .await;
    command!(
        pair.b,
        buffering_request,
        set_position_ms(6_000),
        set_buffering(true)
    )
    .unwrap();
    // Skip the join announcement still queued ahead of it.
    let stalled = until(&mut pair.a_seen, |s| match s {
        Seen::Member {
            name,
            buffering: true,
            ..
        } => Some((name.clone(), true)),
        _ => None,
    })
    .await;
    assert_eq!(stalled, ("guest".to_owned(), true));
    command!(
        pair.b,
        ready_request,
        set_position_ms(6_000),
        set_playing(true)
    )
    .unwrap();
    let recovered = until(&mut pair.a_seen, |s| match s {
        Seen::Member {
            name,
            buffering: false,
            ..
        } => Some((name.clone(), false)),
        _ => None,
    })
    .await;
    assert_eq!(recovered, ("guest".to_owned(), false));
}

/// The liveness beacon and the clock report are accepted from a member, and
/// refused once it has left.
#[actix_web::test]
async fn ping_and_clock_report_are_accepted_until_the_member_leaves() {
    let rig = rig().await;
    let pair = playing_pair(&rig).await;
    command!(pair.a, ping_request).unwrap();
    command!(
        pair.a,
        clock_report_request,
        set_t1(1),
        set_t2(2),
        set_t3(3),
        set_t4(4)
    )
    .unwrap();
    command!(pair.a, leave_request).unwrap();
    assert_eq!(
        command!(pair.a, ping_request).unwrap_err(),
        "not found: group"
    );
}

/// While the group is paused the actor sends nothing on a buffering change, so
/// the others learn of it (and of its end) from the pump's poll.
/// Disarm check: with the poll tick doing nothing, no `member` update arrives
/// and this times out.
#[actix_web::test]
async fn a_pauses_buffering_reaches_the_others_by_the_poll() {
    let rig = rig().await;
    let mut pair = playing_pair(&rig).await;
    command!(pair.a, unpause_request).unwrap();
    command!(pair.a, pause_request).unwrap();
    settle(&mut pair.b_seen).await;
    command!(
        pair.a,
        buffering_request,
        set_position_ms(5_000),
        set_buffering(true)
    )
    .unwrap();
    let stalled = until_polled(&mut pair.b_seen, |s| match s {
        Seen::Member {
            name,
            buffering: true,
            ..
        } => Some(name.clone()),
        _ => None,
    })
    .await;
    assert_eq!(stalled, "ali");
    command!(
        pair.a,
        buffering_request,
        set_position_ms(5_000),
        set_buffering(false)
    )
    .unwrap();
    let recovered = until_polled(&mut pair.b_seen, |s| match s {
        Seen::Member {
            name,
            buffering: false,
            ..
        } => Some(name.clone()),
        _ => None,
    })
    .await;
    assert_eq!(recovered, "ali");
}

/// A member joining while another buffers is told so in the roster it joins.
/// Disarm check: with the `joined` roster sending `buffering: false` for
/// everyone, the buffering list is empty and this fails.
#[actix_web::test]
async fn a_joiner_sees_who_is_already_buffering() {
    let rig = rig().await;
    let mut pair = playing_pair(&rig).await;
    command!(pair.a, unpause_request).unwrap();
    command!(pair.a, pause_request).unwrap();
    settle(&mut pair.b_seen).await;
    command!(
        pair.a,
        buffering_request,
        set_position_ms(5_000),
        set_buffering(true)
    )
    .unwrap();
    let _ = until_polled(&mut pair.b_seen, |s| {
        matches!(
            s,
            Seen::Member {
                buffering: true,
                ..
            }
        )
        .then_some(())
    })
    .await;
    let gid = only_group(&rig).await;
    let c_session = rig.session(&rig.fx.user_token).await;
    let (c_sink, mut c_seen) = sink();
    let _c = join(&c_session, "dev-c", &gid, c_sink).await.unwrap();
    let Seen::Joined { buffering, .. } = until(&mut c_seen, |s| {
        matches!(s, Seen::Joined { .. }).then(|| s.clone())
    })
    .await
    else {
        unreachable!()
    };
    assert_eq!(buffering, ["ali"]);
    // The poll does not announce what the roster already said.
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    while let Ok(seen) = c_seen.try_recv() {
        assert!(!matches!(seen, Seen::Member { .. }), "{seen:?}");
    }
}

/// A device is in one group at a time: creating a group from a device that is
/// in another takes it out of the old roster, the old membership's sink is
/// told `ended(left)`, and the old membership's commands are refused without
/// reaching the group.
/// Disarm check: with `leave_previous` doing nothing, the old group still
/// counts two members and the old sink hears nothing.
#[actix_web::test]
async fn creating_from_a_device_in_another_group_moves_it() {
    let rig = rig().await;
    let mut pair = playing_pair(&rig).await;
    let old = rig.deps.registry.list().await.unwrap().remove(0);
    let b_session = rig.session(&rig.fx.user_token).await;
    let (new_sink, _new_seen) = sink();
    let _moved = create(&b_session, "dev-b", MOVIE, 0, new_sink)
        .await
        .unwrap();
    assert_eq!(
        until(&mut pair.b_seen, |s| {
            matches!(s, Seen::Ended(_)).then(|| s.clone())
        })
        .await,
        Seen::Ended("left")
    );
    let _ = until(&mut pair.a_seen, |s| {
        matches!(s, Seen::Left(_)).then_some(())
    })
    .await;
    assert_eq!(old.snapshot().await.unwrap().member_count, 1);
    // The stale membership neither commands the old group nor the new one.
    command!(pair.a, unpause_request).unwrap();
    assert_eq!(
        command!(pair.b, pause_request).unwrap_err(),
        "not found: membership"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    while let Ok(seen) = pair.a_seen.try_recv() {
        assert!(!matches!(seen, Seen::Pause { .. }), "{seen:?}");
    }
}

/// Joining another group does the same.
#[actix_web::test]
async fn joining_from_a_device_in_another_group_moves_it() {
    let rig = rig().await;
    let mut pair = playing_pair(&rig).await;
    let old = rig.deps.registry.list().await.unwrap().remove(0);
    let (other, _other_rx) = rig.jellyfin_group("Other").await;
    let b_session = rig.session(&rig.fx.user_token).await;
    let (new_sink, mut new_seen) = sink();
    let _moved = join(&b_session, "dev-b", &other.group_id.to_string(), new_sink)
        .await
        .unwrap();
    let _ = until(&mut new_seen, |s| {
        matches!(s, Seen::Joined { .. }).then_some(())
    })
    .await;
    assert_eq!(
        until(&mut pair.b_seen, |s| {
            matches!(s, Seen::Ended(_)).then(|| s.clone())
        })
        .await,
        Seen::Ended("left")
    );
    assert_eq!(old.snapshot().await.unwrap().member_count, 1);
    assert_eq!(
        command!(pair.b, pause_request).unwrap_err(),
        "not found: membership"
    );
}

// ---- US6: what each member has seen --------------------------------------

/// One member's row of a `seen` answer.
#[derive(Debug, PartialEq)]
struct SeenRow {
    name: String,
    items: Vec<SeenItem>,
}

#[derive(Debug, PartialEq, Clone, Copy)]
struct SeenItem {
    id: WireId,
    played: bool,
    pct: u8,
    played_count: u32,
    total_count: u32,
}

async fn seen(member: &membership::Client, ids: &[WireId]) -> Result<Vec<SeenRow>, String> {
    let mut req = member.seen_request();
    {
        let mut p = req.get();
        set_device(p.reborrow().init_ctx(), "dev");
        let mut list = p.init_ids(u32::try_from(ids.len()).unwrap());
        for (i, id) in (0u32..).zip(ids) {
            match id {
                WireId::Real(n) => list.reborrow().get(i).set_real(*n),
                WireId::Synth(n) => list.reborrow().get(i).set_synth(*n),
            }
        }
    }
    let reply = req.send().promise.await.expect("seen");
    let result = reply.get().unwrap().get_result().unwrap();
    match result.which().unwrap() {
        seen_result::Error(e) => Err(denial(e.unwrap())),
        seen_result::Members(members) => Ok(members
            .unwrap()
            .iter()
            .map(|m| SeenRow {
                name: m
                    .get_member()
                    .unwrap()
                    .get_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned(),
                items: m
                    .get_items()
                    .unwrap()
                    .iter()
                    .map(|i| SeenItem {
                        id: match i.get_id().unwrap().which().unwrap() {
                            pharos_api::pharos_capnp::item_id::Real(n) => WireId::Real(n),
                            pharos_api::pharos_capnp::item_id::Synth(n) => WireId::Synth(n),
                        },
                        played: i.get_played(),
                        pct: i.get_position_pct(),
                        played_count: i.get_played_count(),
                        total_count: i.get_total_count(),
                    })
                    .collect(),
            })
            .collect()),
    }
}

const SHOW_S1: [u64; 3] = [801, 802, 803];
const SHOW_S2: u64 = 804;
const SHOW_DIR: &str = "/libA/Show";

/// Four episodes of "Show" in `/libA` (three in season 1, one in season 2,
/// each 100 s long), as A and B's library.
async fn seed_show(rig: &Rig) {
    use pharos_core::{MediaItem, MediaKind, MediaProbe, MediaStore as _, SeriesInfo};
    for (id, season, ep) in [(801, 1, 1), (802, 1, 2), (803, 1, 3), (SHOW_S2, 2, 1)] {
        rig.fx
            .state
            .stores
            .put(MediaItem {
                id,
                path: format!("{SHOW_DIR}/Season {season:02}/e{ep}.mkv").into(),
                title: format!("Show {season}x{ep}"),
                kind: MediaKind::Episode,
                probe: MediaProbe {
                    duration_ms: Some(100_000),
                    ..MediaProbe::default()
                },
                series: Some(SeriesInfo {
                    series_name: "Show".into(),
                    season_number: Some(season),
                    episode_number: Some(ep),
                    series_folder: Some(SHOW_DIR.into()),
                    series_year: None,
                }),
                ..MediaItem::default()
            })
            .await
            .unwrap();
    }
}

fn synth(key: String) -> WireId {
    let id = WireId::parse(&key).unwrap();
    assert!(matches!(id, WireId::Synth(_)), "{key} is a synth id");
    id
}

fn series_id() -> WireId {
    synth(pharos_jellyfin_api::dto::series_id_for_key(
        Some(SHOW_DIR),
        "Show",
    ))
}

fn season_id(n: u32) -> WireId {
    synth(pharos_jellyfin_api::dto::season_id_for_key(
        Some(SHOW_DIR),
        "Show",
        n,
    ))
}

async fn a_plays(rig: &Rig, id: u64, data: pharos_core::UserItemData) {
    use pharos_core::UserDataStore as _;
    rig.fx
        .state
        .stores
        .set_user_data(rig.fx.admin_id, id, data)
        .await
        .unwrap();
}

fn played() -> pharos_core::UserItemData {
    pharos_core::UserItemData {
        played: true,
        play_count: 1,
        ..pharos_core::UserItemData::default()
    }
}

/// The row of the member named `name` (the fixture's users: "ali" is A, the
/// admin, and "guest" is B).
fn row<'a>(rows: &'a [SeenRow], name: &str) -> &'a SeenRow {
    rows.iter().find(|r| r.name == name).unwrap()
}

/// B sees that A played one episode, is part-way through another and has not
/// started a third, and that B has seen none of them.
/// Disarm check: with `item_seen` reporting `played: false` for a `Target::Item`,
/// A's first episode shows as unseen and this fails.
#[actix_web::test]
async fn a_member_sees_what_a_peer_has_played_of_an_episode() {
    let rig = rig().await;
    seed_show(&rig).await;
    a_plays(&rig, 801, played()).await;
    a_plays(
        &rig,
        802,
        pharos_core::UserItemData {
            last_played_position_ticks: 25_000 * 10_000,
            ..pharos_core::UserItemData::default()
        },
    )
    .await;
    let pair = playing_pair(&rig).await;
    let ids = SHOW_S1.map(WireId::Real);
    let rows = seen(&pair.b, &ids).await.unwrap();
    assert_eq!(rows.len(), 2, "both members are listed {rows:?}");
    let item = |id: u64, played: bool, pct: u8| SeenItem {
        id: WireId::Real(id),
        played,
        pct,
        played_count: 0,
        total_count: 0,
    };
    assert_eq!(
        row(&rows, "ali").items,
        vec![
            item(801, true, 0),
            item(802, false, 25),
            item(803, false, 0)
        ]
    );
    assert!(row(&rows, "guest")
        .items
        .iter()
        .all(|i| !i.played && i.pct == 0));
}

/// A series and a season answer with the episodes a member has played of the
/// ones the caller may see.
/// Disarm check: with `resolve` filing an episode under its series only (not
/// its season), the season rows are omitted and this fails.
#[actix_web::test]
async fn a_series_and_a_season_count_the_peers_played_episodes() {
    let rig = rig().await;
    seed_show(&rig).await;
    a_plays(&rig, 801, played()).await;
    a_plays(&rig, SHOW_S2, played()).await;
    let pair = playing_pair(&rig).await;
    let rows = seen(&pair.b, &[series_id(), season_id(1), season_id(2)])
        .await
        .unwrap();
    let count = |id, played_count, total_count| SeenItem {
        id,
        played: played_count == total_count,
        pct: 0,
        played_count,
        total_count,
    };
    assert_eq!(
        row(&rows, "ali").items,
        vec![
            count(series_id(), 2, 4),
            count(season_id(1), 1, 3),
            count(season_id(2), 1, 1)
        ]
    );
}

/// What the caller may not see is in no member's answer, and its count never
/// includes episodes the caller may not see.
/// Disarm check: with `seen` skipping `retain_visible`, the hidden episode
/// comes back with A's state and this fails.
#[actix_web::test]
async fn an_item_the_caller_may_not_see_is_omitted() {
    let rig = rig().await;
    seed_show(&rig).await;
    a_plays(&rig, 801, played()).await;
    let pair = playing_pair(&rig).await;
    rig.fx
        .state
        .stores
        .set_policy(
            rig.fx.user_id,
            UserPolicy {
                enable_all_folders: false,
                enabled_folders: vec![rig.fx.lib_b_wire.clone()],
                ..UserPolicy::default()
            },
        )
        .await
        .unwrap();
    let other = rig.fx.other_item_id;
    let rows = seen(
        &pair.b,
        &[WireId::Real(801), series_id(), WireId::Real(other)],
    )
    .await
    .unwrap();
    for r in &rows {
        assert_eq!(
            r.items.iter().map(|i| i.id).collect::<Vec<_>>(),
            vec![WireId::Real(other)],
            "{}: only the visible item",
            r.name
        );
    }
}

/// A member with no recorded user is listed with no items; a caller who has
/// left the group, or asks for too many ids, is refused.
/// Disarm check: with the `in_group` check removed, the call after `leave`
/// answers and this fails.
#[actix_web::test]
async fn seen_is_refused_outside_the_group_and_over_the_cap() {
    let rig = rig().await;
    seed_show(&rig).await;
    let pair = playing_pair(&rig).await;
    let groups = rig.deps.registry.list().await.unwrap();
    let (reply, joined) = oneshot::channel();
    groups[0]
        .tx
        .send(GroupMsg::AddMember {
            member_id: pharos_sync::MemberId::new(),
            name: "ghost".into(),
            user: None,
            reply,
        })
        .await
        .unwrap();
    joined.await.unwrap();
    let rows = seen(&pair.b, &[WireId::Real(801)]).await.unwrap();
    let ghost = rows.iter().find(|r| r.name == "ghost").unwrap();
    assert!(ghost.items.is_empty());
    let many: Vec<WireId> = (0..201).map(WireId::Real).collect();
    assert_eq!(seen(&pair.b, &many).await.unwrap_err(), "limit: seen_ids");
    command!(pair.b, leave_request).unwrap();
    assert_eq!(
        seen(&pair.b, &[WireId::Real(801)]).await.unwrap_err(),
        "not found: group"
    );
}
