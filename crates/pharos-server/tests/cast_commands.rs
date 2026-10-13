//! 009 US2 over HTTP: transport commands aimed at the TV session are applied
//! to the SyncPlay group as the caller, never relayed to a player (V173), and
//! each is counted like any other SyncPlay command (V160).

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "common/cast_rig.rs"]
mod cast_rig;

use cast_rig::{eventually, CastRig, Person};
use pharos_server::state::SocketBroadcast;

fn command_total(command: &str, outcome: &str) -> u64 {
    cast_rig::counter(
        "pharos_syncplay_command_total",
        &[("command", command), ("outcome", outcome)],
    )
}

async fn rig_with_cast() -> CastRig {
    let rig = CastRig::new().await;
    let (status, body) = rig.play_on_tv(&rig.alice).await;
    assert_eq!(status, 204, "{body}");
    eventually("TV loaded", || rig.receiver.state().playing).await;
    rig
}

async fn tv_command(rig: &CastRig, who: &Person, command: &str, query: &str) -> (u16, String) {
    rig.post(
        who,
        &format!("/Sessions/{}/Playing/{command}{query}", rig.tv_id),
        None,
    )
    .await
}

fn raw_session_commands(events: &[SocketBroadcast]) -> usize {
    events
        .iter()
        .filter(|b| matches!(b, SocketBroadcast::SessionCommand { .. }))
        .count()
}

#[actix_web::test]
async fn transport_commands_reach_the_group_and_the_tv_and_are_counted() {
    let mut rig = rig_with_cast().await;
    rig.bus_events();

    let before = command_total("pause", "dispatched");
    let (status, body) = tv_command(&rig, &rig.bob, "Pause", "").await;
    assert_eq!(status, 204, "{body}");
    eventually("TV paused by a group member's TV-session Pause", || {
        rig.receiver.state().paused
    })
    .await;
    assert_eq!(command_total("pause", "dispatched"), before + 1);

    let before = command_total("unpause", "dispatched");
    let (status, body) = tv_command(&rig, &rig.bob, "Unpause", "").await;
    assert_eq!(status, 204, "{body}");
    eventually("TV unpaused", || !rig.receiver.state().paused).await;
    assert_eq!(command_total("unpause", "dispatched"), before + 1);

    let before = command_total("seek", "dispatched");
    let (status, body) = tv_command(&rig, &rig.bob, "Seek", "?seekPositionTicks=9000000000").await;
    assert_eq!(status, 204, "{body}");
    eventually("TV seeked to 900 s", || {
        rig.receiver
            .commands()
            .iter()
            .any(|c| c.body.name() == "Seek")
            && (rig.receiver.state().position_ms - 900_000).abs() < 15_000
    })
    .await;
    assert_eq!(command_total("seek", "dispatched"), before + 1);

    let before = command_total("pause", "dispatched");
    let (status, body) = tv_command(&rig, &rig.bob, "PlayPause", "").await;
    assert_eq!(status, 204, "{body}");
    eventually("PlayPause on a playing group pauses it", || {
        rig.receiver.state().paused
    })
    .await;
    assert_eq!(command_total("pause", "dispatched"), before + 1);

    let (next, prev) = (
        command_total("nextitem", "dispatched"),
        command_total("previousitem", "dispatched"),
    );
    assert_eq!(tv_command(&rig, &rig.bob, "NextTrack", "").await.0, 204);
    assert_eq!(tv_command(&rig, &rig.bob, "PreviousTrack", "").await.0, 204);
    assert_eq!(command_total("nextitem", "dispatched"), next + 1);
    assert_eq!(command_total("previousitem", "dispatched"), prev + 1);

    assert_eq!(
        raw_session_commands(&rig.bus_events()),
        0,
        "no raw PlayState is broadcast for a virtual TV id (V173)"
    );
}

#[actix_web::test]
async fn a_seek_without_a_position_is_the_callers_error() {
    let rig = rig_with_cast().await;
    let (status, body) = tv_command(&rig, &rig.bob, "Seek", "").await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("SeekPositionTicks"), "{body}");
}

#[actix_web::test]
async fn only_the_owner_stops_the_cast() {
    let rig = rig_with_cast().await;

    let (status, body) = tv_command(&rig, &rig.bob, "Stop", "").await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(rig.service.snapshot().await.len(), 1, "bob did not end it");

    let (status, body) = tv_command(&rig, &rig.alice, "Stop", "").await;
    assert_eq!(status, 204, "{body}");
    assert!(
        rig.service.snapshot().await.is_empty(),
        "owner Stop ends it"
    );
}

#[actix_web::test]
async fn a_caller_outside_the_casts_group_is_refused_with_a_reason() {
    let rig = rig_with_cast().await;
    let (status, body) = tv_command(&rig, &rig.carol, "Pause", "").await;
    assert_eq!(status, 403, "{body}");
    assert!(body.starts_with("not_in_group"), "{body}");
    assert!(!rig.receiver.state().paused);
}

#[actix_web::test]
async fn a_command_for_an_idle_tv_is_a_conflict_not_a_relay() {
    let mut rig = CastRig::new().await;
    rig.bus_events();
    let (status, body) = tv_command(&rig, &rig.alice, "Pause", "").await;
    assert_eq!(status, 409, "{body}");
    assert!(body.starts_with("no_cast"), "{body}");
    assert_eq!(raw_session_commands(&rig.bus_events()), 0);
}

#[actix_web::test]
async fn volume_and_display_commands_are_ignored_for_the_tv() {
    let mut rig = rig_with_cast().await;
    rig.bus_events();
    for command in ["SetVolume", "ToggleMute", "DisplayMessage"] {
        let (status, body) = rig
            .post(
                &rig.bob,
                &format!("/Sessions/{}/Command/{command}", rig.tv_id),
                None,
            )
            .await;
        assert_eq!(status, 204, "{command}: {body}");
    }
    assert_eq!(raw_session_commands(&rig.bus_events()), 0);
}

#[actix_web::test]
async fn commands_for_an_ordinary_session_are_still_relayed() {
    let mut rig = CastRig::new().await;
    rig.bus_events();
    let (status, _) = rig
        .post(&rig.alice, "/Sessions/some-player/Playing/Pause", None)
        .await;
    assert_eq!(status, 204);
    let (status, _) = rig
        .post(&rig.alice, "/Sessions/some-player/Command/ToggleMute", None)
        .await;
    assert_eq!(status, 204);
    assert_eq!(raw_session_commands(&rig.bus_events()), 2);
}

/// `(item id, start position ms)` of every `PlayNow` the TV has received.
fn play_nows(rig: &CastRig) -> Vec<(String, i64)> {
    rig.receiver
        .commands()
        .into_iter()
        .filter_map(|c| match c.body {
            pharos_cast::messages::CommandBody::PlayNow(o) => {
                Some((o.items.first()?.id.clone(), o.start_position_ticks / 10_000))
            }
            _ => None,
        })
        .collect()
}

#[actix_web::test]
async fn a_queue_change_by_another_member_loads_the_new_item_on_the_tv() {
    let rig = rig_with_cast().await;
    assert_eq!(play_nows(&rig).len(), 1, "only the hand-off load so far");

    let (status, body) = rig
        .post(
            &rig.bob,
            "/SyncPlay/SetNewQueue",
            Some(serde_json::json!({
                "PlayingQueue": ["item-2", "item-3"],
                "PlayingItemPosition": 0,
                "StartPositionTicks": 6_000_000_000u64,
            })),
        )
        .await;
    assert_eq!(status, 204, "{body}");
    eventually("PlayNow for the new queue's first item", || {
        play_nows(&rig).iter().any(|(id, _)| id == "item-2")
    })
    .await;
    let (_, pos) = play_nows(&rig)
        .into_iter()
        .find(|(id, _)| id == "item-2")
        .unwrap();
    assert!(
        (pos - 600_000).abs() < 15_000,
        "loaded at the requested position, got {pos} ms"
    );

    // NextItem advances the group, and the TV follows to the third item from 0.
    let (status, body) = rig.post(&rig.bob, "/SyncPlay/NextItem", None).await;
    assert_eq!(status, 204, "{body}");
    eventually("PlayNow for the next item", || {
        play_nows(&rig).iter().any(|(id, _)| id == "item-3")
    })
    .await;
    let (_, pos) = play_nows(&rig)
        .into_iter()
        .find(|(id, _)| id == "item-3")
        .unwrap();
    assert!(
        pos < 15_000,
        "next item starts near its beginning, got {pos} ms"
    );
    assert_eq!(rig.service.snapshot().await.len(), 1, "still one seat");
}

/// Past the driver's settle window, which ignores reports echoing our own PlayNow.
async fn settled_cast() -> CastRig {
    let mut rig = rig_with_cast().await;
    tokio::time::sleep(std::time::Duration::from_millis(2_200)).await;
    cast_rig::drain(&mut rig.bob.rx);
    rig
}

fn count(msgs: &[pharos_sync::messages::ServerMsg], kind: &str) -> usize {
    msgs.iter().filter(|m| m.kind() == kind).count()
}

fn tv_commands_named(rig: &CastRig, name: &str) -> usize {
    rig.receiver
        .commands()
        .iter()
        .filter(|c| c.body.name() == name)
        .count()
}

#[actix_web::test]
async fn a_pause_on_the_tv_pauses_the_group_once_and_the_planner_leaves_it_alone() {
    let mut rig = settled_cast().await;
    let before = command_total("pause", "dispatched");
    let tv_pauses = tv_commands_named(&rig, "Pause");

    rig.receiver.user_pause();
    cast_rig::wait_msg(&mut rig.bob.rx, "group Pause", |m| m.kind() == "pause").await;
    tokio::time::sleep(std::time::Duration::from_millis(3_500)).await;

    let seen = cast_rig::drain(&mut rig.bob.rx);
    assert_eq!(count(&seen, "pause"), 0, "no second group Pause: {seen:?}");
    assert_eq!(
        count(&seen, "play"),
        0,
        "the group is not resumed: {seen:?}"
    );
    assert_eq!(command_total("pause", "dispatched"), before + 1);
    assert!(rig.receiver.state().paused, "the TV stays paused");
    assert_eq!(tv_commands_named(&rig, "Unpause"), 0);
    assert!(
        tv_commands_named(&rig, "Pause") <= tv_pauses + 1,
        "at most the group's echo reaches the TV"
    );

    let before = command_total("unpause", "dispatched");
    rig.receiver.user_unpause();
    cast_rig::wait_msg(&mut rig.bob.rx, "group Play", |m| m.kind() == "play").await;
    tokio::time::sleep(std::time::Duration::from_millis(2_000)).await;
    assert_eq!(command_total("unpause", "dispatched"), before + 1);
    assert!(!rig.receiver.state().paused);
    let seen = cast_rig::drain(&mut rig.bob.rx);
    assert_eq!(count(&seen, "pause"), 0, "{seen:?}");
}

#[actix_web::test]
async fn a_seek_on_the_tv_seeks_the_group_once_and_the_planner_leaves_it_alone() {
    let mut rig = settled_cast().await;
    let before = command_total("seek", "dispatched");
    let tv_seeks = tv_commands_named(&rig, "Seek");

    rig.receiver
        .user_seek(std::time::Duration::from_secs(1_800));
    cast_rig::wait_msg(&mut rig.bob.rx, "group Seek", |m| m.kind() == "seek").await;
    tokio::time::sleep(std::time::Duration::from_millis(3_500)).await;

    let seen = cast_rig::drain(&mut rig.bob.rx);
    assert_eq!(count(&seen, "seek"), 0, "no second group Seek: {seen:?}");
    assert_eq!(command_total("seek", "dispatched"), before + 1);
    assert!(
        (rig.receiver.state().position_ms - 1_800_000).abs() < 15_000,
        "the TV is where the user put it"
    );
    assert!(
        tv_commands_named(&rig, "Seek") <= tv_seeks + 1,
        "the planner does not drag the TV back"
    );
}

#[actix_web::test]
async fn a_stop_on_the_tv_ends_the_cast_once_and_leaves_the_group_playing() {
    let rig = settled_cast().await;
    let before = cast_rig::counter(
        "pharos_cast_seat_total",
        &[("event", "ended"), ("reason", "user_stop")],
    );

    rig.receiver.user_stop();
    rig.wait_cast_gone().await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    assert_eq!(
        cast_rig::counter(
            "pharos_cast_seat_total",
            &[("event", "ended"), ("reason", "user_stop")]
        ),
        before + 1
    );
    assert_ne!(
        rig.group_play_state().await,
        pharos_sync::messages::GroupPlayState::Paused,
        "stopping the TV does not pause the others"
    );
}
