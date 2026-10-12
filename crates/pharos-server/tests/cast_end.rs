//! 009 US3 over HTTP: leaving the group ends the leaver's cast first, and a
//! user holds at most one seat.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "common/cast_rig.rs"]
mod cast_rig;

use cast_rig::{counter, CastRig};

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

    let (status, body) = rig.post(&rig.alice, "/SyncPlay/Leave", None).await;
    assert_eq!(status, 204, "{body}");

    assert!(
        rig.service.snapshot().await.is_empty(),
        "the seat is gone by the time Leave answers"
    );
    assert_eq!(seat_total("ended", "owner_left"), before + 1);
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
