//! Spec 014: a native client listing, joining and driving the SyncPlay groups
//! jellyfin-web uses. The groups, access rules and queue checks are the
//! Jellyfin handlers' own, so a stand-in Jellyfin member here is a hub-
//! registered sink exactly as `/socket` makes one.

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use pharos_api::pharos_capnp::{
    clock_result, error, group_sink, group_update, groups_result, membership, membership_result,
    session, EndReason,
};
use pharos_api::{server, Connection};
use pharos_core::{UserPolicy, UserStore as _};
use pharos_jellyfin_api::dto::wire_item_id;
use pharos_server::group_ops::SyncDeps;
use pharos_sync::group::{GroupHandle, GroupMsg};
use pharos_sync::messages::ServerMsg;
use pharos_sync::{GroupRegistry, LocalDelivery, MemberSinks, SessionHub};
use tokio::sync::{mpsc, oneshot};

use crate::common::Fixture;
use crate::playback::{seed_playable, MOVIE};

struct Rig {
    server: actix_test::TestServer,
    fx: Fixture,
    deps: SyncDeps,
}

async fn rig() -> Rig {
    let (server, fx) = crate::revocation::start().await;
    seed_playable(&fx).await;
    let sinks = MemberSinks::new();
    let deps = SyncDeps {
        hub: SessionHub::new(),
        registry: GroupRegistry::spawn(Arc::new(LocalDelivery::new(sinks.clone()))),
        sinks,
    };
    fx.state.sync.install(deps.clone());
    Rig { server, fx, deps }
}

impl Rig {
    async fn session(&self, token: &str) -> session::Client {
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
    },
    Play {
        at: u64,
        position_ms: u64,
    },
    Pause {
        at: u64,
        position_ms: u64,
    },
    Seek {
        at: u64,
        position_ms: u64,
    },
    Member {
        name: String,
        buffering: bool,
    },
    Left(String),
    Item {
        item: u64,
        playing: bool,
        position_ms: u64,
    },
    Ended(&'static str),
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
                }
            }
            group_update::Play(a) => {
                let a = a?;
                Seen::Play {
                    at: a.get_at_server_ms(),
                    position_ms: a.get_position_ms(),
                }
            }
            group_update::Pause(a) => {
                let a = a?;
                Seen::Pause {
                    at: a.get_at_server_ms(),
                    position_ms: a.get_position_ms(),
                }
            }
            group_update::Seek(a) => {
                let a = a?;
                Seen::Seek {
                    at: a.get_at_server_ms(),
                    position_ms: a.get_position_ms(),
                }
            }
            group_update::Member(m) => {
                let m = m?;
                Seen::Member {
                    name: text(m.get_name()),
                    buffering: m.get_buffering(),
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
