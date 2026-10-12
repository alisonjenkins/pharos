//! 009 US3 over HTTP: leaving the group ends the leaver's cast first, and a
//! user holds at most one seat.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "common/cast_rig.rs"]
mod cast_rig;

use cast_rig::{counter, eventually, CastRig};
use pharos_cast::obs::ConnState;
use pharos_sync::group::GroupMsg;
use pharos_sync::messages::{GroupPlayState, MemberId};
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

/// A Seek by Bob opens its gate as soon as every Player in `players` has
/// acked: nothing waits on the TV, whether its seat is lost or gone.
async fn assert_players_not_frozen(rig: &CastRig, players: &[MemberId]) {
    rig.group
        .tx
        .send(GroupMsg::SeekTo {
            sender: rig.bob.member,
            position_ms: 300_000,
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    for member_id in players {
        rig.group
            .tx
            .send(GroupMsg::MemberReady {
                member_id: *member_id,
                position_ms: 300_000,
                playlist_item_id: None,
                is_playing: false,
            })
            .await
            .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_ne!(
        rig.group_play_state().await,
        GroupPlayState::Waiting,
        "a gate is still held after the only Player acked"
    );
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
    assert_eq!(rig.token_status(&token).await, 401, "V171");
    eventually("the TV is told to stop", || {
        rig.receiver
            .commands()
            .iter()
            .any(|c| c.body.name() == "Stop")
    })
    .await;
    assert_players_not_frozen(&rig, &[rig.bob.member, rig.alice.member]).await;
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
    assert_players_not_frozen(&rig, &[rig.bob.member]).await;

    rig.wait_cast_gone().await;
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "grace is 1 s; the end must not wait out anything longer"
    );
    assert_eq!(seat_total("ended", "grace_expired"), ended + 1);
    assert_eq!(rig.token_status(&token).await, 401, "V171");
    assert_players_not_frozen(&rig, &[rig.bob.member, rig.alice.member]).await;
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
    assert_players_not_frozen(&rig, &[rig.bob.member]).await;
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
    assert_eq!(rig.token_status(&token).await, 401, "V171");
}
