//! T042 / T043 (spec 009): a cast survives a server restart and a change of
//! group owner. A "process" is a registry + hub + sinks + cast service over a
//! shared user/token store and a shared snapshot store; a restart is a second
//! process booted over the first one's last snapshot with a fresh emulator
//! standing in for the TV.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

mod common;

use actix_web::{http::StatusCode, test, web};
use metrics_util::debugging::{DebugValue, DebuggingRecorder, Snapshotter};
use pharos_cast::fake::FakeReceiverHandle;
use pharos_cast::messages::CommandBody;
use pharos_cast::obs::ConnState;
use pharos_core::{SecretString, UserId, UserPolicy, UserRecord, UserStore};
use pharos_server::auth::BuiltinAuth;
use pharos_server::cast_service::{
    self, CastDeps, CastServiceHandle, FakeTransportFactory, StartCast,
};
use pharos_server::config::CastConfig;
use pharos_server::state::{AppState, Stores};
use pharos_sync::distributed::{
    CommandSink, Distributed, HydrationSource, LoadFuture, OwnFuture, OwnershipSource,
};
use pharos_sync::group::{GroupHandle, GroupMsg, GroupSnapshot, RemoteCommand};
use pharos_sync::messages::{GroupId, GroupPlayState, MemberId, ServerMsg};
use pharos_sync::persistence::GroupPersistence;
use pharos_sync::{GroupRegistry, LocalDelivery, MemberSinks, SessionHub};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

const TV: &str = "Living Room";
const ITEM: &str = "item-1";
const START_MS: u64 = 120_000;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the owner replica persists and a taker hydrates from.
#[derive(Default)]
struct Snapshots(Mutex<HashMap<GroupId, (u64, String)>>);

impl Snapshots {
    fn get(&self, id: GroupId) -> Option<(u64, String)> {
        lock(&self.0).get(&id).cloned()
    }
    fn put(&self, id: GroupId, row: (u64, String)) {
        lock(&self.0).insert(id, row);
    }
}

impl GroupPersistence for Snapshots {
    fn persist(&self, group_id: GroupId, epoch_unix_ms: u64, state_json: String) {
        self.put(group_id, (epoch_unix_ms, state_json));
    }
    fn remove(&self, group_id: GroupId) {
        lock(&self.0).remove(&group_id);
    }
}

impl HydrationSource for Snapshots {
    fn load(&self, group_id: GroupId) -> LoadFuture<'_> {
        let row = self.get(group_id);
        Box::pin(async move { row })
    }
}

/// Stand-in for the advisory locks: a key is held by at most one process.
#[derive(Clone, Default)]
struct Locks(Arc<Mutex<HashMap<GroupId, usize>>>);

struct Ownership {
    locks: Locks,
    me: usize,
}

impl OwnershipSource for Ownership {
    fn try_own(&self, group_id: GroupId) -> OwnFuture<'_> {
        Box::pin(async move {
            let mut held = lock(&self.locks.0);
            match held.get(&group_id) {
                Some(owner) if *owner != self.me => false,
                _ => {
                    held.insert(group_id, self.me);
                    true
                }
            }
        })
    }
    fn release(&self, group_id: GroupId) {
        let mut held = lock(&self.locks.0);
        if held.get(&group_id) == Some(&self.me) {
            held.remove(&group_id);
        }
    }
}

struct NoCommands;
impl CommandSink for NoCommands {
    fn submit(&self, _group_id: GroupId, _cmd: RemoteCommand) {}
}

fn config(tvs: &[&str]) -> CastConfig {
    let tvs: String = tvs
        .iter()
        .enumerate()
        .map(|(i, name)| {
            format!(
                "[[tvs]]\nname = \"{name}\"\naddress = \"192.168.1.{}:8009\"\n",
                60 + i
            )
        })
        .collect();
    toml::from_str(&format!(
        "server_url = \"http://pharos.lan:8096\"\nreconnect_grace_secs = 1\n{tvs}"
    ))
    .unwrap()
}

/// One server process.
struct Process {
    stores: Stores,
    hub: SessionHub,
    sinks: MemberSinks,
    registry: GroupRegistry,
    service: CastServiceHandle,
    factory: FakeTransportFactory,
    tv: FakeReceiverHandle,
}

impl Process {
    fn boot(
        stores: &Stores,
        snapshots: &Arc<Snapshots>,
        locks: &Locks,
        me: usize,
        cfg: CastConfig,
        tvs: &[&str],
    ) -> Self {
        let sinks = MemberSinks::new();
        let hub = SessionHub::new();
        let registry = GroupRegistry::spawn_distributed(
            Arc::new(LocalDelivery::new(sinks.clone())),
            Distributed {
                ownership: Arc::new(Ownership {
                    locks: locks.clone(),
                    me,
                }),
                hydration: snapshots.clone(),
                commands: Arc::new(NoCommands),
                persistence: snapshots.clone(),
            },
        );
        let factory = FakeTransportFactory::new();
        let mut tv = None;
        for name in tvs {
            let handle = factory.add_tv(name);
            if *name == TV {
                tv = Some(handle);
            }
        }
        let tv = tv.unwrap_or_else(|| factory.add_tv("unused"));
        let service = cast_service::spawn(
            CastDeps {
                hub: hub.clone(),
                registry: registry.clone(),
                sinks: sinks.clone(),
                stores: stores.clone(),
            },
            cfg,
            factory.clone(),
        );
        Self {
            stores: stores.clone(),
            hub,
            sinks,
            registry,
            service,
            factory,
            tv,
        }
    }
}

struct Owner {
    id: UserId,
    key: String,
    member: MemberId,
    _rx: mpsc::Receiver<ServerMsg>,
}

async fn new_user(stores: &Stores, name: &str) -> UserId {
    let auth = BuiltinAuth::new(stores.clone());
    let id = UserId::new();
    stores
        .create(UserRecord {
            id,
            name: name.into(),
            password_hash: auth.hash_password(&SecretString::new("p")).unwrap(),
            policy: UserPolicy::default(),
        })
        .await
        .unwrap();
    id
}

/// Process A: a group seeded mid-item, `ali` in it, and a cast to the TV.
async fn running_cast(
    stores: &Stores,
    snapshots: &Arc<Snapshots>,
    a: &Process,
) -> (GroupHandle, Owner) {
    let group = a.registry.create().await.unwrap();
    let id = new_user(stores, "ali").await;
    let key = format!("{}:dev-ali", id.0.simple());
    let (tx, rx) = mpsc::channel(256);
    let reg = a.hub.register(key.clone(), "ali".into(), tx.clone());
    a.hub.attach_group(&key, group.clone());
    a.sinks.insert(reg.member_id, reg.gen, tx);
    let (reply, joined) = oneshot::channel();
    group
        .tx
        .send(GroupMsg::AddMember {
            member_id: reg.member_id,
            name: "ali".into(),
            reply,
        })
        .await
        .unwrap();
    joined.await.unwrap();
    group
        .tx
        .send(GroupMsg::SeedQueue {
            item_id: ITEM.into(),
            runtime_ms: Some(3_600_000),
            position_ms: START_MS,
            is_paused: false,
        })
        .await
        .unwrap();
    a.service
        .start(StartCast {
            user_id: id,
            user_name: "ali".into(),
            owner_sync_key: key.clone(),
            tv_name: TV.into(),
        })
        .await
        .unwrap();
    eventually("PlayNow on the first emulator", || {
        has_command(&a.tv, "PlayNow")
    })
    .await;
    eventually("the seat in the persisted snapshot", || {
        snapshots
            .get(group.group_id)
            .is_some_and(|(_, json)| json.contains("CastSeat"))
    })
    .await;
    (
        group,
        Owner {
            id,
            key,
            member: reg.member_id,
            _rx: rx,
        },
    )
}

async fn eventually<F: FnMut() -> bool>(what: &str, mut f: F) {
    for _ in 0..250 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for: {what}");
}

async fn wait_following(service: &CastServiceHandle) {
    for _ in 0..250 {
        let casts = service.snapshot().await;
        if casts.first().map(|c| c.state) == Some(ConnState::Following) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "the seat never reached Following: {:?}",
        service.snapshot().await
    );
}

fn has_command(h: &FakeReceiverHandle, name: &str) -> bool {
    h.commands().iter().any(|c| c.body.name() == name)
}

fn play_now_position_ms(h: &FakeReceiverHandle) -> Option<(String, i64)> {
    h.commands().into_iter().find_map(|c| match c.body {
        CommandBody::PlayNow(o) => {
            Some((o.items.first()?.id.clone(), o.start_position_ticks / 10_000))
        }
        _ => None,
    })
}

fn tv_token(h: &FakeReceiverHandle) -> String {
    h.commands()
        .first()
        .map(|c| c.access_token.expose().to_owned())
        .expect("the TV was sent a command carrying its token")
}

async fn token_status(stores: &Stores, token: &str) -> StatusCode {
    let state = web::Data::new(AppState::new(stores.clone(), "srv".into()));
    let app = test::init_service(common::build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Users/Me")
        .insert_header((
            "Authorization",
            format!(
                "MediaBrowser Client=\"Chromecast\", Device=\"Chromecast\", \
                 DeviceId=\"dGlr\", Version=\"3.0\", Token=\"{token}\""
            ),
        ))
        .to_request();
    test::call_service(&app, req).await.status()
}

async fn snapshot_of(group: &GroupHandle) -> GroupSnapshot {
    group.snapshot().await.expect("a local group actor")
}

fn seat_total(snap: &Snapshotter, event: &str, reason: &str) -> u64 {
    snap.snapshot()
        .into_vec()
        .into_iter()
        .filter(|(ck, _, _, _)| ck.key().name() == "pharos_cast_seat_total")
        .filter(|(ck, _, _, _)| {
            let l: Vec<_> = ck.key().labels().collect();
            l.iter().any(|x| x.key() == "event" && x.value() == event)
                && l.iter().any(|x| x.key() == "reason" && x.value() == reason)
        })
        .map(|(_, _, _, v)| match v {
            DebugValue::Counter(c) => c,
            _ => 0,
        })
        .sum()
}

fn recorder() -> (Snapshotter, metrics::LocalRecorderGuard<'static>) {
    let recorder = DebuggingRecorder::new();
    let snap = recorder.snapshotter();
    let recorder: &'static DebuggingRecorder = Box::leak(Box::new(recorder));
    (snap, metrics::set_default_local_recorder(recorder))
}

/// The state a restarted process starts from: the last snapshot A persisted.
fn restart_snapshots(old: &Snapshots, group: GroupId) -> Arc<Snapshots> {
    let fresh = Arc::new(Snapshots::default());
    fresh.put(group, old.get(group).expect("A persisted the group"));
    fresh
}

#[tokio::test]
async fn a_persisted_cast_seat_is_resumed_after_a_restart() {
    let (_snap, _guard) = recorder();
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let snapshots = Arc::new(Snapshots::default());
    let a = Process::boot(
        &stores,
        &snapshots,
        &Locks::default(),
        1,
        config(&[TV]),
        &[TV],
    );
    let (group_a, owner) = running_cast(&stores, &snapshots, &a).await;
    let old_token = tv_token(&a.tv);
    assert_eq!(token_status(&stores, &old_token).await, StatusCode::OK);

    // The process dies without a word: A is left running but unreachable, and
    // B starts from what A last persisted.
    let b_snapshots = restart_snapshots(&snapshots, group_a.group_id);
    let b = Process::boot(
        &stores,
        &b_snapshots,
        &Locks::default(),
        2,
        config(&[TV]),
        &[TV],
    );
    let group_b = b.registry.get_or_create(group_a.group_id).await.unwrap();

    eventually("the seat following on the new emulator", || {
        has_command(&b.tv, "PlayNow")
    })
    .await;
    let (item, pos) = play_now_position_ms(&b.tv).unwrap();
    assert_eq!(item, ITEM);
    assert!(
        (pos - START_MS as i64).abs() < 15_000,
        "PlayNow at the group position, got {pos} ms"
    );
    wait_following(&b.service).await;
    let info = b.service.snapshot().await;
    assert_eq!(info.len(), 1, "{info:?}");
    assert_eq!(info[0].tv, TV);
    assert_eq!(info[0].owner, owner.id);
    assert_eq!(info[0].owner_member, owner.member);

    let snap = snapshot_of(&group_b).await;
    assert_eq!(snap.participants, ["ali"], "the owner appears once");
    assert_eq!(snap.cast_seats.len(), 1);
    assert_eq!(snap.cast_seats[0].controller, Some(owner.member));

    assert_eq!(
        token_status(&stores, &old_token).await,
        StatusCode::UNAUTHORIZED,
        "the token the dead process minted must not outlive its cast"
    );
    let new_token = tv_token(&b.tv);
    assert_ne!(new_token, old_token);
    assert_eq!(token_status(&stores, &new_token).await, StatusCode::OK);
}

#[tokio::test]
async fn a_tv_no_longer_configured_ends_the_cast_cleanly() {
    let (snap, _guard) = recorder();
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let snapshots = Arc::new(Snapshots::default());
    let a = Process::boot(
        &stores,
        &snapshots,
        &Locks::default(),
        1,
        config(&[TV]),
        &[TV],
    );
    let (group_a, owner) = running_cast(&stores, &snapshots, &a).await;
    let old_token = tv_token(&a.tv);

    let b_snapshots = restart_snapshots(&snapshots, group_a.group_id);
    let b = Process::boot(
        &stores,
        &b_snapshots,
        &Locks::default(),
        2,
        config(&["Den"]),
        &["Den"],
    );
    let group_b = b.registry.get_or_create(group_a.group_id).await.unwrap();

    eventually("the orphaned seat removed", || {
        seat_total(&snap, "ended", "tv_removed") == 1
    })
    .await;
    assert!(b.service.snapshot().await.is_empty());
    let after = snapshot_of(&group_b).await;
    assert!(after.cast_seats.is_empty(), "{:?}", after.cast_seats);
    assert_eq!(after.member_count, 1, "only the owner's player is left");
    assert_eq!(after.participants, ["ali"]);
    assert_eq!(
        token_status(&stores, &old_token).await,
        StatusCode::UNAUTHORIZED
    );
    assert!(
        !has_command(&b.tv, "PlayNow"),
        "nothing is played on an unconfigured TV"
    );
    let _ = owner;
}

#[tokio::test]
async fn a_tv_that_stays_unreachable_ends_the_resumed_cast_after_the_grace() {
    let (snap, _guard) = recorder();
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let snapshots = Arc::new(Snapshots::default());
    let a = Process::boot(
        &stores,
        &snapshots,
        &Locks::default(),
        1,
        config(&[TV]),
        &[TV],
    );
    let (group_a, _owner) = running_cast(&stores, &snapshots, &a).await;

    let b_snapshots = restart_snapshots(&snapshots, group_a.group_id);
    let b = Process::boot(
        &stores,
        &b_snapshots,
        &Locks::default(),
        2,
        config(&[TV]),
        &[TV],
    );
    b.factory.set_unreachable(TV, true);
    let group_b = b.registry.get_or_create(group_a.group_id).await.unwrap();

    eventually("the unreachable TV's seat given up", || {
        seat_total(&snap, "ended", "tv_unreachable") == 1
    })
    .await;
    assert!(b.service.snapshot().await.is_empty());
    assert!(snapshot_of(&group_b).await.cast_seats.is_empty());
}

#[tokio::test]
async fn a_resumed_cast_still_obeys_the_tvs_own_remote() {
    let (_snap, _guard) = recorder();
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let snapshots = Arc::new(Snapshots::default());
    let a = Process::boot(
        &stores,
        &snapshots,
        &Locks::default(),
        1,
        config(&[TV]),
        &[TV],
    );
    let (group_a, _owner) = running_cast(&stores, &snapshots, &a).await;

    let b_snapshots = restart_snapshots(&snapshots, group_a.group_id);
    let b = Process::boot(
        &stores,
        &b_snapshots,
        &Locks::default(),
        2,
        config(&[TV]),
        &[TV],
    );
    let group_b = b.registry.get_or_create(group_a.group_id).await.unwrap();
    eventually("PlayNow on the new emulator", || {
        has_command(&b.tv, "PlayNow")
    })
    .await;
    wait_following(&b.service).await;

    b.tv.user_pause();
    for _ in 0..250 {
        if snapshot_of(&group_b).await.play_state == GroupPlayState::Paused {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("a Pause on the TV's remote never reached the group");
}

#[tokio::test]
async fn shutting_the_service_down_leaves_the_seat_in_the_snapshot() {
    let (_snap, _guard) = recorder();
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let snapshots = Arc::new(Snapshots::default());
    let a = Process::boot(
        &stores,
        &snapshots,
        &Locks::default(),
        1,
        config(&[TV]),
        &[TV],
    );
    let (group_a, _owner) = running_cast(&stores, &snapshots, &a).await;
    drop(a);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let (_, json) = snapshots.get(group_a.group_id).expect("still persisted");
    assert!(
        json.contains("CastSeat"),
        "a restart must not erase the seat it is about to resume: {json}"
    );
}
