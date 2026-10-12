//! End-to-end group-watch protocol test at the engine+hub level: two simulated
//! sessions (two `ServerMsg` sinks registered in the [`SessionHub`], exactly as
//! the `/socket` task does) driven through the same `GroupMsg`/registry calls
//! the HTTP `/SyncPlay/*` handlers make. Proves the readiness gate, the
//! anti-wedge timeout, and the join-read-only invariant that stops a late
//! joiner skipping everyone to the next episode.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use pharos_sync::group::{GroupHandle, GroupMsg};
use pharos_sync::messages::{MemberId, ServerMsg};
use pharos_sync::{GroupRegistry, LocalDelivery, MemberSinks, SessionHub};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// A hub + registry + shared `MemberSinks` wired exactly as the single-replica
/// server does: the registry's groups deliver into the same sink table the
/// `/socket` layer registers member sinks into.
fn wiring() -> (SessionHub, GroupRegistry, MemberSinks) {
    let hub = SessionHub::new();
    let sinks = MemberSinks::new();
    let registry = GroupRegistry::spawn(Arc::new(LocalDelivery::new(sinks.clone())));
    (hub, registry, sinks)
}

/// A simulated client: a member id + the receiving end of its sink.
struct Client {
    device: String,
    member_id: MemberId,
    rx: mpsc::Receiver<ServerMsg>,
}

/// Register a session in the hub (as the `/socket` task does) and return the
/// client handle. The sink is what the group engine broadcasts to.
fn connect(hub: &SessionHub, device: &str, name: &str) -> Client {
    let (tx, rx) = mpsc::channel(64);
    let reg = hub.register(device.to_string(), name.to_string(), tx);
    Client {
        device: device.to_string(),
        member_id: reg.member_id,
        rx,
    }
}

/// Add a hub-registered session to `handle` — mirrors the HTTP New/Join
/// handler's `add_caller_to_group`: register the sink into the replica's
/// `MemberSinks`, then `AddMember`.
async fn add_to_group(hub: &SessionHub, sinks: &MemberSinks, handle: &GroupHandle, device: &str) {
    let sess = hub.resolve(device).unwrap();
    hub.attach_group(device, handle.clone());
    sinks.insert(sess.member_id, sess.conn_gen, sess.sink);
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .tx
        .send(GroupMsg::AddMember {
            member_id: sess.member_id,
            name: sess.name,
            reply: reply_tx,
        })
        .await
        .unwrap();
    reply_rx.await.unwrap();
}

/// Drain a client's sink until a message matching `pred` arrives (or time out).
async fn recv_until<F>(c: &mut Client, pred: F) -> Option<ServerMsg>
where
    F: Fn(&ServerMsg) -> bool,
{
    loop {
        // Strictly longer than the engine's READY_TIMEOUT so that under a paused
        // clock (auto-advance picks the EARLIEST timer) the gate deadline fires
        // before this receive gives up. Passing paths return immediately.
        let budget = Duration::from_millis(pharos_sync::group::READY_TIMEOUT_MS + 5_000);
        match tokio::time::timeout(budget, c.rx.recv()).await {
            Ok(Some(msg)) => {
                if pred(&msg) {
                    return Some(msg);
                }
            }
            _ => return None,
        }
    }
}

fn is_play(m: &ServerMsg) -> bool {
    matches!(m, ServerMsg::Play { .. })
}

#[tokio::test]
async fn two_sessions_start_in_lockstep_after_both_ready() {
    let (hub, registry, sinks) = wiring();

    let mut a = connect(&hub, "devA", "ali");
    let mut b = connect(&hub, "devB", "gf");

    // A creates the group (leader) and both join — the HTTP New/Join flow.
    let handle = registry.create().await.unwrap();
    add_to_group(&hub, &sinks, &handle, &a.device).await;
    add_to_group(&hub, &sinks, &handle, &b.device).await;

    // A (leader) sets the queue → both get a PlayQueue and enter Waiting.
    handle
        .tx
        .send(GroupMsg::SetNewQueue {
            sender: a.member_id,
            item_ids: vec!["ep1".into(), "ep2".into()],
            item_runtimes_ms: Vec::new(),
            playing_index: 0,
            start_position_ms: 0,
        })
        .await
        .unwrap();

    let a_queue = recv_until(&mut a, |m| matches!(m, ServerMsg::PlayQueue { .. })).await;
    let b_queue = recv_until(&mut b, |m| matches!(m, ServerMsg::PlayQueue { .. })).await;
    assert!(a_queue.is_some() && b_queue.is_some(), "both get PlayQueue");

    // Neither should have started yet — the gate holds the Play.
    // Both report Ready → the gate resolves and both receive Play together.
    handle
        .tx
        .send(GroupMsg::MemberReady {
            member_id: a.member_id,
            position_ms: 0,
            playlist_item_id: None,
            is_playing: false,
        })
        .await
        .unwrap();
    handle
        .tx
        .send(GroupMsg::MemberReady {
            member_id: b.member_id,
            position_ms: 0,
            playlist_item_id: None,
            is_playing: false,
        })
        .await
        .unwrap();

    let a_play = recv_until(&mut a, is_play).await;
    let b_play = recv_until(&mut b, is_play).await;
    assert!(
        matches!(a_play, Some(ServerMsg::Play { position_ms: 0, .. })),
        "leader gets Play at position 0, got {a_play:?}"
    );
    assert!(
        matches!(b_play, Some(ServerMsg::Play { position_ms: 0, .. })),
        "follower gets Play at position 0, got {b_play:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn silent_member_does_not_wedge_the_group() {
    // Only one of two members reports Ready. The readiness-gate timeout must
    // fire so the group starts anyway — a silent client can't block playback.
    // `start_paused` auto-advances tokio's clock to the gate deadline, so the
    // 5 s timeout resolves instantly instead of stalling the test.
    let (hub, registry, sinks) = wiring();
    let mut a = connect(&hub, "devA", "ali");
    let mut b = connect(&hub, "devB", "gf");

    let handle = registry.create().await.unwrap();
    add_to_group(&hub, &sinks, &handle, &a.device).await;
    add_to_group(&hub, &sinks, &handle, &b.device).await;

    handle
        .tx
        .send(GroupMsg::SetNewQueue {
            sender: a.member_id,
            item_ids: vec!["ep1".into()],
            item_runtimes_ms: Vec::new(),
            playing_index: 0,
            start_position_ms: 0,
        })
        .await
        .unwrap();

    // Only A readies; B stays silent. The timeout must still start both.
    handle
        .tx
        .send(GroupMsg::MemberReady {
            member_id: a.member_id,
            position_ms: 0,
            playlist_item_id: None,
            is_playing: false,
        })
        .await
        .unwrap();

    assert!(
        recv_until(&mut a, is_play).await.is_some(),
        "A starts after the readiness timeout"
    );
    assert!(
        recv_until(&mut b, is_play).await.is_some(),
        "silent B is started too, not left wedged"
    );
}

#[tokio::test]
async fn late_joiner_does_not_advance_the_queue() {
    // The "a joiner skips everyone to the next episode" guard: a member joining
    // mid-playback receives the CURRENT queue position, never advances it, and
    // cannot advance it (non-leader NextItem is rejected).
    let (hub, registry, sinks) = wiring();
    let mut a = connect(&hub, "devA", "ali");
    let mut b = connect(&hub, "devB", "gf");

    let handle = registry.create().await.unwrap();
    add_to_group(&hub, &sinks, &handle, &a.device).await;
    add_to_group(&hub, &sinks, &handle, &b.device).await;

    handle
        .tx
        .send(GroupMsg::SetNewQueue {
            sender: a.member_id,
            item_ids: vec!["ep1".into(), "ep2".into()],
            item_runtimes_ms: Vec::new(),
            playing_index: 0,
            start_position_ms: 0,
        })
        .await
        .unwrap();
    // Both ready → group plays item 0 (no need to wait out the gate timeout).
    for m in [a.member_id, b.member_id] {
        handle
            .tx
            .send(GroupMsg::MemberReady {
                member_id: m,
                position_ms: 0,
                playlist_item_id: None,
                is_playing: false,
            })
            .await
            .unwrap();
    }
    for c in [&mut a, &mut b] {
        recv_until(c, is_play).await; // both playing item 0
    }

    // A third member joins mid-playback.
    let mut c = connect(&hub, "devC", "friend");
    add_to_group(&hub, &sinks, &handle, &c.device).await;

    // The joiner gets the queue at the CURRENT index (0), not advanced.
    let q = recv_until(&mut c, |m| matches!(m, ServerMsg::PlayQueue { .. })).await;
    match q {
        Some(ServerMsg::PlayQueue { playing_index, .. }) => {
            assert_eq!(
                playing_index, 0,
                "joiner must see the current item, not next"
            );
        }
        other => panic!("joiner should receive PlayQueue, got {other:?}"),
    }

    // Shared control (Jellyfin default): ANY member may advance the queue — the
    // join above was passive (didn't advance), but an EXPLICIT NextItem from any
    // member now moves the whole group to item 1.
    handle
        .tx
        .send(GroupMsg::NextItem {
            sender: c.member_id,
            playlist_item_id: None,
        })
        .await
        .unwrap();
    let adv = recv_until(&mut a, |m| {
        matches!(
            m,
            ServerMsg::PlayQueue {
                playing_index: 1,
                ..
            }
        )
    })
    .await;
    assert!(
        adv.is_some(),
        "an explicit NextItem from any member advances the group to item 1"
    );
}

#[tokio::test]
async fn socket_reconnect_keeps_membership_and_resyncs() {
    // The critical robustness case: a member's /socket drops and reconnects
    // (jellyfin-web churns its socket constantly). Membership must SURVIVE — the
    // hub keeps the member id + group, and an UpdateSink re-points the group at
    // the new socket + re-syncs it. Without this, commands broadcast to a dead
    // sink and playback never syncs.
    let (hub, registry, sinks) = wiring();
    let mut a = connect(&hub, "devA", "ali");
    let mut b = connect(&hub, "devB", "gf");

    let handle = registry.create().await.unwrap();
    add_to_group(&hub, &sinks, &handle, &a.device).await;
    add_to_group(&hub, &sinks, &handle, &b.device).await;
    handle
        .tx
        .send(GroupMsg::SetNewQueue {
            sender: a.member_id,
            item_ids: vec!["ep1".into()],
            item_runtimes_ms: Vec::new(),
            playing_index: 0,
            start_position_ms: 0,
        })
        .await
        .unwrap();
    for m in [a.member_id, b.member_id] {
        handle
            .tx
            .send(GroupMsg::MemberReady {
                member_id: m,
                position_ms: 0,
                playlist_item_id: None,
                is_playing: false,
            })
            .await
            .unwrap();
    }
    for c in [&mut a, &mut b] {
        assert!(recv_until(c, is_play).await.is_some(), "both playing");
    }

    // B's socket reconnects: SAME device, a NEW sink. The hub returns the
    // existing member id + group (not a fresh member); the socket then re-points
    // the group at its new sink: re-register the fresh sink in the replica's
    // MemberSinks, then ResyncMember re-sends the catch-up.
    let (new_tx, mut new_rx) = mpsc::channel(64);
    let reg = hub.register("devB".to_string(), "gf".to_string(), new_tx);
    assert_eq!(
        reg.member_id, b.member_id,
        "member id stable across reconnect"
    );
    let group = reg.group.expect("reconnect sees the existing group");
    let s = hub.resolve("devB").unwrap();
    sinks.insert(b.member_id, s.conn_gen, s.sink);
    group
        .tx
        .send(GroupMsg::ResyncMember {
            member_id: b.member_id,
        })
        .await
        .unwrap();
    // A leader command now reaches B's NEW sink.
    handle
        .tx
        .send(GroupMsg::SeekTo {
            sender: a.member_id,
            position_ms: 30_000,
        })
        .await
        .unwrap();
    // B38 — a Ready before the Seek's scheduled at_server_ms (MIN_LEAD after
    // broadcast) is treated as a spurious player transition and ignored;
    // wait past it so these Readys count as real ACKs.
    tokio::time::sleep(Duration::from_millis(400)).await;
    handle
        .tx
        .send(GroupMsg::MemberReady {
            member_id: a.member_id,
            position_ms: 30_000,
            playlist_item_id: None,
            is_playing: false,
        })
        .await
        .unwrap();
    handle
        .tx
        .send(GroupMsg::MemberReady {
            member_id: b.member_id,
            position_ms: 30_000,
            playlist_item_id: None,
            is_playing: false,
        })
        .await
        .unwrap();
    let got = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match new_rx.recv().await {
                Some(ServerMsg::Play { position_ms, .. }) if position_ms >= 30_000 => return true,
                Some(_) => continue,
                None => return false,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(got, "reconnected socket receives the group's live commands");
}

// --- Persisted member kind (spec 009) --------------------------------------

/// Shared single-slot snapshot store: persistence writes it, hydration reads it.
#[derive(Clone, Default)]
struct Slot(Arc<std::sync::Mutex<Option<(u64, String)>>>);

impl pharos_sync::GroupPersistence for Slot {
    fn persist(&self, _g: pharos_sync::GroupId, epoch: u64, json: String) {
        *self.0.lock().unwrap() = Some((epoch, json));
    }
    fn remove(&self, _g: pharos_sync::GroupId) {}
}

impl pharos_sync::HydrationSource for Slot {
    fn load(&self, _g: pharos_sync::GroupId) -> pharos_sync::distributed::LoadFuture<'_> {
        let got = self.0.lock().unwrap().clone();
        Box::pin(async move { got })
    }
}

struct AlwaysOwn;
impl pharos_sync::OwnershipSource for AlwaysOwn {
    fn try_own(&self, _g: pharos_sync::GroupId) -> pharos_sync::distributed::OwnFuture<'_> {
        Box::pin(async { true })
    }
    fn release(&self, _g: pharos_sync::GroupId) {}
}

struct NoCommands;
impl pharos_sync::CommandSink for NoCommands {
    fn submit(&self, _g: pharos_sync::GroupId, _c: pharos_sync::group::RemoteCommand) {}
}

fn registry_over(slot: &Slot) -> GroupRegistry {
    GroupRegistry::spawn_distributed(
        Arc::new(LocalDelivery::new(MemberSinks::new())),
        pharos_sync::Distributed {
            ownership: Arc::new(AlwaysOwn),
            hydration: Arc::new(slot.clone()),
            commands: Arc::new(NoCommands),
            persistence: Arc::new(slot.clone()),
        },
    )
}

async fn add(h: &GroupHandle, member: MemberId, name: &str) {
    let (tx, rx) = oneshot::channel();
    h.tx.send(GroupMsg::AddMember {
        member_id: member,
        name: name.into(),
        reply: tx,
    })
    .await
    .unwrap();
    let _ = rx.await;
}

/// Force a persist and return the member entries of the latest snapshot.
async fn persisted_members(h: &GroupHandle, slot: &Slot, tag: &str) -> Vec<serde_json::Value> {
    h.tx.send(GroupMsg::SetGroupName { name: tag.into() })
        .await
        .unwrap();
    for _ in 0..100 {
        let json = slot.0.lock().unwrap().clone().map(|(_, j)| j);
        if let Some(json) = json.filter(|j| j.contains(tag)) {
            let v: serde_json::Value = serde_json::from_str(&json).unwrap();
            return v["members"].as_array().cloned().unwrap();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("snapshot with group name {tag} was never persisted");
}

#[tokio::test]
async fn persisted_snapshot_without_kind_loads_as_player() {
    let slot = Slot::default();
    let a = registry_over(&slot);
    let h = a.create().await.unwrap();
    let gid = h.group_id;
    add(&h, MemberId::new(), "ali").await;
    let _ = persisted_members(&h, &slot, "before").await;

    // Strip the new fields: this is the shape an older replica wrote.
    let (epoch, json) = slot.0.lock().unwrap().clone().unwrap();
    let mut v: serde_json::Value = serde_json::from_str(&json).unwrap();
    for m in v["members"].as_array_mut().unwrap() {
        let o = m.as_object_mut().unwrap();
        o.remove("kind");
        o.remove("cast");
    }
    *slot.0.lock().unwrap() = Some((epoch, v.to_string()));

    let b = registry_over(&slot);
    let hb = b.get_or_create(gid).await.unwrap();
    let members = persisted_members(&hb, &slot, "after").await;
    assert_eq!(members.len(), 1, "old snapshot must still hydrate");
    assert_eq!(
        members[0]["kind"], "Player",
        "missing kind defaults to Player"
    );
}

#[tokio::test]
async fn cast_seat_kind_round_trips_through_a_snapshot() {
    use pharos_sync::group::{CastBinding, MemberKind};
    let slot = Slot::default();
    let a = registry_over(&slot);
    let h = a.create().await.unwrap();
    let gid = h.group_id;
    let seat = MemberId::new();
    add(&h, MemberId::new(), "ali").await;
    add(&h, seat, "Living Room").await;
    h.tx.send(GroupMsg::SetMemberKind {
        member: seat,
        kind: MemberKind::CastSeat,
        cast: Some(CastBinding {
            tv: "Living Room".into(),
            owner: "ali".into(),
        }),
    })
    .await
    .unwrap();
    let _ = persisted_members(&h, &slot, "before").await;

    let b = registry_over(&slot);
    let hb = b.get_or_create(gid).await.unwrap();
    let members = persisted_members(&hb, &slot, "after").await;
    let seats: Vec<_> = members.iter().filter(|m| m["kind"] == "CastSeat").collect();
    assert_eq!(
        seats.len(),
        1,
        "the seat must survive hydration: {members:?}"
    );
    assert_eq!(seats[0]["cast"]["tv"], "Living Room");
    assert_eq!(seats[0]["cast"]["owner"], "ali");
    assert!(members.iter().any(|m| m["kind"] == "Player"));
}
