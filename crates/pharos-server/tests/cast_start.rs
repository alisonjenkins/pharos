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

fn seat_total(event: &str, reason: &str) -> u64 {
    cast_rig::counter(
        "pharos_cast_seat_total",
        &[("event", event), ("reason", reason)],
    )
}

#[actix_web::test]
async fn a_tv_running_another_app_is_refused_with_tv_busy_naming_the_app() {
    let rig = CastRig::new().await;
    rig.factory.set_occupancy(
        TV,
        pharos_cast::castv2::Occupancy::ForeignApp {
            app_id: "CC1AD845".into(),
            display_name: Some("Default Media Receiver".into()),
        },
    );
    let before = seat_total("rejected", "tv_busy");

    let (status, body) = rig.play_on_tv(&rig.alice).await;

    assert_eq!(status, 409, "{body}");
    assert!(body.starts_with("tv_busy"), "{body}");
    assert!(
        body.contains("CC1AD845"),
        "the reason names the app: {body}"
    );
    assert_eq!(seat_total("rejected", "tv_busy"), before + 1);
    assert!(rig.service.snapshot().await.is_empty());
    assert!(rig.receiver.commands().is_empty(), "the TV was left alone");
}

#[actix_web::test]
async fn a_tv_that_is_slow_to_load_gets_its_seat_without_holding_the_group() {
    let rig = CastRig::new().await;
    rig.receiver.configure(|c| c.command_latency_ms = 1_500);

    let started = std::time::Instant::now();
    let (status, body) = rig.play_on_tv(&rig.alice).await;
    assert_eq!(status, 204, "{body}");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the request does not wait for the TV to load"
    );

    // While the TV is still loading, the Player's seek is not held by it.
    rig.assert_players_not_frozen(&[rig.bob.member]).await;
    rig.wait_cast_state(pharos_cast::obs::ConnState::Following)
        .await;
    assert_eq!(rig.service.snapshot().await.len(), 1);
}

#[actix_web::test]
async fn a_tv_that_never_reports_the_item_ends_the_cast_with_a_reason() {
    let mut rig = CastRig::new().await;
    rig.receiver.configure(|c| c.command_latency_ms = 3_600_000);
    let (lost, ended) = (
        seat_total("lost", "tv_unreachable"),
        seat_total("ended", "tv_unreachable"),
    );
    assert_eq!(rig.play_on_tv(&rig.alice).await.0, 204);
    let token = rig.tv_token();
    cast_rig::drain(&mut rig.alice.rx);

    // The driver's load bound (30 s) is the only way out: real time.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    while !rig.service.snapshot().await.is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the cast never gave up on a TV that does not load"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    assert_eq!(seat_total("lost", "tv_unreachable"), lost + 1);
    assert_eq!(seat_total("ended", "tv_unreachable"), ended + 1);
    assert_eq!(rig.token_status(&token).await, 401);
    cast_rig::wait_msg(&mut rig.alice.rx, "owner caught up", |m| {
        matches!(m, pharos_sync::messages::ServerMsg::PlayQueue { .. })
    })
    .await;
}

#[actix_web::test]
async fn the_tv_session_id_is_the_same_after_a_restart() {
    let first = CastRig::new().await;
    let before = first.tv_session(&first.bob).await["Id"].clone();
    drop(first);

    let second = CastRig::new().await;
    let after = second.tv_session(&second.bob).await["Id"].clone();
    assert_eq!(before, after, "a new AppState from the same config");
    assert_eq!(after, second.tv_id.as_str());
}
