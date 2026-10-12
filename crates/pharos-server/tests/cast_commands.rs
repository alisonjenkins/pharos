//! 009 US2 over HTTP: transport commands aimed at the TV session are applied
//! to the SyncPlay group as the caller, never relayed to a player (V169), and
//! each is counted like any other SyncPlay command (V160).

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "common/cast_rig.rs"]
mod cast_rig;

use cast_rig::{eventually, CastRig, Person};
use metrics_util::debugging::{DebugValue, DebuggingRecorder, Snapshotter};
use pharos_server::state::SocketBroadcast;
use std::sync::OnceLock;

/// One process-wide recorder: the handlers run on the HTTP server's thread, so
/// a thread-local recorder would never see their counters.
fn snapshotter() -> &'static Snapshotter {
    static SNAP: OnceLock<Snapshotter> = OnceLock::new();
    SNAP.get_or_init(|| {
        let recorder = DebuggingRecorder::new();
        let snap = recorder.snapshotter();
        recorder
            .install()
            .expect("no other recorder in this binary");
        snap
    })
}

fn command_total(command: &str, outcome: &str) -> u64 {
    snapshotter()
        .snapshot()
        .into_vec()
        .into_iter()
        .filter(|(ck, _, _, _)| ck.key().name() == "pharos_syncplay_command_total")
        .filter(|(ck, _, _, _)| {
            let has = |k: &str, v: &str| ck.key().labels().any(|l| l.key() == k && l.value() == v);
            has("command", command) && has("outcome", outcome)
        })
        .map(|(_, _, _, v)| match v {
            DebugValue::Counter(c) => c,
            _ => 0,
        })
        .sum()
}

async fn rig_with_cast() -> CastRig {
    snapshotter();
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
        "no raw PlayState is broadcast for a virtual TV id (V169)"
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
