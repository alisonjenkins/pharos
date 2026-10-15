//! 009 US3 over HTTP: leaving the group ends the leaver's cast first, and a
//! user holds at most one seat.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "common/cast_rig.rs"]
mod cast_rig;

use cast_rig::{counter, drain, eventually, wait_msg, CastRig};
use pharos_cast::obs::ConnState;
use pharos_sync::group::GroupMsg;
use pharos_sync::messages::ServerMsg;
use std::time::Duration;

fn seat_total(event: &str, reason: &str) -> u64 {
    counter(
        "pharos_cast_seat_total",
        &[("event", event), ("reason", reason)],
    )
}

#[actix_web::test]
async fn the_owner_leaving_the_group_ends_the_cast_with_reason_owner_left() {
    let rig = CastRig::new().await;
    let (status, body) = rig.play_on_tv(&rig.alice).await;
    assert_eq!(status, 204, "{body}");
    assert_eq!(rig.service.snapshot().await.len(), 1);
    let before = seat_total("ended", "owner_left");
    let stop_before = seat_total("ended", "user_stop");

    let (status, body) = rig.post(&rig.alice, "/SyncPlay/Leave", None).await;
    assert_eq!(status, 204, "{body}");

    assert!(
        rig.service.snapshot().await.is_empty(),
        "the seat is gone by the time Leave answers"
    );
    assert_eq!(seat_total("ended", "owner_left"), before + 1);
    assert_eq!(
        seat_total("ended", "user_stop"),
        stop_before,
        "an owner Leave is not also counted as a user stop"
    );
}

#[actix_web::test]
async fn another_member_leaving_leaves_the_cast_running() {
    let rig = CastRig::new().await;
    assert_eq!(rig.play_on_tv(&rig.alice).await.0, 204);

    let (status, body) = rig.post(&rig.bob, "/SyncPlay/Leave", None).await;
    assert_eq!(status, 204, "{body}");
    assert_eq!(rig.service.snapshot().await.len(), 1);
}

#[actix_web::test]
async fn a_second_start_by_the_seat_owner_is_refused_with_user_has_seat() {
    let rig = CastRig::new().await;
    assert_eq!(rig.play_on_tv(&rig.alice).await.0, 204);
    let before = seat_total("rejected", "user_has_seat");

    let (status, body) = rig.play_on_tv(&rig.alice).await;
    assert_eq!(status, 409, "{body}");
    assert!(body.starts_with("user_has_seat"), "{body}");
    assert_eq!(seat_total("rejected", "user_has_seat"), before + 1);
    assert_eq!(rig.service.snapshot().await.len(), 1, "still one seat");
}

#[actix_web::test]
async fn the_owner_can_leave_rejoin_and_cast_again() {
    let rig = CastRig::new().await;
    assert_eq!(rig.play_on_tv(&rig.alice).await.0, 204);
    assert_eq!(rig.post(&rig.alice, "/SyncPlay/Leave", None).await.0, 204);
    assert!(rig.service.snapshot().await.is_empty());
    // A FakeReceiver serves one connection; the TV needs a fresh one.
    rig.factory.add_tv(cast_rig::TV);

    let (status, body) = rig
        .post(
            &rig.alice,
            "/SyncPlay/Join",
            Some(serde_json::json!({"GroupId": rig.group.group_id.0})),
        )
        .await;
    assert_eq!(status, 204, "{body}");
    let (status, body) = rig.play_on_tv(&rig.alice).await;
    assert_eq!(status, 204, "rejoined owner casts again: {body}");
    assert_eq!(rig.service.snapshot().await.len(), 1);
}

async fn rig_casting() -> (CastRig, String) {
    let rig = CastRig::new().await;
    let (status, body) = rig.play_on_tv(&rig.alice).await;
    assert_eq!(status, 204, "{body}");
    eventually("TV playing", || rig.receiver.state().playing).await;
    let token = rig.tv_token();
    assert_eq!(
        rig.token_status(&token).await,
        200,
        "token works while casting"
    );
    (rig, token)
}

#[actix_web::test]
async fn the_owner_stopping_on_the_tv_session_ends_the_cast_and_revokes_the_token() {
    let (rig, token) = rig_casting().await;
    let before = seat_total("ended", "user_stop");

    let (status, body) = rig
        .post(
            &rig.alice,
            &format!("/Sessions/{}/Playing/Stop", rig.tv_id),
            None,
        )
        .await;
    assert_eq!(status, 204, "{body}");

    assert!(rig.service.snapshot().await.is_empty());
    assert_eq!(seat_total("ended", "user_stop"), before + 1);
    assert_eq!(rig.token_status(&token).await, 401, "V175");
    eventually("the TV is told to stop", || {
        rig.receiver
            .commands()
            .iter()
            .any(|c| c.body.name() == "Stop")
    })
    .await;
    rig.assert_players_not_frozen(&[rig.bob.member, rig.alice.member])
        .await;
}

#[actix_web::test]
async fn a_tv_that_drops_off_the_network_past_the_grace_ends_the_cast() {
    let (rig, token) = rig_casting().await;
    let lost = seat_total("lost", "tv_unreachable");
    let ended = seat_total("ended", "grace_expired");

    rig.receiver.configure(|c| c.refuse_connect = true);
    rig.receiver.drop_connection();
    let started = tokio::time::Instant::now();
    eventually("the seat is marked lost", || {
        seat_total("lost", "tv_unreachable") > lost
    })
    .await;
    // While lost, the Player is not held by a seat that cannot ack.
    rig.assert_players_not_frozen(&[rig.bob.member]).await;

    rig.wait_cast_gone().await;
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "grace is 1 s; the end must not wait out anything longer"
    );
    assert_eq!(seat_total("ended", "grace_expired"), ended + 1);
    assert_eq!(rig.token_status(&token).await, 401, "V175");
    rig.assert_players_not_frozen(&[rig.bob.member, rig.alice.member])
        .await;
}

#[actix_web::test]
async fn a_tv_that_comes_back_inside_the_grace_keeps_its_seat_and_token() {
    let (rig, token) = rig_casting().await;
    let lost = seat_total("lost", "tv_unreachable");
    let back = seat_total("reconnected", "tv_unreachable");
    let ended = seat_total("ended", "grace_expired");

    rig.receiver.drop_connection();
    eventually("lost then reconnected", || {
        seat_total("lost", "tv_unreachable") > lost
            && seat_total("reconnected", "tv_unreachable") > back
    })
    .await;
    rig.wait_cast_state(ConnState::Following).await;

    assert_eq!(rig.service.snapshot().await.len(), 1);
    assert_eq!(seat_total("ended", "grace_expired"), ended);
    assert_eq!(rig.token_status(&token).await, 200, "token survives a blip");
    rig.assert_players_not_frozen(&[rig.bob.member]).await;
}

#[actix_web::test]
async fn a_dissolved_group_ends_the_cast_with_group_gone_and_revokes_the_token() {
    let (rig, token) = rig_casting().await;
    let before = seat_total("ended", "group_gone");

    for m in [rig.bob.member, rig.alice.member] {
        rig.group
            .tx
            .send(GroupMsg::RemoveMember { member_id: m })
            .await
            .unwrap();
    }
    let seat = rig.service.snapshot().await[0].seat_member;
    rig.group
        .tx
        .send(GroupMsg::RemoveMember { member_id: seat })
        .await
        .unwrap();
    rig.wait_cast_gone().await;

    assert_eq!(seat_total("ended", "group_gone"), before + 1);
    assert_eq!(rig.token_status(&token).await, 401, "V175");
}

fn restart_messages(msgs: &[ServerMsg]) -> Vec<&'static str> {
    msgs.iter()
        .filter(|m| {
            matches!(
                m,
                ServerMsg::Joined { .. } | ServerMsg::PlayQueue { .. } | ServerMsg::Play { .. }
            )
        })
        .map(ServerMsg::kind)
        .collect()
}

#[actix_web::test]
async fn ending_a_cast_catches_the_owner_up_so_its_local_player_restarts() {
    let (mut rig, _token) = rig_casting().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    drain(&mut rig.alice.rx);
    drain(&mut rig.bob.rx);

    let (status, body) = rig
        .post(
            &rig.alice,
            &format!("/Sessions/{}/Playing/Stop", rig.tv_id),
            None,
        )
        .await;
    assert_eq!(status, 204, "{body}");

    // The late-joiner catch-up is what jellyfin-web needs after its player was
    // stopped at hand-off: group state, the queue at the live position, Play.
    wait_msg(&mut rig.alice.rx, "Joined", |m| {
        matches!(m, ServerMsg::Joined { .. })
    })
    .await;
    let queue = wait_msg(&mut rig.alice.rx, "PlayQueue", |m| {
        matches!(m, ServerMsg::PlayQueue { .. })
    })
    .await;
    match queue {
        ServerMsg::PlayQueue {
            reason,
            items,
            start_position_ms,
            is_playing,
            ..
        } => {
            assert_eq!(reason, "user_joined");
            assert_eq!(
                items.first().map(|i| i.item_id.as_str()),
                Some(cast_rig::ITEM)
            );
            assert!(is_playing);
            assert!(
                (cast_rig::START_MS..cast_rig::START_MS + 30_000).contains(&start_position_ms),
                "restarts at the group position, got {start_position_ms}"
            );
        }
        other => panic!("{other:?}"),
    }
    wait_msg(&mut rig.alice.rx, "Play", |m| {
        matches!(m, ServerMsg::Play { .. })
    })
    .await;

    eventually("the TV session is closed in an orderly way", || {
        rig.receiver
            .commands()
            .iter()
            .any(|c| c.body.name() == "Stop")
    })
    .await;
    assert_eq!(
        restart_messages(&drain(&mut rig.bob.rx)),
        Vec::<&str>::new(),
        "other members are not re-synced"
    );
}

#[actix_web::test]
async fn an_owner_who_leaves_is_not_caught_up_into_the_group_it_is_leaving() {
    let (mut rig, _token) = rig_casting().await;
    drain(&mut rig.alice.rx);

    let (status, body) = rig.post(&rig.alice, "/SyncPlay/Leave", None).await;
    assert_eq!(status, 204, "{body}");

    wait_msg(&mut rig.alice.rx, "GroupLeft", |m| {
        matches!(m, ServerMsg::GroupLeft)
    })
    .await;
    assert_eq!(
        restart_messages(&drain(&mut rig.alice.rx)),
        Vec::<&str>::new()
    );
}

async fn roster_seen_by_bob(rig: &mut CastRig) -> Vec<pharos_sync::messages::MemberSummary> {
    drain(&mut rig.bob.rx);
    rig.group
        .tx
        .send(GroupMsg::ResyncMember {
            member_id: rig.bob.member,
        })
        .await
        .unwrap();
    match wait_msg(&mut rig.bob.rx, "Joined", |m| {
        matches!(m, ServerMsg::Joined { .. })
    })
    .await
    {
        ServerMsg::Joined { members, .. } => members,
        other => panic!("{other:?}"),
    }
}

#[actix_web::test]
async fn the_owners_socket_dropping_leaves_the_cast_following_and_the_owner_listed() {
    let (mut rig, _token) = rig_casting().await;
    rig.group
        .tx
        .send(GroupMsg::MemberSocketLost {
            member_id: rig.alice.member,
        })
        .await
        .unwrap();

    // The seat still follows the group without its owner.
    let (status, body) = rig
        .post(
            &rig.bob,
            &format!("/Sessions/{}/Playing/Pause", rig.tv_id),
            None,
        )
        .await;
    assert_eq!(status, 204, "{body}");
    eventually("TV paused by bob while the owner's socket is down", || {
        rig.receiver.state().paused
    })
    .await;
    assert_eq!(rig.service.snapshot().await.len(), 1);

    let roster = roster_seen_by_bob(&mut rig).await;
    let alice: Vec<_> = roster.iter().filter(|m| m.name == "alice").collect();
    assert_eq!(alice.len(), 1, "{roster:?}");
    assert_eq!(alice[0].device.as_deref(), Some(cast_rig::TV));

    // The owner's page reloads: a resync is a reconnect, not a new cast.
    rig.group
        .tx
        .send(GroupMsg::ResyncMember {
            member_id: rig.alice.member,
        })
        .await
        .unwrap();
    wait_msg(&mut rig.alice.rx, "Joined for the returning owner", |m| {
        matches!(m, ServerMsg::Joined { .. })
    })
    .await;
    let casts = rig.service.snapshot().await;
    assert_eq!(casts.len(), 1);
    assert_eq!(casts[0].owner, rig.alice.id);
}

#[actix_web::test]
async fn an_owner_pruned_after_its_socket_grace_takes_the_cast_with_it() {
    let (mut rig, token) = rig_casting().await;
    let before = seat_total("ended", "owner_left");
    rig.group
        .tx
        .send(GroupMsg::MemberSocketLost {
            member_id: rig.alice.member,
        })
        .await
        .unwrap();

    // The socket's grace teardown, once nobody came back.
    let gen = rig.hub.conn_gen(&rig.alice.key).unwrap();
    pharos_server::api::jellyfin::socket::expire_membership(
        &rig.state,
        &rig.hub,
        &rig.sinks,
        &rig.alice.key,
        gen,
        rig.alice.member,
        Some(rig.alice.id),
    )
    .await;

    rig.wait_cast_gone().await;
    assert_eq!(seat_total("ended", "owner_left"), before + 1);
    assert_eq!(rig.token_status(&token).await, 401);
    let roster = roster_seen_by_bob(&mut rig).await;
    assert_eq!(roster.len(), 1, "only bob remains: {roster:?}");
}

async fn cast_seat_count(rig: &CastRig) -> usize {
    let (tx, rx) = tokio::sync::oneshot::channel();
    rig.group
        .tx
        .send(GroupMsg::Snapshot { reply: tx })
        .await
        .unwrap();
    rx.await.unwrap().cast_seats.len()
}

async fn cast_tokens(rig: &CastRig) -> usize {
    use pharos_core::TokenStore;
    rig.state
        .stores
        .tokens_for(rig.alice.id)
        .await
        .unwrap()
        .iter()
        .filter(|t| t.device_id.starts_with("cast:"))
        .count()
}

#[actix_web::test]
async fn a_tv_answering_after_its_owner_left_is_not_given_a_seat() {
    let rig = CastRig::new().await;
    let release = rig.factory.hold_open(cast_rig::TV);
    let before = seat_total("rejected", "not_in_group");

    let start = rig.play_on_tv(&rig.alice);
    let vanish = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        // The owner is gone from the group without the service being told.
        let handle = rig.hub.detach_group(&rig.alice.key).unwrap();
        handle
            .tx
            .send(GroupMsg::RemoveMember {
                member_id: rig.alice.member,
            })
            .await
            .unwrap();
        release.notify_one();
    };
    let ((start_status, start_body), ()) = futures_util::future::join(start, vanish).await;

    assert_ne!(start_status, 204, "the cast must not start: {start_body}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(rig.service.snapshot().await.is_empty(), "no seat is left");
    assert_eq!(cast_seat_count(&rig).await, 0, "no seat in the group");
    assert!(
        rig.receiver.commands().is_empty(),
        "the TV was never driven"
    );
    assert_eq!(cast_tokens(&rig).await, 0, "the minted TV token is revoked");
    assert_eq!(seat_total("rejected", "not_in_group"), before + 1);
}
