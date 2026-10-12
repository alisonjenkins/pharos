//! 009 US4 over HTTP: casting before joining a group is refused with a
//! readable reason, and the same request works once the caller has joined.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "common/cast_rig.rs"]
mod cast_rig;

use cast_rig::{counter, eventually, CastRig};
use pharos_cast::messages::CommandBody;

fn seat_total(event: &str, reason: &str) -> u64 {
    counter(
        "pharos_cast_seat_total",
        &[("event", event), ("reason", reason)],
    )
}

#[actix_web::test]
async fn casting_before_joining_is_refused_then_works_after_joining() {
    let rig = CastRig::new().await;
    let before = seat_total("rejected", "not_in_group");

    let (status, body) = rig.play_on_tv(&rig.carol).await;
    assert_eq!(status, 400, "{body}");
    assert!(body.starts_with("not_in_group"), "{body}");
    assert!(
        body.contains("join"),
        "the reason tells the user what to do: {body}"
    );
    assert_eq!(seat_total("rejected", "not_in_group"), before + 1);
    assert!(rig.service.snapshot().await.is_empty());
    assert!(
        rig.receiver.commands().is_empty(),
        "a refused request never touches the TV"
    );

    let (status, body) = rig
        .post(
            &rig.carol,
            "/SyncPlay/Join",
            Some(serde_json::json!({"GroupId": rig.group.group_id.0})),
        )
        .await;
    assert_eq!(status, 204, "{body}");

    let (status, body) = rig.play_on_tv(&rig.carol).await;
    assert_eq!(status, 204, "{body}");
    assert_eq!(seat_total("rejected", "not_in_group"), before + 1);
    eventually("the TV adopts the group position", || {
        rig.receiver.commands().iter().any(|c| match &c.body {
            CommandBody::PlayNow(o) => {
                o.items.first().map(|i| i.id.as_str()) == Some(cast_rig::ITEM)
                    && (o.start_position_ticks / 10_000 - cast_rig::START_MS as i64).abs() < 30_000
            }
            _ => false,
        })
    })
    .await;
    let casts = rig.service.snapshot().await;
    assert_eq!(casts.len(), 1);
    assert_eq!(casts[0].owner, rig.carol.id);
    assert_eq!(casts[0].group, rig.group.group_id);
}

#[actix_web::test]
async fn leaving_the_group_while_casting_ends_the_seat() {
    let rig = CastRig::new().await;
    let (status, _) = rig
        .post(
            &rig.carol,
            "/SyncPlay/Join",
            Some(serde_json::json!({"GroupId": rig.group.group_id.0})),
        )
        .await;
    assert_eq!(status, 204);
    assert_eq!(rig.play_on_tv(&rig.carol).await.0, 204);
    let before = seat_total("ended", "owner_left");

    let (status, body) = rig.post(&rig.carol, "/SyncPlay/Leave", None).await;
    assert_eq!(status, 204, "{body}");

    assert!(rig.service.snapshot().await.is_empty());
    assert_eq!(seat_total("ended", "owner_left"), before + 1);
    let (status, body) = rig.play_on_tv(&rig.carol).await;
    assert_eq!(status, 400, "a leaver is back to not_in_group: {body}");
    assert!(body.starts_with("not_in_group"), "{body}");
}
