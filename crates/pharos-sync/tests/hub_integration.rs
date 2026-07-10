//! End-to-end group-watch protocol test at the engine+hub level: two simulated
//! sessions (two `ServerMsg` sinks registered in the [`SessionHub`], exactly as
//! the `/socket` task does) driven through the same `GroupMsg`/registry calls
//! the HTTP `/SyncPlay/*` handlers make. Proves the readiness gate, the
//! anti-wedge timeout, and the join-read-only invariant that stops a late
//! joiner skipping everyone to the next episode.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use pharos_sync::group::{GroupHandle, GroupMsg};
use pharos_sync::messages::{MemberId, ServerMsg};
use pharos_sync::{GroupRegistry, SessionHub};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

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
    let member_id = MemberId::new();
    hub.register(device.to_string(), member_id, name.to_string(), tx);
    Client {
        device: device.to_string(),
        member_id,
        rx,
    }
}

/// Add a hub-registered session to `handle` — mirrors the HTTP New/Join
/// handler's `add_caller_to_group`.
async fn add_to_group(hub: &SessionHub, handle: &GroupHandle, device: &str) {
    let sess = hub.resolve(device).unwrap();
    hub.attach_group(device, handle.clone());
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .tx
        .send(GroupMsg::AddMember {
            member_id: sess.member_id,
            name: sess.name,
            sink: sess.sink,
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
        // Longer than the engine's READY_TIMEOUT so that under a paused clock
        // (auto-advance picks the EARLIEST timer) the gate deadline fires before
        // this receive gives up. Passing paths return immediately regardless.
        match tokio::time::timeout(Duration::from_secs(30), c.rx.recv()).await {
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
    let hub = SessionHub::new();
    let registry = GroupRegistry::spawn();

    let mut a = connect(&hub, "devA", "ali");
    let mut b = connect(&hub, "devB", "gf");

    // A creates the group (leader) and both join — the HTTP New/Join flow.
    let handle = registry.create().await.unwrap();
    add_to_group(&hub, &handle, &a.device).await;
    add_to_group(&hub, &handle, &b.device).await;

    // A (leader) sets the queue → both get a PlayQueue and enter Waiting.
    handle
        .tx
        .send(GroupMsg::SetNewQueue {
            sender: a.member_id,
            item_ids: vec!["ep1".into(), "ep2".into()],
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
        })
        .await
        .unwrap();
    handle
        .tx
        .send(GroupMsg::MemberReady {
            member_id: b.member_id,
            position_ms: 0,
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
    let hub = SessionHub::new();
    let registry = GroupRegistry::spawn();
    let mut a = connect(&hub, "devA", "ali");
    let mut b = connect(&hub, "devB", "gf");

    let handle = registry.create().await.unwrap();
    add_to_group(&hub, &handle, &a.device).await;
    add_to_group(&hub, &handle, &b.device).await;

    handle
        .tx
        .send(GroupMsg::SetNewQueue {
            sender: a.member_id,
            item_ids: vec!["ep1".into()],
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
    let hub = SessionHub::new();
    let registry = GroupRegistry::spawn();
    let mut a = connect(&hub, "devA", "ali");
    let mut b = connect(&hub, "devB", "gf");

    let handle = registry.create().await.unwrap();
    add_to_group(&hub, &handle, &a.device).await;
    add_to_group(&hub, &handle, &b.device).await;

    handle
        .tx
        .send(GroupMsg::SetNewQueue {
            sender: a.member_id,
            item_ids: vec!["ep1".into(), "ep2".into()],
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
            })
            .await
            .unwrap();
    }
    for c in [&mut a, &mut b] {
        recv_until(c, is_play).await; // both playing item 0
    }

    // A third member joins mid-playback.
    let mut c = connect(&hub, "devC", "friend");
    add_to_group(&hub, &handle, &c.device).await;

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

    // The joiner (non-leader) cannot advance the group.
    handle
        .tx
        .send(GroupMsg::NextItem {
            sender: c.member_id,
        })
        .await
        .unwrap();
    let err = recv_until(&mut c, |m| matches!(m, ServerMsg::Error { .. })).await;
    assert!(
        matches!(err, Some(ServerMsg::Error { .. })),
        "non-leader NextItem is rejected, got {err:?}"
    );
}
