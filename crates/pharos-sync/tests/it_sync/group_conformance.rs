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

use pharos_sync::group::{
    CastBinding, GroupHandle, GroupMsg, GroupSnapshot, Joined, MemberKind, MIN_LEAD_MS,
};
use pharos_sync::messages::{GroupId, MemberId, ServerMsg};
use pharos_sync::{LocalDelivery, MemberSinks};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// Spawn a group delivering to an in-process `MemberSinks` (the single-replica
/// wiring). Returns the sinks so tests register each member before `AddMember`.
fn spawn_group() -> (GroupHandle, MemberSinks) {
    let sinks = MemberSinks::new();
    let h = GroupHandle::spawn(GroupId::new(), Arc::new(LocalDelivery::new(sinks.clone())));
    (h, sinks)
}

async fn add_member_with_cap(
    h: &GroupHandle,
    sinks: &MemberSinks,
    name: &str,
    cap: usize,
) -> (MemberId, mpsc::Receiver<ServerMsg>) {
    let (tx, rx) = mpsc::channel(cap);
    let mid = MemberId::new();
    sinks.insert(mid, 1, tx);
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

async fn add_member(
    h: &GroupHandle,
    sinks: &MemberSinks,
    name: &str,
) -> (MemberId, mpsc::Receiver<ServerMsg>) {
    add_member_with_cap(h, sinks, name, 64).await
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
    let (h, sinks) = spawn_group();
    let (leader, mut leader_rx) = add_member(&h, &sinks, "leader").await;
    let (_m2, _m2_rx) = add_member(&h, &sinks, "m2").await;

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
    let (_late, mut late_rx) = add_member(&h, &sinks, "late").await;

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

/// V19 — late joiner who finds the group paused receives a Pause carrying
/// the freeze position (jellyfin-web's schedulePause seeks to the command's
/// PositionTicks, so one positioned Pause replaces the old Seek+Pause pair —
/// and survives the client's single-slot pre-time-sync command queue).
#[tokio::test]
async fn late_joiner_receives_pause_state_when_group_paused() {
    let (h, sinks) = spawn_group();
    let (leader, mut leader_rx) = add_member(&h, &sinks, "leader").await;
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

    let (_late, mut late_rx) = add_member(&h, &sinks, "late").await;
    // Should see a Pause carrying the freeze position (~10s + the 20ms of
    // play time before the pause).
    let pause = wait_for(&mut late_rx, Duration::from_millis(500), |m| {
        matches!(m, ServerMsg::Pause { .. })
    })
    .await;
    match pause {
        Some(ServerMsg::Pause { position_ms, .. }) => assert!(
            (10_000..10_500).contains(&position_ms),
            "Pause must carry the freeze position (~10000ms), got {position_ms}"
        ),
        other => panic!("expected Pause for paused late joiner, got {other:?}"),
    }
}

/// V19 — a wedged member with a full sink must not block broadcasts
/// to everyone else. The actor uses `try_send` and drops messages on
/// a full sink (the laggard reconciles on next state catch-up).
#[tokio::test]
async fn slow_member_does_not_block_broadcasts() {
    let (h, sinks) = spawn_group();
    let (leader, mut leader_rx) = add_member(&h, &sinks, "leader").await;
    // Member 2 has capacity 1 and we never read from it — sink fills
    // immediately and any further send would block under the old
    // implementation.
    let (_slow, mut slow_rx) = add_member_with_cap(&h, &sinks, "slow", 1).await;
    let (_m3, mut m3_rx) = add_member(&h, &sinks, "m3").await;

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
    assert!(
        leader_got.is_some(),
        "actor wedged: leader received nothing"
    );
    assert!(m3_got.is_some(), "actor wedged: m3 received nothing");
}

/// V19 — leader handoff: lowest-id surviving member is elected and
/// every survivor receives LeaderChange + MemberLeft. Existing unit
/// test covers the happy path; this conformance test exercises the
/// 3-member case with explicit assertions on each survivor's stream.
#[tokio::test]
async fn leader_handoff_broadcasts_to_all_survivors() {
    let (h, sinks) = spawn_group();
    let (leader, _leader_rx) = add_member(&h, &sinks, "leader").await;
    let (m2, mut m2_rx) = add_member(&h, &sinks, "m2").await;
    let (m3, mut m3_rx) = add_member(&h, &sinks, "m3").await;
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
    let (h, sinks) = spawn_group();
    let (leader, mut leader_rx) = add_member(&h, &sinks, "leader").await;
    let (m2, _m2_rx) = add_member(&h, &sinks, "m2").await;

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
    let (_m2_again, mut m2_again_rx) = add_member(&h, &sinks, "m2-reconnect").await;
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
    let (h, sinks) = spawn_group();
    let (leader, _leader_rx) = add_member(&h, &sinks, "leader").await;
    let (m2, mut m2_rx) = add_member(&h, &sinks, "m2").await;

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

async fn set_kind(h: &GroupHandle, member: MemberId, kind: MemberKind, cast: Option<CastBinding>) {
    h.tx.send(GroupMsg::SetMemberKind { member, kind, cast })
        .await
        .unwrap();
}

fn is_play(m: &ServerMsg) -> bool {
    matches!(m, ServerMsg::Play { .. })
}

fn is_pause(m: &ServerMsg) -> bool {
    matches!(m, ServerMsg::Pause { .. })
}

/// V139/V161 — a Controller and a CastSeat never ack, so neither may hold a
/// queue-change or Seek gate; both stay in the roster and still hear the group.
#[tokio::test]
async fn non_player_members_never_hold_a_gate_but_stay_in_the_roster() {
    let (h, sinks) = spawn_group();
    let (leader, mut leader_rx) = add_member(&h, &sinks, "leader").await;
    let (p2, mut p2_rx) = add_member(&h, &sinks, "p2").await;
    let (seat, mut seat_rx) = add_member(&h, &sinks, "tv").await;
    let (ctrl, mut ctrl_rx) = add_member(&h, &sinks, "phone").await;
    let cast = CastBinding {
        tv: "Living Room".into(),
        owner: "ali".into(),
    };
    set_kind(&h, seat, MemberKind::CastSeat, Some(cast.clone())).await;
    set_kind(&h, ctrl, MemberKind::Controller, Some(cast)).await;

    h.tx.send(GroupMsg::SetNewQueue {
        sender: leader,
        item_ids: vec!["ep1".into()],
        item_runtimes_ms: Vec::new(),
        playing_index: 0,
        start_position_ms: 0,
    })
    .await
    .unwrap();
    for member_id in [leader, p2] {
        h.tx.send(GroupMsg::MemberReady {
            member_id,
            position_ms: 0,
            playlist_item_id: None,
            is_playing: false,
        })
        .await
        .unwrap();
    }
    assert!(
        wait_for(&mut leader_rx, Duration::from_millis(800), is_play)
            .await
            .is_some(),
        "queue-change gate must complete on the Player acks alone"
    );

    h.tx.send(GroupMsg::SeekTo {
        sender: leader,
        position_ms: 30_000,
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    for member_id in [leader, p2] {
        h.tx.send(GroupMsg::MemberReady {
            member_id,
            position_ms: 30_000,
            playlist_item_id: None,
            is_playing: false,
        })
        .await
        .unwrap();
    }
    assert!(
        wait_for(&mut leader_rx, Duration::from_millis(800), is_play)
            .await
            .is_some(),
        "seek gate must complete on the Player acks alone"
    );

    for rx in [&mut seat_rx, &mut ctrl_rx, &mut p2_rx] {
        assert!(
            wait_for(rx, Duration::from_millis(500), |m| matches!(
                m,
                ServerMsg::Seek { .. }
            ))
            .await
            .is_some(),
            "every member, whatever its kind, receives the Seek"
        );
    }
    let snap = snapshot(&h).await;
    assert_eq!(snap.member_count, 4);
    // The phone and the TV share a cast, so they list as one name.
    assert!(snap.participants.iter().any(|p| p == "phone"));
    assert_eq!(snap.participants.len(), 3);
}

/// V139 — a buffering report from a non-player must neither freeze the group
/// nor stay behind as a holdout that blocks the Player's recovery.
#[tokio::test]
async fn non_player_buffering_is_not_a_freeze_wait_target() {
    let (h, sinks) = spawn_group();
    let (leader, mut leader_rx) = add_member(&h, &sinks, "leader").await;
    let (p2, _p2_rx) = add_member(&h, &sinks, "p2").await;
    let (seat, _seat_rx) = add_member(&h, &sinks, "tv").await;
    set_kind(&h, seat, MemberKind::CastSeat, None).await;

    h.tx.send(GroupMsg::LeaderPlay {
        sender: leader,
        position_ms: 10_000,
    })
    .await
    .unwrap();
    drain(&mut leader_rx).await;

    h.tx.send(GroupMsg::BufferingStart {
        member_id: seat,
        position_ms: 10_000,
        playlist_item_id: None,
    })
    .await
    .unwrap();
    assert!(
        wait_for(&mut leader_rx, Duration::from_millis(300), is_pause)
            .await
            .is_none(),
        "a CastSeat buffering report must not freeze the group"
    );

    h.tx.send(GroupMsg::BufferingStart {
        member_id: p2,
        position_ms: 10_000,
        playlist_item_id: None,
    })
    .await
    .unwrap();
    assert!(
        wait_for(&mut leader_rx, Duration::from_millis(500), is_pause)
            .await
            .is_some(),
        "a Player buffering still freezes the group"
    );
    h.tx.send(GroupMsg::BufferingEnd { member_id: p2 })
        .await
        .unwrap();
    assert!(
        wait_for(&mut leader_rx, Duration::from_millis(800), is_play)
            .await
            .is_some(),
        "the freeze must lift without waiting on the CastSeat"
    );
}

/// A channel-attached adapter sink (what a Cast seat uses) must see exactly
/// the message stream a websocket-style sink sees for pause / unpause / seek.
#[tokio::test]
async fn channel_attached_sink_receives_the_same_stream_as_a_socket_sink() {
    let (h, sinks) = spawn_group();
    let (leader, mut leader_rx) = add_member(&h, &sinks, "leader").await;
    let (ws, mut ws_rx) = add_member(&h, &sinks, "socket").await;
    let adapter = MemberId::new();
    let mut adapter_rx = sinks.attach_channel(adapter, 1, 64);
    let (reply, joined) = oneshot::channel();
    h.tx.send(GroupMsg::AddMember {
        member_id: adapter,
        name: "tv".into(),
        reply,
    })
    .await
    .unwrap();
    let _: Joined = joined.await.unwrap();
    set_kind(&h, adapter, MemberKind::CastSeat, None).await;
    let _ = ws;

    h.tx.send(GroupMsg::LeaderPlay {
        sender: leader,
        position_ms: 5_000,
    })
    .await
    .unwrap();
    h.tx.send(GroupMsg::PauseShared { sender: leader })
        .await
        .unwrap();
    h.tx.send(GroupMsg::Unpause { sender: leader })
        .await
        .unwrap();
    h.tx.send(GroupMsg::SeekTo {
        sender: leader,
        position_ms: 20_000,
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    fn kinds(rx: &mut mpsc::Receiver<ServerMsg>) -> Vec<&'static str> {
        let mut out = Vec::new();
        while let Ok(m) = rx.try_recv() {
            if matches!(
                m,
                ServerMsg::Play { .. } | ServerMsg::Pause { .. } | ServerMsg::Seek { .. }
            ) {
                out.push(m.kind());
            }
        }
        out
    }
    let socket = kinds(&mut ws_rx);
    let adapter = kinds(&mut adapter_rx);
    let _ = kinds(&mut leader_rx);
    assert!(
        socket.contains(&"pause") && socket.contains(&"seek"),
        "scenario must produce commands: {socket:?}"
    );
    assert_eq!(adapter, socket, "adapter and socket sinks diverged");
}

async fn join_roster(
    h: &GroupHandle,
    sinks: &MemberSinks,
    name: &str,
) -> (MemberId, Vec<pharos_sync::MemberSummary>) {
    let (tx, _rx) = mpsc::channel(64);
    let mid = MemberId::new();
    sinks.insert(mid, 1, tx);
    let (reply, joined) = oneshot::channel();
    h.tx.send(GroupMsg::AddMember {
        member_id: mid,
        name: name.into(),
        reply,
    })
    .await
    .unwrap();
    let joined: Joined = joined.await.unwrap();
    (mid, joined.members)
}

/// FR-005 — a user driving a TV has a Controller (their phone) and a CastSeat
/// (the TV) bound to the same cast. The roster shows ONE entry: the user, with
/// the TV as the device. It is a projection, so the raw member count is intact.
#[tokio::test]
async fn casting_user_appears_once_in_the_roster_with_the_tv_as_device() {
    let (h, sinks) = spawn_group();
    let (_bob, _) = join_roster(&h, &sinks, "bob").await;
    let (ctrl, _) = join_roster(&h, &sinks, "ali").await;
    let (seat, _) = join_roster(&h, &sinks, "Living Room").await;
    let cast = |owner: &str| {
        Some(CastBinding {
            tv: "Living Room".into(),
            owner: owner.into(),
        })
    };
    set_kind(&h, ctrl, MemberKind::Controller, cast("ali")).await;
    set_kind(&h, seat, MemberKind::CastSeat, cast("ali")).await;

    let (_carol, roster) = join_roster(&h, &sinks, "carol").await;
    let mut names: Vec<_> = roster.iter().map(|m| m.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["ali", "bob", "carol"], "roster: {roster:?}");
    let ali = roster.iter().find(|m| m.name == "ali").unwrap();
    assert_eq!(ali.device.as_deref(), Some("Living Room"));
    assert!(
        roster
            .iter()
            .filter(|m| m.name != "ali")
            .all(|m| m.device.is_none()),
        "ordinary members carry no device"
    );

    let snap = snapshot(&h).await;
    assert_eq!(
        snap.member_count, 4,
        "projection must not change the roster"
    );
    assert_eq!(snap.participants.len(), 3);
}

/// A seat whose Controller belongs to a different owner is a different cast:
/// nothing is collapsed.
#[tokio::test]
async fn roster_keeps_members_of_different_casts_apart() {
    let (h, sinks) = spawn_group();
    let (ctrl, _) = join_roster(&h, &sinks, "ali").await;
    let (seat, _) = join_roster(&h, &sinks, "Living Room").await;
    set_kind(
        &h,
        ctrl,
        MemberKind::Controller,
        Some(CastBinding {
            tv: "Living Room".into(),
            owner: "ali".into(),
        }),
    )
    .await;
    set_kind(
        &h,
        seat,
        MemberKind::CastSeat,
        Some(CastBinding {
            tv: "Living Room".into(),
            owner: "bob".into(),
        }),
    )
    .await;
    let (_c, roster) = join_roster(&h, &sinks, "carol").await;
    assert_eq!(roster.len(), 3, "roster: {roster:?}");
}

fn ordered_id(n: u128) -> MemberId {
    MemberId(uuid::Uuid::from_u128(n))
}

async fn add_member_with_id(
    h: &GroupHandle,
    sinks: &MemberSinks,
    id: MemberId,
    name: &str,
) -> mpsc::Receiver<ServerMsg> {
    let (tx, rx) = mpsc::channel(64);
    sinks.insert(id, 1, tx);
    let (reply, joined) = oneshot::channel();
    h.tx.send(GroupMsg::AddMember {
        member_id: id,
        name: name.into(),
        reply,
    })
    .await
    .unwrap();
    let _: Joined = joined.await.unwrap();
    rx
}

fn tv_cast() -> Option<CastBinding> {
    Some(CastBinding {
        tv: "Living Room".into(),
        owner: "ali".into(),
    })
}

/// T057 — a CastSeat with a lower id than a Player must lose the election:
/// when the leader leaves, the Player takes over, never the seat.
#[tokio::test]
async fn leader_removed_hands_over_to_a_player_not_a_lower_id_seat() {
    let (h, sinks) = spawn_group();
    let leader = ordered_id(10);
    let seat = ordered_id(11);
    let player = ordered_id(12);
    let _l = add_member_with_id(&h, &sinks, leader, "leader").await;
    let _s = add_member_with_id(&h, &sinks, seat, "tv").await;
    let mut p_rx = add_member_with_id(&h, &sinks, player, "player").await;
    set_kind(&h, seat, MemberKind::CastSeat, tv_cast()).await;

    h.tx.send(GroupMsg::RemoveMember { member_id: leader })
        .await
        .unwrap();
    let change = wait_for(&mut p_rx, Duration::from_millis(500), |m| {
        matches!(m, ServerMsg::LeaderChange { .. })
    })
    .await;
    assert!(
        matches!(change, Some(ServerMsg::LeaderChange { leader }) if leader == player),
        "expected LeaderChange to the Player, got {change:?}"
    );
    assert_eq!(snapshot(&h).await.leader, Some(player));
}

/// T057 — re-classifying the current leader as a non-Player moves leadership
/// to a connected Player and announces it.
#[tokio::test]
async fn reclassifying_the_leader_as_a_seat_re_elects_a_player() {
    let (h, sinks) = spawn_group();
    let seat = ordered_id(20);
    let player = ordered_id(21);
    let _s = add_member_with_id(&h, &sinks, seat, "tv").await;
    let mut p_rx = add_member_with_id(&h, &sinks, player, "player").await;
    assert_eq!(snapshot(&h).await.leader, Some(seat));
    drain(&mut p_rx).await;

    set_kind(&h, seat, MemberKind::CastSeat, tv_cast()).await;
    let change = wait_for(&mut p_rx, Duration::from_millis(500), |m| {
        matches!(m, ServerMsg::LeaderChange { .. })
    })
    .await;
    assert!(
        matches!(change, Some(ServerMsg::LeaderChange { leader }) if leader == player),
        "expected LeaderChange to the Player, got {change:?}"
    );
    assert_eq!(snapshot(&h).await.leader, Some(player));
}

/// T057 — a non-Player that leads (no Player was around) yields as soon as a
/// Player joins.
#[tokio::test]
async fn a_joining_player_takes_leadership_from_a_non_player() {
    let (h, sinks) = spawn_group();
    let ctrl = ordered_id(30);
    let _c = add_member_with_id(&h, &sinks, ctrl, "phone").await;
    set_kind(&h, ctrl, MemberKind::Controller, None).await;
    assert_eq!(snapshot(&h).await.leader, Some(ctrl));

    let player = ordered_id(31);
    let _p = add_member_with_id(&h, &sinks, player, "player").await;
    assert_eq!(snapshot(&h).await.leader, Some(player));
}

/// T057 — with no Player at all the group still has a leader.
#[tokio::test]
async fn an_all_non_player_group_still_has_a_leader() {
    let (h, sinks) = spawn_group();
    let a = ordered_id(40);
    let b = ordered_id(41);
    let _a = add_member_with_id(&h, &sinks, a, "phone").await;
    let _b = add_member_with_id(&h, &sinks, b, "tv").await;
    set_kind(&h, a, MemberKind::Controller, None).await;
    set_kind(&h, b, MemberKind::CastSeat, None).await;
    // `a` yielded to the still-Player `b` first; `b` then became a seat with
    // nobody left to yield to, so it keeps the lead.
    assert_eq!(snapshot(&h).await.leader, Some(b));

    h.tx.send(GroupMsg::RemoveMember { member_id: b })
        .await
        .unwrap();
    assert_eq!(snapshot(&h).await.leader, Some(a));
}
