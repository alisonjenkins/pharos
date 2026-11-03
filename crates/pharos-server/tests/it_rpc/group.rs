//! Spec 014: a native client listing, joining and driving the SyncPlay groups
//! jellyfin-web uses. The groups, access rules and queue checks are the
//! Jellyfin handlers' own, so a stand-in Jellyfin member here is a hub-
//! registered sink exactly as `/socket` makes one.

use std::sync::Arc;

use pharos_api::pharos_capnp::{clock_result, error, groups_result, session};
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
