#![allow(clippy::unwrap_used, clippy::expect_used)]
//! V582 — `/SyncPlay/Join` of a group that does not exist is answered with
//! `GroupDoesNotExist` (Jellyfin's `SyncPlayManager.JoinGroup`), never by
//! spawning a phantom empty group the caller lands alone in.

use actix_web::{test, web, App};
use pharos_core::{
    PersistedSyncGroup, SecretString, SyncGroupStore, TokenStore, UserId, UserPolicy, UserRecord,
    UserStore,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};
use pharos_sync::distributed::{
    CommandSink, Distributed, HydrationSource, LoadFuture, OwnFuture, OwnershipSource,
};
use pharos_sync::group::{GroupMsg, RemoteCommand};
use pharos_sync::messages::{GroupId, ServerMsg};
use pharos_sync::persistence::GroupPersistence;
use pharos_sync::{GroupRegistry, LocalDelivery, MemberSinks, SessionHub};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

const DEV: &str = "dev-a";

struct Rig {
    uid: UserId,
    token: String,
    stores: Stores,
    state: web::Data<AppState>,
    registry: GroupRegistry,
    hub: SessionHub,
    sinks: MemberSinks,
    rx: mpsc::Receiver<ServerMsg>,
}

/// One user, one registered device socket, an empty single-replica registry.
async fn rig() -> Rig {
    rig_with(|_, sinks| GroupRegistry::spawn(Arc::new(LocalDelivery::new(sinks.clone())))).await
}

/// [`rig`] over the registry `make` builds from the stores and sinks.
async fn rig_with(make: impl FnOnce(&Stores, &MemberSinks) -> GroupRegistry) -> Rig {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("p")).unwrap();
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "lace".into(),
            password_hash: hash,
            policy: UserPolicy::default(),
        })
        .await
        .unwrap();
    let token = stores.issue(uid, "t").await.unwrap().0.expose().to_string();
    let state = web::Data::new(AppState::new(stores.clone(), "t".into()));
    let hub = SessionHub::new();
    let sinks = MemberSinks::new();
    let registry = make(&stores, &sinks);
    let (tx, rx) = mpsc::channel(64);
    let reg = hub.register(
        format!("{}:{DEV}", uid.0.simple()),
        "lace".into(),
        tx.clone(),
    );
    sinks.insert(reg.member_id, reg.gen, tx);
    Rig {
        uid,
        token,
        stores,
        state,
        registry,
        hub,
        sinks,
        rx,
    }
}

/// A replica of a distributed deployment: `held_elsewhere` says whether
/// another replica holds every group's lease, snapshots are hydrated from the
/// shared store, commands go nowhere.
struct Replica {
    held_elsewhere: bool,
}

impl OwnershipSource for Replica {
    fn try_own(&self, _group_id: GroupId) -> OwnFuture<'_> {
        let own = !self.held_elsewhere;
        Box::pin(async move { own })
    }
    fn release(&self, _group_id: GroupId) {}
    fn held_elsewhere(&self, _group_id: GroupId) -> OwnFuture<'_> {
        let held = self.held_elsewhere;
        Box::pin(async move { held })
    }
}

struct StoreHydration(Stores);

impl HydrationSource for StoreHydration {
    fn load(&self, group_id: GroupId) -> LoadFuture<'_> {
        Box::pin(async move {
            let row = self.0.get_sync_group(&group_id.to_string()).await.ok()??;
            Some((row.epoch_unix_ms as u64, row.state_json))
        })
    }
}

struct NoCommands;
impl CommandSink for NoCommands {
    fn submit(&self, _group_id: GroupId, _cmd: RemoteCommand) {}
}

#[derive(Default)]
struct Captured(Mutex<Option<String>>);
impl GroupPersistence for Captured {
    fn persist(&self, _group_id: GroupId, _epoch: u64, state_json: String) {
        *self.0.lock().unwrap() = Some(state_json);
    }
    fn remove(&self, _group_id: GroupId) {}
}

fn distributed(
    held_elsewhere: bool,
    hydration: Arc<dyn HydrationSource>,
    persistence: Arc<dyn GroupPersistence>,
) -> Distributed {
    Distributed {
        ownership: Arc::new(Replica { held_elsewhere }),
        hydration,
        commands: Arc::new(NoCommands),
        persistence,
    }
}

/// A distributed replica reading its snapshots from the rig's store.
fn replica(held_elsewhere: bool) -> impl FnOnce(&Stores, &MemberSinks) -> GroupRegistry {
    move |stores, sinks| {
        GroupRegistry::spawn_distributed(
            Arc::new(LocalDelivery::new(sinks.clone())),
            distributed(
                held_elsewhere,
                Arc::new(StoreHydration(stores.clone())),
                Arc::new(Captured::default()),
            ),
        )
    }
}

struct NoSnapshots;
impl HydrationSource for NoSnapshots {
    fn load(&self, _group_id: GroupId) -> LoadFuture<'_> {
        Box::pin(async { None })
    }
}

/// A group named `name` playing `item`, as its owner persists it: the id it
/// ran under and the snapshot the actor wrote.
async fn owner_snapshot(name: &str, item: &str) -> (GroupId, String) {
    let captured = Arc::new(Captured::default());
    let registry = GroupRegistry::spawn_distributed(
        Arc::new(LocalDelivery::new(MemberSinks::new())),
        distributed(false, Arc::new(NoSnapshots), captured.clone()),
    );
    let group = registry.create().await.unwrap();
    group
        .tx
        .send(GroupMsg::SetGroupName { name: name.into() })
        .await
        .unwrap();
    group
        .tx
        .send(GroupMsg::SeedQueue {
            item_id: item.into(),
            runtime_ms: None,
            position_ms: 0,
            is_paused: true,
        })
        .await
        .unwrap();
    group.snapshot().await.unwrap();
    let json = captured.0.lock().unwrap().clone().unwrap();
    (group.group_id, json)
}

async fn store_snapshot(stores: &Stores, gid: GroupId, json: &str, age_secs: i64) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    stores
        .upsert_sync_group(
            &PersistedSyncGroup {
                group_id: gid.to_string(),
                epoch_unix_ms: 1_000,
                state_json: json.into(),
                updated_at: 0,
            },
            now - age_secs,
        )
        .await
        .unwrap();
}

macro_rules! join {
    ($rig:expr, $gid:expr) => {{
        let app = test::init_service(
            App::new()
                .app_data($rig.state.clone())
                .app_data(web::Data::new($rig.registry.clone()))
                .app_data(web::Data::new($rig.hub.clone()))
                .app_data(web::Data::new($rig.sinks.clone()))
                .wrap(LowercasePath)
                .configure(jellyfin::configure),
        )
        .await;
        test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/SyncPlay/Join")
                .insert_header(("X-Emby-Token", $rig.token.as_str()))
                .insert_header(("X-Emby-Device-Id", DEV))
                .set_json(serde_json::json!({ "GroupId": $gid.to_string() }))
                .to_request(),
        )
        .await
        .status()
        .as_u16()
    }};
}

async fn next_matching(
    rx: &mut mpsc::Receiver<ServerMsg>,
    pred: impl Fn(&ServerMsg) -> bool,
) -> Option<ServerMsg> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while let Ok(Some(m)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        if pred(&m) {
            return Some(m);
        }
    }
    None
}

#[actix_web::test]
async fn joining_a_dead_group_says_group_does_not_exist_and_spawns_nothing() {
    let mut rig = rig().await;
    let dead = GroupId::new();
    assert_eq!(join!(rig, dead), 204);

    let told = next_matching(&mut rig.rx, |m| matches!(m, ServerMsg::GroupDoesNotExist)).await;
    assert!(told.is_some(), "caller must be told GroupDoesNotExist");
    assert!(
        rig.registry.get(dead).await.unwrap().is_none(),
        "no phantom group may be spawned for a dead id"
    );
    assert!(rig.registry.list().await.unwrap().is_empty());
}

#[actix_web::test]
async fn joining_a_live_group_is_unchanged() {
    let mut rig = rig().await;
    let live = rig.registry.create().await.unwrap();
    assert_eq!(join!(rig, live.group_id), 204);

    let joined = next_matching(&mut rig.rx, |m| matches!(m, ServerMsg::Joined { .. })).await;
    assert!(joined.is_some(), "a live group admits the joiner");
}

/// A group that survives only as a fresh persisted snapshot (restart) is
/// listed by `/SyncPlay/List` (B28), so Join must hydrate it, not refuse it:
/// the hydrated group is the snapshot's, not an empty one.
#[actix_web::test]
async fn joining_a_fresh_persisted_only_group_hydrates_it() {
    let item = pharos_jellyfin_api::dto::wire_item_id(7);
    let (gid, json) = owner_snapshot("Movie Night", &item).await;
    let mut rig = rig_with(replica(false)).await;
    store_snapshot(&rig.stores, gid, &json, 60).await;
    assert!(rig.registry.get(gid).await.unwrap().is_none());
    assert_eq!(join!(rig, gid), 204);

    assert!(
        next_matching(&mut rig.rx, |m| matches!(m, ServerMsg::GroupDoesNotExist))
            .await
            .is_none(),
        "a fresh snapshot is a joinable group"
    );
    let snap = rig
        .registry
        .get(gid)
        .await
        .unwrap()
        .expect("the join hydrated the group")
        .snapshot()
        .await
        .unwrap();
    assert_eq!(snap.group_name, "Movie Night");
    assert_eq!(snap.queue_item_ids, vec![item]);
}

/// A fresh snapshot that does not parse is no group: `get_or_create` would
/// fail on it, which read as a registry outage.
#[actix_web::test]
async fn joining_a_fresh_but_corrupt_snapshot_says_group_does_not_exist() {
    let mut rig = rig_with(replica(false)).await;
    let gid = GroupId::new();
    store_snapshot(&rig.stores, gid, "not json", 60).await;
    assert_eq!(join!(rig, gid), 204);

    assert!(
        next_matching(&mut rig.rx, |m| matches!(m, ServerMsg::GroupDoesNotExist))
            .await
            .is_some()
    );
    assert!(rig.registry.list().await.unwrap().is_empty());
}

/// A snapshot's `updated_at` only moves on mutation, so a party paused for
/// days is stale in the store while its owner replica still runs it. Another
/// replica holding the lease makes the group joinable (a remote handle), with
/// or without a snapshot.
#[actix_web::test]
async fn joining_a_group_live_on_another_replica_is_admitted_whatever_its_snapshot_age() {
    let (stale, json) =
        owner_snapshot("Movie Night", &pharos_jellyfin_api::dto::wire_item_id(7)).await;
    for snapshot_age in [Some(3 * 24 * 3600), None] {
        let mut rig = rig_with(replica(true)).await;
        let gid = if let Some(age) = snapshot_age {
            store_snapshot(&rig.stores, stale, &json, age).await;
            stale
        } else {
            GroupId::new()
        };
        assert_eq!(join!(rig, gid), 204);

        assert!(
            next_matching(&mut rig.rx, |m| matches!(m, ServerMsg::GroupDoesNotExist))
                .await
                .is_none(),
            "a group held by another replica is live (snapshot {snapshot_age:?})"
        );
        assert!(rig.registry.get(gid).await.unwrap().is_some());
    }
}

/// A user who may not see a group's queue is answered exactly as for a group
/// that does not exist (Jellyfin's `LibraryAccessDenied` would confirm it
/// exists), and the probe changes nothing.
#[actix_web::test]
async fn a_denied_join_is_indistinguishable_from_a_nonexistent_group() {
    let hidden_item = pharos_jellyfin_api::dto::wire_item_id(999);
    let mut rig = rig().await;
    rig.stores
        .set_policy(
            rig.uid,
            UserPolicy {
                enable_all_folders: false,
                enabled_folders: vec![],
                ..UserPolicy::default()
            },
        )
        .await
        .unwrap();
    let live = rig.registry.create().await.unwrap();
    live.tx
        .send(GroupMsg::SeedQueue {
            item_id: hidden_item,
            runtime_ms: None,
            position_ms: 0,
            is_paused: true,
        })
        .await
        .unwrap();
    live.snapshot().await.unwrap();

    let absent = GroupId::new();
    let mut answers = Vec::new();
    for gid in [live.group_id, absent] {
        let status = join!(rig, gid);
        let told = next_matching(&mut rig.rx, |m| {
            matches!(
                m,
                ServerMsg::GroupDoesNotExist | ServerMsg::NotInGroup | ServerMsg::Joined { .. }
            )
        })
        .await;
        answers.push((status, format!("{told:?}")));
    }
    assert_eq!(
        answers[0], answers[1],
        "hidden and absent must answer alike"
    );
    assert_eq!(answers[0].1, "Some(GroupDoesNotExist)");
    assert_eq!(live.snapshot().await.unwrap().member_count, 0);
    assert_eq!(rig.registry.list().await.unwrap().len(), 1);
    assert!(rig.registry.get(absent).await.unwrap().is_none());
}

/// The permission check precedes hydration: a refused probe of a group held
/// only by a snapshot does not spin its actor up.
#[actix_web::test]
async fn a_denied_join_of_a_persisted_group_does_not_hydrate_it() {
    let (gid, json) = owner_snapshot("Secret", &pharos_jellyfin_api::dto::wire_item_id(999)).await;
    let mut rig = rig_with(replica(false)).await;
    store_snapshot(&rig.stores, gid, &json, 60).await;
    rig.stores
        .set_policy(
            rig.uid,
            UserPolicy {
                enable_all_folders: false,
                enabled_folders: vec![],
                ..UserPolicy::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(join!(rig, gid), 204);

    assert!(
        next_matching(&mut rig.rx, |m| matches!(m, ServerMsg::GroupDoesNotExist))
            .await
            .is_some()
    );
    assert!(rig.registry.list().await.unwrap().is_empty());
    assert!(rig.registry.get(gid).await.unwrap().is_none());
}

/// A snapshot past the recovery window is not a group any more.
#[actix_web::test]
async fn joining_a_stale_persisted_group_says_group_does_not_exist() {
    let mut rig = rig().await;
    let gid = GroupId::new();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    rig.stores
        .upsert_sync_group(
            &PersistedSyncGroup {
                group_id: gid.to_string(),
                epoch_unix_ms: 1_000,
                state_json: "{}".into(),
                updated_at: 0,
            },
            now - 3 * 24 * 3600,
        )
        .await
        .unwrap();
    assert_eq!(join!(rig, gid), 204);

    assert!(
        next_matching(&mut rig.rx, |m| matches!(m, ServerMsg::GroupDoesNotExist))
            .await
            .is_some()
    );
    assert!(rig.registry.get(gid).await.unwrap().is_none());
}
