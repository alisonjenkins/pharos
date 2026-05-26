#![allow(clippy::unwrap_used, clippy::expect_used)]
//! V19 conformance harness (T43). Each test simulates one of Jellyfin's
//! SyncPlay failure modes against pharos's group actor and asserts the
//! pharos-specific fix holds:
//!
//! - late-joiner: a member who joins mid-playback receives the current
//!   play/pause state immediately, not on the next leader command.
//! - slow-member: a wedged member's full sink does not block the actor;
//!   other members continue to receive Play/Pause/Seek in real time.
//! - leader-handoff: when the leader leaves the group, the lowest-id
//!   surviving member is elected and broadcast to everyone.
//! - network-blip: a member who drops and rejoins picks up the current
//!   playback state on rejoin.

use pharos_server::sync::group::{GroupHandle, GroupMsg, GroupSnapshot, Joined, MIN_LEAD_MS};
use pharos_server::sync::messages::{GroupId, MemberId, ServerMsg};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

async fn add_member_with_cap(
    h: &GroupHandle,
    name: &str,
    cap: usize,
) -> (MemberId, mpsc::Receiver<ServerMsg>) {
    let (tx, rx) = mpsc::channel(cap);
    let mid = MemberId::new();
    let (reply_tx, reply_rx) = oneshot::channel();
    h.tx.send(GroupMsg::AddMember {
        member_id: mid,
        name: name.into(),
        sink: tx,
        reply: reply_tx,
    })
    .await
    .unwrap();
    let _: Joined = reply_rx.await.unwrap();
    (mid, rx)
}

async fn add_member(
    h: &GroupHandle,
    name: &str,
) -> (MemberId, mpsc::Receiver<ServerMsg>) {
    add_member_with_cap(h, name, 64).await
}

async fn snapshot(h: &GroupHandle) -> GroupSnapshot {
    let (tx, rx) = oneshot::channel();
    h.tx.send(GroupMsg::Snapshot { reply: tx }).await.unwrap();
    rx.await.unwrap()
}

async fn drain(rx: &mut mpsc::Receiver<ServerMsg>) {
    while rx.try_recv().is_ok() {}
}

/// Collect server messages from a receiver until either `predicate`
/// returns true on one or the timeout elapses. Returns the matched
/// message or `None`.
async fn wait_for<F>(
    rx: &mut mpsc::Receiver<ServerMsg>,
    timeout: Duration,
    mut predicate: F,
) -> Option<ServerMsg>
where
    F: FnMut(&ServerMsg) -> bool,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(m)) => {
                if predicate(&m) {
                    return Some(m);
                }
            }
            Ok(None) | Err(_) => return None,
        }
    }
}

/// V19 — late joiner catches up to current play state without waiting
/// on the leader to re-issue Play.
#[tokio::test]
async fn late_joiner_receives_current_play_state() {
    let h = GroupHandle::spawn(GroupId::new());
    let (leader, mut leader_rx) = add_member(&h, "leader").await;
    let (_m2, _m2_rx) = add_member(&h, "m2").await;

    // Leader plays.
    h.tx.send(GroupMsg::LeaderPlay {
        sender: leader,
        position_ms: 30_000,
    })
    .await
    .unwrap();
    drain(&mut leader_rx).await;

    // Third member joins after a tick.
    tokio::time::sleep(Duration::from_millis(20)).await;
    let (_late, mut late_rx) = add_member(&h, "late").await;

    let msg = wait_for(&mut late_rx, Duration::from_millis(500), |m| {
        matches!(m, ServerMsg::Play { .. })
    })
    .await
    .expect("late joiner did not receive Play");
    match msg {
        ServerMsg::Play { position_ms, .. } => {
            // Position must be >= 30s (the leader's anchor) and within
            // a generous slack window — wall-clock between LeaderPlay
            // and AddMember should be ~20 ms.
            assert!(
                (30_000..30_500).contains(&position_ms),
                "expected ~30000ms, got {position_ms}",
            );
        }
        other => panic!("expected Play, got {other:?}"),
    }
}

/// V19 — late joiner who finds the group paused receives a Seek (to
/// the freeze position) followed by Pause.
#[tokio::test]
async fn late_joiner_receives_pause_state_when_group_paused() {
    let h = GroupHandle::spawn(GroupId::new());
    let (leader, mut leader_rx) = add_member(&h, "leader").await;
    h.tx.send(GroupMsg::LeaderPlay {
        sender: leader,
        position_ms: 10_000,
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    h.tx.send(GroupMsg::LeaderPause { sender: leader })
        .await
        .unwrap();
    drain(&mut leader_rx).await;

    let (_late, mut late_rx) = add_member(&h, "late").await;
    // Should see at least one Seek followed by Pause.
    let seek = wait_for(&mut late_rx, Duration::from_millis(500), |m| {
        matches!(m, ServerMsg::Seek { .. })
    })
    .await;
    assert!(seek.is_some(), "expected Seek for paused late joiner");
    let pause = wait_for(&mut late_rx, Duration::from_millis(500), |m| {
        matches!(m, ServerMsg::Pause { .. })
    })
    .await;
    assert!(pause.is_some(), "expected Pause for paused late joiner");
}

/// V19 — a wedged member with a full sink must not block broadcasts
/// to everyone else. The actor uses `try_send` and drops messages on
/// a full sink (the laggard reconciles on next state catch-up).
#[tokio::test]
async fn slow_member_does_not_block_broadcasts() {
    let h = GroupHandle::spawn(GroupId::new());
    let (leader, mut leader_rx) = add_member(&h, "leader").await;
    // Member 2 has capacity 1 and we never read from it — sink fills
    // immediately and any further send would block under the old
    // implementation.
    let (_slow, mut slow_rx) = add_member_with_cap(&h, "slow", 1).await;
    let (_m3, mut m3_rx) = add_member(&h, "m3").await;

    drain(&mut leader_rx).await;
    drain(&mut m3_rx).await;
    drain(&mut slow_rx).await;

    // Issue Play.
    h.tx.send(GroupMsg::LeaderPlay {
        sender: leader,
        position_ms: 0,
    })
    .await
    .unwrap();
    // Issue many follow-up commands to ensure the slow sink overflows
    // — under the previous `send().await` path this would deadlock
    // the actor and m3 would never see anything.
    for i in 0..50 {
        h.tx.send(GroupMsg::LeaderSeek {
            sender: leader,
            position_ms: (i + 1) * 1000,
        })
        .await
        .unwrap();
    }

    // Leader + m3 must still receive their broadcasts. We allow up to
    // 500 ms for the actor to process the queue.
    let leader_got = wait_for(&mut leader_rx, Duration::from_millis(500), |m| {
        matches!(m, ServerMsg::Seek { .. })
    })
    .await;
    let m3_got = wait_for(&mut m3_rx, Duration::from_millis(500), |m| {
        matches!(m, ServerMsg::Seek { .. })
    })
    .await;
    assert!(leader_got.is_some(), "actor wedged: leader received nothing");
    assert!(m3_got.is_some(), "actor wedged: m3 received nothing");
}

/// V19 — leader handoff: lowest-id surviving member is elected and
/// every survivor receives LeaderChange + MemberLeft. Existing unit
/// test covers the happy path; this conformance test exercises the
/// 3-member case with explicit assertions on each survivor's stream.
#[tokio::test]
async fn leader_handoff_broadcasts_to_all_survivors() {
    let h = GroupHandle::spawn(GroupId::new());
    let (leader, _leader_rx) = add_member(&h, "leader").await;
    let (m2, mut m2_rx) = add_member(&h, "m2").await;
    let (m3, mut m3_rx) = add_member(&h, "m3").await;
    drain(&mut m2_rx).await;
    drain(&mut m3_rx).await;

    h.tx.send(GroupMsg::RemoveMember { member_id: leader })
        .await
        .unwrap();

    let expected_new_leader = std::cmp::min(m2, m3);
    let lc2 = wait_for(&mut m2_rx, Duration::from_millis(500), |m| {
        matches!(m, ServerMsg::LeaderChange { .. })
    })
    .await
    .expect("m2 no LeaderChange");
    let lc3 = wait_for(&mut m3_rx, Duration::from_millis(500), |m| {
        matches!(m, ServerMsg::LeaderChange { .. })
    })
    .await
    .expect("m3 no LeaderChange");
    for m in [lc2, lc3] {
        match m {
            ServerMsg::LeaderChange { leader } => assert_eq!(leader, expected_new_leader),
            other => panic!("expected LeaderChange, got {other:?}"),
        }
    }
    let snap = snapshot(&h).await;
    assert_eq!(snap.leader, Some(expected_new_leader));
    assert_eq!(snap.member_count, 2);
}

/// V19 — network blip: member drops, rejoins. On rejoin pharos
/// delivers the current play state without leader intervention.
#[tokio::test]
async fn network_blip_member_rejoins_to_current_play_state() {
    let h = GroupHandle::spawn(GroupId::new());
    let (leader, mut leader_rx) = add_member(&h, "leader").await;
    let (m2, _m2_rx) = add_member(&h, "m2").await;

    h.tx.send(GroupMsg::LeaderPlay {
        sender: leader,
        position_ms: 5_000,
    })
    .await
    .unwrap();
    drain(&mut leader_rx).await;

    // m2 "drops".
    h.tx.send(GroupMsg::RemoveMember { member_id: m2 })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    // m2 rejoins — issuing a fresh MemberId because each WS reconnect
    // mints a new one (matches Jellyfin behaviour).
    let (_m2_again, mut m2_again_rx) = add_member(&h, "m2-reconnect").await;
    let msg = wait_for(&mut m2_again_rx, Duration::from_millis(500), |m| {
        matches!(m, ServerMsg::Play { .. })
    })
    .await
    .expect("rejoiner no Play");
    match msg {
        ServerMsg::Play { position_ms, .. } => assert!(
            (5_000..5_500).contains(&position_ms),
            "rejoiner saw stale position {position_ms}",
        ),
        other => panic!("expected Play, got {other:?}"),
    }
}

/// V19 — leader-only invariant survives handoff. After the original
/// leader leaves, the old leader's LeaderPlay must be rejected (it's
/// no longer a member at all) but the new leader's must succeed.
#[tokio::test]
async fn after_handoff_old_leader_cannot_issue_play() {
    let h = GroupHandle::spawn(GroupId::new());
    let (leader, _leader_rx) = add_member(&h, "leader").await;
    let (m2, mut m2_rx) = add_member(&h, "m2").await;

    h.tx.send(GroupMsg::RemoveMember { member_id: leader })
        .await
        .unwrap();
    drain(&mut m2_rx).await;

    h.tx.send(GroupMsg::LeaderPlay {
        sender: leader,
        position_ms: 0,
    })
    .await
    .unwrap();
    // m2 (the new leader) does NOT receive a Play because the old
    // leader's command was rejected. We give the actor a beat then
    // assert no Play arrived.
    let got = wait_for(&mut m2_rx, Duration::from_millis(150), |m| {
        matches!(m, ServerMsg::Play { .. })
    })
    .await;
    assert!(got.is_none(), "departed leader still issuing Play");

    // The new leader's Play DOES go through.
    h.tx.send(GroupMsg::LeaderPlay {
        sender: m2,
        position_ms: 1234,
    })
    .await
    .unwrap();
    let _ = MIN_LEAD_MS; // make sure the import isn't dead
    let m = wait_for(&mut m2_rx, Duration::from_millis(500), |m| {
        matches!(m, ServerMsg::Play { .. })
    })
    .await
    .expect("new leader's Play not broadcast");
    match m {
        ServerMsg::Play { position_ms, .. } => assert_eq!(position_ms, 1234),
        other => panic!("expected Play, got {other:?}"),
    }
}
