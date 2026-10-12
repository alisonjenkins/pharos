//! 009 US1 over HTTP: the TV session lists the cast's owner and item, and a
//! play request aimed at it starts a cast.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "common/cast_rig.rs"]
mod cast_rig;

use cast_rig::{CastRig, TV};
use pharos_cast::events::CastReason;
use pharos_server::cast_service::StartCast;
use pharos_server::state::SocketBroadcast;
use std::time::Duration;

fn start_request(p: &cast_rig::Person) -> StartCast {
    StartCast {
        user_id: p.id,
        user_name: p.name.clone(),
        owner_sync_key: p.key.clone(),
        tv_name: TV.into(),
    }
}

async fn wait_for_tv_owner(rig: &CastRig, expected: &str) -> serde_json::Value {
    for _ in 0..250 {
        let tv = rig.tv_session(&rig.bob).await;
        if tv["UserName"] == expected {
            return tv;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("TV session never showed owner {expected:?}");
}

#[actix_web::test]
async fn the_tv_session_shows_the_owner_and_item_while_a_cast_runs_and_not_after() {
    let mut rig = CastRig::new().await;
    assert_eq!(rig.tv_session(&rig.bob).await["UserName"], "");
    rig.bus_events();

    rig.service.start(start_request(&rig.alice)).await.unwrap();
    let tv = wait_for_tv_owner(&rig, "alice").await;
    assert_eq!(tv["UserId"], rig.alice.id.0.simple().to_string());
    assert_eq!(tv["NowPlayingItemId"], cast_rig::ITEM);
    assert!(
        rig.bus_events()
            .iter()
            .any(|b| matches!(b, SocketBroadcast::SessionsChanged)),
        "sockets are told to re-read Sessions when a cast starts"
    );

    rig.service.stop(rig.alice.id, CastReason::UserStop).await;
    wait_for_tv_owner(&rig, "").await;
    let tv = rig.tv_session(&rig.bob).await;
    assert!(
        tv["NowPlayingItemId"].is_null(),
        "no item once the cast ended"
    );
    assert!(
        rig.bus_events()
            .iter()
            .any(|b| matches!(b, SocketBroadcast::SessionsChanged)),
        "sockets are told to re-read Sessions when a cast ends"
    );
}

fn session_commands(events: &[SocketBroadcast]) -> Vec<(&str, &str)> {
    events
        .iter()
        .filter_map(|b| match b {
            SocketBroadcast::SessionCommand {
                session_id,
                command,
                ..
            } => Some((session_id.as_str(), command.as_str())),
            _ => None,
        })
        .collect()
}

#[actix_web::test]
async fn a_play_request_for_the_tv_session_starts_a_cast_and_is_not_broadcast() {
    let mut rig = CastRig::new().await;
    rig.bus_events();

    let (status, body) = rig.play_on_tv(&rig.alice).await;
    assert_eq!(status, 204, "{body}");

    let casts = rig.service.snapshot().await;
    assert_eq!(casts.len(), 1);
    assert_eq!(casts[0].tv, TV);
    assert_eq!(casts[0].owner, rig.alice.id);
    cast_rig::eventually("PlayNow on the emulator", || {
        rig.receiver
            .commands()
            .iter()
            .any(|c| c.body.name() == "PlayNow")
    })
    .await;

    let events = rig.bus_events();
    assert!(
        session_commands(&events).is_empty(),
        "no raw Play is broadcast for a virtual TV id: {events:?}"
    );
}

#[actix_web::test]
async fn a_play_request_for_an_ordinary_session_is_still_broadcast() {
    let mut rig = CastRig::new().await;
    rig.bus_events();

    let (status, _) = rig
        .post(
            &rig.alice,
            "/Sessions/some-other-session/Playing",
            Some(serde_json::json!({"ItemIds": ["7"], "PlayCommand": "PlayNow"})),
        )
        .await;
    assert_eq!(status, 204);
    assert_eq!(
        session_commands(&rig.bus_events()),
        vec![("some-other-session", "Play")]
    );
    assert!(rig.service.snapshot().await.is_empty());
}

#[actix_web::test]
async fn rejections_use_stable_reasons_and_statuses_that_name_who_is_at_fault() {
    let rig = CastRig::new().await;

    let (status, body) = rig.play_on_tv(&rig.carol).await;
    assert_eq!(status, 400, "{body}");
    assert!(body.starts_with("not_in_group"), "{body}");

    rig.factory.set_unreachable(TV, true);
    let (status, body) = rig.play_on_tv(&rig.alice).await;
    assert_eq!(status, 503, "{body}");
    assert!(body.starts_with("tv_unreachable"), "{body}");
    rig.factory.set_unreachable(TV, false);

    let (status, body) = rig.play_on_tv(&rig.alice).await;
    assert_eq!(status, 204, "{body}");
    let (status, body) = rig.play_on_tv(&rig.bob).await;
    assert_eq!(status, 409, "{body}");
    assert!(body.starts_with("tv_busy"), "{body}");
    assert_eq!(rig.service.snapshot().await.len(), 1, "bob took no seat");
}

#[actix_web::test]
async fn the_owner_alone_is_told_to_stop_its_local_player() {
    let mut rig = CastRig::new().await;
    rig.bus_events();

    let (status, body) = rig.play_on_tv(&rig.alice).await;
    assert_eq!(status, 204, "{body}");

    let targeted: Vec<_> = rig
        .bus_events()
        .into_iter()
        .filter_map(|b| match b {
            SocketBroadcast::DeviceCommand {
                device_key,
                controlling_user_id,
                command,
                ..
            } => Some((device_key, controlling_user_id, command)),
            _ => None,
        })
        .collect();
    assert_eq!(
        targeted,
        vec![(
            rig.alice.key.clone(),
            rig.alice.id.0.simple().to_string(),
            "Stop".to_string()
        )]
    );

    let (status, _) = rig.play_on_tv(&rig.bob).await;
    assert_eq!(status, 409);
    assert!(
        !rig.bus_events()
            .iter()
            .any(|b| matches!(b, SocketBroadcast::DeviceCommand { .. })),
        "a refused start leaves the caller's player alone"
    );
}
