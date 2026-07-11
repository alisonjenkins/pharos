#![allow(clippy::unwrap_used, clippy::expect_used)]
//! V19 / V3 conformance — measure synthetic position drift across N
//! group members after a single Play broadcast. Each member computes
//! its expected playback position from the anchor in `ServerMsg::Play`
//! plus its own wall-clock; drift is the deviation between two
//! members' computed positions at the same sample time.
//!
//! V3 says group-sync syncs play/pause/seek across members within
//! 500ms p95. With the anchor-based propagation pharos uses, that
//! invariant is the maths on the wire — every member receiving the
//! same `at_server_ms` + `position_ms` reconverges to identical
//! perceived positions modulo their `(local_clock - server_clock)`
//! offset, which pharos already nails down via the WS offset
//! estimator.
//!
//! This test exercises the propagation half: every member must
//! receive the Play with the SAME anchor (no per-member rebase),
//! and the computed positions across a 5-second sample window must
//! stay within the V3 budget.

use pharos_sync::group::{GroupHandle, GroupMsg, Joined};
use pharos_sync::messages::{GroupId, MemberId, ServerMsg};
use pharos_sync::{LocalDelivery, MemberSinks};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

fn spawn_group() -> (GroupHandle, MemberSinks) {
    let sinks = MemberSinks::new();
    let h = GroupHandle::spawn(GroupId::new(), Arc::new(LocalDelivery::new(sinks.clone())));
    (h, sinks)
}

async fn add_member(
    h: &GroupHandle,
    sinks: &MemberSinks,
    name: &str,
) -> (MemberId, mpsc::Receiver<ServerMsg>) {
    let (tx, rx) = mpsc::channel(64);
    let mid = MemberId::new();
    sinks.insert(mid, tx);
    let (reply_tx, reply_rx) = oneshot::channel();
    h.tx.send(GroupMsg::AddMember {
        member_id: mid,
        name: name.into(),
        reply: reply_tx,
    })
    .await
    .unwrap();
    let _: Joined = reply_rx.await.unwrap();
    (mid, rx)
}

async fn drain_until_play(rx: &mut mpsc::Receiver<ServerMsg>) -> Option<(u64, u64)> {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(ServerMsg::Play {
                at_server_ms,
                position_ms,
            })) => return Some((at_server_ms, position_ms)),
            Ok(Some(_)) => continue,
            _ => return None,
        }
    }
}

/// V3 — every member receives the same Play anchor + position_ms, so
/// the drift across all members at any sample wall-time is 0 ms.
/// p95 < 500 ms holds trivially when every member's `at_server_ms`
/// matches the leader's.
#[tokio::test]
async fn play_anchor_propagates_with_zero_drift_across_five_members() {
    let (h, sinks) = spawn_group();
    let (leader, _leader_rx) = add_member(&h, &sinks, "leader").await;
    let (_, mut m2_rx) = add_member(&h, &sinks, "m2").await;
    let (_, mut m3_rx) = add_member(&h, &sinks, "m3").await;
    let (_, mut m4_rx) = add_member(&h, &sinks, "m4").await;
    let (_, mut m5_rx) = add_member(&h, &sinks, "m5").await;

    // Leader fires Play at position 0.
    h.tx.send(GroupMsg::LeaderPlay {
        sender: leader,
        position_ms: 0,
    })
    .await
    .unwrap();

    let m2 = drain_until_play(&mut m2_rx).await.expect("m2 missed Play");
    let m3 = drain_until_play(&mut m3_rx).await.expect("m3 missed Play");
    let m4 = drain_until_play(&mut m4_rx).await.expect("m4 missed Play");
    let m5 = drain_until_play(&mut m5_rx).await.expect("m5 missed Play");

    // Every member sees the same anchor — drift is zero by construction.
    assert_eq!(m2.0, m3.0, "m2.at_server_ms != m3.at_server_ms");
    assert_eq!(m3.0, m4.0);
    assert_eq!(m4.0, m5.0);
    assert_eq!(m2.1, m3.1, "position_ms must match across members");
    assert_eq!(m3.1, m4.1);
    assert_eq!(m4.1, m5.1);

    // Now sample the computed positions at +1s, +3s, +5s of wall time.
    // The expected position at sample time T (server_ms) is:
    //   expected_pos = position_ms + (T - at_server_ms)
    // Every member uses the same anchor, so their computed positions
    // are identical. p95 drift across the cohort = 0.
    let (anchor, pos) = m2;
    let samples = [1000_u64, 3000, 5000];
    for delta in samples {
        let t = anchor + delta;
        let expected = pos + (t - anchor);
        let cohort: Vec<u64> = [m2, m3, m4, m5].iter().map(|(a, p)| p + (t - a)).collect();
        for c in &cohort {
            assert_eq!(*c, expected, "drift > 0 at t={t}");
        }
        // p95 across the cohort is the max — assert under V3's 500ms.
        let drift_ms =
            cohort.iter().copied().max().unwrap_or(0) - cohort.iter().copied().min().unwrap_or(0);
        assert!(drift_ms < 500, "p95 drift {drift_ms}ms exceeds V3 budget");
    }
}

/// V3 — Seek mid-play also propagates with zero anchor drift. Same
/// shape as the Play test but after a Seek broadcast.
#[tokio::test]
async fn seek_anchor_propagates_with_zero_drift() {
    let (h, sinks) = spawn_group();
    let (leader, _leader_rx) = add_member(&h, &sinks, "leader").await;
    let (_, mut m2_rx) = add_member(&h, &sinks, "m2").await;
    let (_, mut m3_rx) = add_member(&h, &sinks, "m3").await;

    h.tx.send(GroupMsg::LeaderPlay {
        sender: leader,
        position_ms: 0,
    })
    .await
    .unwrap();
    let _ = drain_until_play(&mut m2_rx).await;
    let _ = drain_until_play(&mut m3_rx).await;

    // Seek to position 30_000 ms.
    let seek_position_ms: u64 = 30_000;
    h.tx.send(GroupMsg::LeaderSeek {
        sender: leader,
        position_ms: seek_position_ms,
    })
    .await
    .unwrap();

    let m2 = drain_until_seek(&mut m2_rx).await.expect("m2 missed Seek");
    let m3 = drain_until_seek(&mut m3_rx).await.expect("m3 missed Seek");

    assert_eq!(m2.0, m3.0, "Seek at_server_ms must match");
    assert_eq!(m2.1, m3.1, "Seek position_ms must match");
    assert_eq!(m2.1, seek_position_ms);
}

async fn drain_until_seek(rx: &mut mpsc::Receiver<ServerMsg>) -> Option<(u64, u64)> {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(ServerMsg::Seek {
                at_server_ms,
                position_ms,
            })) => return Some((at_server_ms, position_ms)),
            Ok(Some(_)) => continue,
            _ => return None,
        }
    }
}
