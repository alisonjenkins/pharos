//! Phase B4.3d integration: two in-process "replicas" sharing one bus + one
//! (fake) store model the multi-replica Postgres deployment. Proves the whole
//! loop: a member joining on a NON-owner replica is forwarded to the owner, the
//! owner's broadcast is delivered back to that member across the bus, and after
//! the owner drains a reconnect on the other replica takes over from the last
//! persisted snapshot.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use pharos_sync::bus::LocalSyncBus;
use pharos_sync::bus_delivery::{spawn_ingress, BusDelivery, BusMsg};
use pharos_sync::distributed::{CommandSink, Distributed, HydrationSource, OwnershipSource};
use pharos_sync::group::{GroupHandle, GroupMsg, GroupSnapshot, RemoteCommand};
use pharos_sync::messages::{GroupId, MemberId, ServerMsg};
use pharos_sync::persistence::GroupPersistence;
use pharos_sync::{GroupRegistry, MemberSinks, SyncBus};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

// --- Fake store/bus shared across the two replicas --------------------------

/// Shared advisory-lock stand-in: at most one replica owns a group at a time.
#[derive(Clone, Default)]
struct FakeOwnerMap(Arc<Mutex<HashMap<GroupId, usize>>>);

struct FakeOwnership {
    map: FakeOwnerMap,
    me: usize,
}
impl OwnershipSource for FakeOwnership {
    fn try_own(&self, group_id: GroupId) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
        Box::pin(async move {
            let mut m = self.map.0.lock().unwrap();
            match m.get(&group_id) {
                Some(o) if *o != self.me => false,
                _ => {
                    m.insert(group_id, self.me);
                    true
                }
            }
        })
    }
    fn release(&self, group_id: GroupId) {
        let mut m = self.map.0.lock().unwrap();
        if m.get(&group_id) == Some(&self.me) {
            m.remove(&group_id);
        }
    }
}

/// Shared snapshot store backing both persistence (write) and hydration (read).
#[derive(Clone, Default)]
struct FakeStore(Arc<Mutex<HashMap<GroupId, (u64, String)>>>);
impl GroupPersistence for FakeStore {
    fn persist(&self, group_id: GroupId, epoch_unix_ms: u64, state_json: String) {
        self.0
            .lock()
            .unwrap()
            .insert(group_id, (epoch_unix_ms, state_json));
    }
    fn remove(&self, group_id: GroupId) {
        self.0.lock().unwrap().remove(&group_id);
    }
}
impl HydrationSource for FakeStore {
    fn load(
        &self,
        group_id: GroupId,
    ) -> Pin<Box<dyn Future<Output = Option<(u64, String)>> + Send + '_>> {
        let got = self.0.lock().unwrap().get(&group_id).cloned();
        Box::pin(async move { got })
    }
}

/// Forwards a non-owner's command onto the shared bus as a `BusMsg::Command`.
struct FakeCommands {
    bus: Arc<LocalSyncBus>,
}
impl CommandSink for FakeCommands {
    fn submit(&self, group_id: GroupId, cmd: RemoteCommand) {
        if let Ok(payload) = serde_json::to_string(&BusMsg::Command { group_id, cmd }) {
            let bus = self.bus.clone();
            tokio::spawn(async move {
                let _ = bus.publish(payload).await;
            });
        }
    }
}

/// One replica's wiring: a distributed registry + member sinks + both bus
/// ingresses (delivery → local sinks; command → owned actor).
struct Replica {
    registry: GroupRegistry,
    sinks: MemberSinks,
}

fn spawn_replica(
    bus: Arc<LocalSyncBus>,
    owners: FakeOwnerMap,
    store: FakeStore,
    me: usize,
) -> Replica {
    let sinks = MemberSinks::new();
    let delivery = Arc::new(BusDelivery::new(bus.clone(), sinks.clone()));
    let distributed = Distributed {
        ownership: Arc::new(FakeOwnership { map: owners, me }),
        hydration: Arc::new(store.clone()),
        commands: Arc::new(FakeCommands { bus: bus.clone() }),
        persistence: Arc::new(store),
    };
    let registry = GroupRegistry::spawn_distributed(delivery, distributed);

    // Outbound: deliver bus broadcasts to this replica's local sinks.
    spawn_ingress(bus.as_ref(), sinks.clone());

    // Inbound: apply bus-forwarded commands to this replica's owned actors.
    let mut cmd_rx = bus.subscribe();
    let reg2 = registry.clone();
    tokio::spawn(async move {
        loop {
            match cmd_rx.recv().await {
                Ok(payload) => {
                    if let Ok(BusMsg::Command { group_id, cmd }) = serde_json::from_str(&payload) {
                        let _ = reg2.deliver_command(group_id, cmd).await;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    });

    Replica { registry, sinks }
}

/// Register a member's sink and AddMember through `handle` (owned or remote).
async fn join(
    handle: &GroupHandle,
    sinks: &MemberSinks,
    member: MemberId,
    name: &str,
) -> mpsc::Receiver<ServerMsg> {
    let (tx, rx) = mpsc::channel(64);
    sinks.insert(member, tx);
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .tx
        .send(GroupMsg::AddMember {
            member_id: member,
            name: name.into(),
            reply: reply_tx,
        })
        .await
        .unwrap();
    let _ = reply_rx.await;
    rx
}

async fn recv_matching(
    rx: &mut mpsc::Receiver<ServerMsg>,
    pred: impl Fn(&ServerMsg) -> bool,
) -> Option<ServerMsg> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(m)) if pred(&m) => return Some(m),
            Ok(Some(_)) => continue,
            _ => return None,
        }
    }
}

async fn snapshot(h: &GroupHandle) -> Option<GroupSnapshot> {
    let (tx, rx) = oneshot::channel();
    h.tx.send(GroupMsg::Snapshot { reply: tx }).await.ok()?;
    rx.await.ok()
}

#[tokio::test]
async fn command_from_non_owner_reaches_members_on_both_replicas() {
    let bus = Arc::new(LocalSyncBus::new());
    let owners = FakeOwnerMap::default();
    let store = FakeStore::default();
    let a = spawn_replica(bus.clone(), owners.clone(), store.clone(), 1);
    let b = spawn_replica(bus.clone(), owners.clone(), store.clone(), 2);
    // Let both replicas' ingresses subscribe.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Replica A creates the group → A owns it.
    let handle_a = a.registry.create().await.unwrap();
    let gid = handle_a.group_id;
    let m_a = MemberId::new();
    let mut rx_a = join(&handle_a, &a.sinks, m_a, "ali").await;

    // Replica B joins the SAME group → B gets a remote handle (A owns it).
    let handle_b = b.registry.get_or_create(gid).await.unwrap();
    let m_b = MemberId::new();
    let mut rx_b = join(&handle_b, &b.sinks, m_b, "gf").await;
    // The remote AddMember must have reached the owner — give the bus a beat.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Leader (m_a, on A) sets the queue; both members report Ready → the group
    // starts and Play is broadcast to BOTH replicas' members over the bus.
    handle_a
        .tx
        .send(GroupMsg::SetNewQueue {
            sender: m_a,
            item_ids: vec!["ep1".into()],
            playing_index: 0,
            start_position_ms: 0,
        })
        .await
        .unwrap();
    handle_a
        .tx
        .send(GroupMsg::MemberReady {
            member_id: m_a,
            position_ms: 0,
            playlist_item_id: None,
        })
        .await
        .unwrap();
    // m_b's Ready is issued through the REMOTE handle on B → forwarded to A.
    handle_b
        .tx
        .send(GroupMsg::MemberReady {
            member_id: m_b,
            position_ms: 0,
            playlist_item_id: None,
        })
        .await
        .unwrap();

    let a_play = recv_matching(&mut rx_a, |m| matches!(m, ServerMsg::Play { .. })).await;
    let b_play = recv_matching(&mut rx_b, |m| matches!(m, ServerMsg::Play { .. })).await;
    assert!(a_play.is_some(), "owner-replica member missed Play");
    assert!(
        b_play.is_some(),
        "non-owner-replica member missed Play — cross-replica loop broken"
    );
}

#[tokio::test]
async fn group_survives_owner_drain_via_takeover() {
    let bus = Arc::new(LocalSyncBus::new());
    let owners = FakeOwnerMap::default();
    let store = FakeStore::default();
    let a = spawn_replica(bus.clone(), owners.clone(), store.clone(), 1);
    tokio::time::sleep(Duration::from_millis(30)).await;

    // A owns a group with a member + a queue.
    let handle_a = a.registry.create().await.unwrap();
    let gid = handle_a.group_id;
    let m_a = MemberId::new();
    let _rx_a = join(&handle_a, &a.sinks, m_a, "ali").await;
    handle_a
        .tx
        .send(GroupMsg::SetGroupName {
            name: "Movie Night".into(),
        })
        .await
        .unwrap();
    // Ensure the snapshot persisted.
    let _ = snapshot(&handle_a).await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        store.0.lock().unwrap().contains_key(&gid),
        "owner must have persisted the group snapshot"
    );

    // Replica A "drains": release its ownership lease (its advisory lock would
    // free when the pod dies). The snapshot stays in the shared store.
    owners.0.lock().unwrap().remove(&gid);

    // Replica B comes up and a member reconnects into the group → B wins
    // ownership and hydrates from the snapshot.
    let b = spawn_replica(bus.clone(), owners.clone(), store.clone(), 2);
    tokio::time::sleep(Duration::from_millis(30)).await;
    let handle_b = b.registry.get_or_create(gid).await.unwrap();
    let snap = snapshot(&handle_b)
        .await
        .expect("B must own a live actor after takeover");
    assert_eq!(snap.id, gid);
    assert_eq!(
        snap.group_name, "Movie Night",
        "group state survived the owner drain via hydration"
    );
    assert_eq!(snap.member_count, 1, "the roster survived takeover");
}

/// B33 — the boot-reconciliation retry primitive: while another replica (the
/// DRAINING old pod during a rolling deploy) still holds a group's advisory
/// lock, `get_or_create` yields a REMOTE handle — detectable because
/// `snapshot()` answers only from a local actor. Once the lock frees, a
/// retried `get_or_create` must win ownership and hydrate a LOCAL actor
/// (snapshot answers). Without the retry, a group nobody touched again ran
/// with NO actor at all: no ghost prune, no dissolution, snapshot squatting.
#[tokio::test]
async fn get_or_create_upgrades_remote_handle_to_owned_after_lock_frees() {
    let bus = Arc::new(LocalSyncBus::new());
    let owners = FakeOwnerMap::default();
    let store = FakeStore::default();

    // "Old pod" (replica 2) owns the group and persisted a snapshot.
    let r2 = spawn_replica(bus.clone(), owners.clone(), store.clone(), 2);
    let h2 = r2.registry.create().await.unwrap();
    let gid = h2.group_id;
    let m = MemberId::new();
    let (tx, _rx) = mpsc::channel(8);
    r2.sinks.insert(m, tx);
    let (rtx, rrx) = oneshot::channel();
    h2.tx
        .send(GroupMsg::AddMember {
            member_id: m,
            name: "alison".into(),
            reply: rtx,
        })
        .await
        .unwrap();
    let _ = rrx.await.unwrap();

    // New pod (replica 1) boots while the old one still holds the lock:
    // reconciliation's first attempt yields a REMOTE handle.
    let r1 = spawn_replica(bus.clone(), owners.clone(), store.clone(), 1);
    let first = r1.registry.get_or_create(gid).await.unwrap();
    assert!(
        first.snapshot().await.is_none(),
        "while the old owner holds the lock, the handle must be remote"
    );

    // The old pod finishes draining: its lock releases (B26 keeps the
    // snapshot intact — nothing removed it).
    owners.0.lock().unwrap().remove(&gid);

    // Retry (what spawn_boot_reconciliation now does each round): ownership
    // is won and the LOCAL actor hydrates from the snapshot.
    let second = r1.registry.get_or_create(gid).await.unwrap();
    let snap = second
        .snapshot()
        .await
        .expect("after the lock frees, a retried get_or_create must own + hydrate");
    assert_eq!(snap.member_count, 1, "roster hydrated from the snapshot");
}
