//! 009 end to end on the production path: a real HTTP server, the real
//! `CastV2Factory`/`CastV2Transport` and a `MockTv` speaking CASTV2 over TLS on
//! a local port, with three occupants (owner, bob, carol) holding sockets in
//! one SyncPlay group. The TV must stay in sync with the group.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::cast_rig;

use std::time::Duration;

use cast_rig::{counter, drain, CastRig, Person, ITEM, START_MS};
use pharos_cast::obs::ConnState;
use pharos_sync::group::GroupMsg;
use pharos_sync::messages::{GroupPlayState, ServerMsg};
use tokio::time::Instant;

fn unix_now_ms() -> i64 {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    i64::try_from(since.as_millis()).unwrap()
}

/// Allowed gap between the TV and the group: the 750 ms dead-band plus one
/// progress interval of measurement staleness.
const TOLERANCE_MS: i64 = 1_800;

const SETTINGS: &str = "drift_threshold_ms = 750\n\
                        correction_cooldown_ms = 800\n\
                        tv_token_lifetime_secs = 600";

/// Where the group is, as its members were last told.
///
/// Anchored on the group's own clock: a command's `at_server_ms` plus the
/// group's epoch is the wall-clock instant its position holds at. Anchoring
/// on when the test happened to read the message instead made the model lag
/// the group by however late the read was, which on a loaded runner passed
/// the tolerance before the TV had drifted at all (B428).
struct GroupClock {
    /// The group's `server_ms` origin, in unix ms.
    epoch_unix_ms: i64,
    position_ms: i64,
    /// Unix ms at which the group was at `position_ms`.
    at_unix_ms: i64,
    playing: bool,
}

impl GroupClock {
    fn now(&self) -> i64 {
        if self.playing {
            self.position_ms + (unix_now_ms() - self.at_unix_ms)
        } else {
            self.position_ms
        }
    }

    fn observe(&mut self, rx: &mut tokio::sync::mpsc::Receiver<ServerMsg>) {
        for msg in drain(rx) {
            let (at_server_ms, position_ms, playing) = match msg {
                ServerMsg::Play {
                    by: _,
                    at_server_ms,
                    position_ms,
                } => (at_server_ms, position_ms, true),
                ServerMsg::Pause {
                    by: _,
                    at_server_ms,
                    position_ms,
                } => (at_server_ms, position_ms, false),
                ServerMsg::Seek {
                    by: _,
                    at_server_ms,
                    position_ms,
                } => (at_server_ms, position_ms, self.playing),
                _ => continue,
            };
            self.position_ms = i64::try_from(position_ms).unwrap();
            self.at_unix_ms = self.epoch_unix_ms + i64::try_from(at_server_ms).unwrap();
            self.playing = playing;
        }
    }
}

async fn eventually_within(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out after {secs} s waiting for: {what}");
}

fn tv_gap_ms(rig: &CastRig, clock: &GroupClock) -> i64 {
    (rig.receiver.state().position_ms - clock.now()).abs()
}

async fn casting() -> (CastRig, GroupClock) {
    let mut rig = CastRig::with_mock_tv(SETTINGS).await;
    let mut clock = GroupClock {
        epoch_unix_ms: i64::try_from(rig.group.epoch_unix_ms).unwrap(),
        position_ms: i64::try_from(START_MS).unwrap(),
        at_unix_ms: unix_now_ms(),
        playing: true,
    };
    // Bob's join was caught up with the group's Play (at_server_ms and
    // position), which anchors the clock on the group's own timeline.
    clock.observe(&mut rig.bob.rx);
    let (status, body) = rig.play_on_tv(&rig.alice).await;
    assert_eq!(status, 204, "{body}");
    eventually_within("the mock TV playing", 10, || rig.receiver.state().playing).await;
    (rig, clock)
}

async fn join(rig: &CastRig, who: &Person) {
    let (status, body) = rig
        .post(
            who,
            "/SyncPlay/Join",
            Some(serde_json::json!({"GroupId": rig.group.group_id.0})),
        )
        .await;
    assert_eq!(status, 204, "{body}");
}

async fn ready(rig: &CastRig, members: &[&Person], position_ms: u64, is_playing: bool) {
    for p in members {
        rig.group
            .tx
            .send(GroupMsg::MemberReady {
                member_id: p.member,
                position_ms,
                playlist_item_id: None,
                is_playing,
            })
            .await
            .unwrap();
    }
}

fn correction_total() -> u64 {
    counter("pharos_cast_correction_total", &[])
}

fn seat_total(event: &str, reason: &str) -> u64 {
    counter(
        "pharos_cast_seat_total",
        &[("event", event), ("reason", reason)],
    )
}

#[actix_web::test]
async fn casting_sends_the_mock_tv_a_playnow_at_the_group_position_with_a_working_token() {
    let (rig, clock) = casting().await;
    let mock = rig.mock.as_ref().unwrap();

    let commands = mock.commands();
    let first = commands.first().expect("the TV was sent a command");
    assert_eq!(first.body.name(), "PlayNow");
    assert_eq!(first.server_address, "http://pharos.lan:8096");
    let pharos_cast::messages::CommandBody::PlayNow(opts) = &first.body else {
        panic!("{first:?}");
    };
    assert_eq!(opts.items.first().map(|i| i.id.as_str()), Some(ITEM));
    let start_ms = opts.start_position_ticks / 10_000;
    assert!(
        (start_ms - clock.now()).abs() <= TOLERANCE_MS,
        "PlayNow starts at {start_ms} ms, the group is at {}",
        clock.now()
    );
    assert!(mock.malformed().is_empty(), "{:?}", mock.malformed());

    let token = rig.tv_token();
    assert!(!token.is_empty());
    assert_eq!(rig.token_status(&token).await, 200, "the TV's token works");
    assert_eq!(rig.service.snapshot().await.len(), 1);
    assert!(tv_gap_ms(&rig, &clock) <= TOLERANCE_MS);
}

#[actix_web::test]
async fn pause_seek_and_unpause_by_other_occupants_reach_the_tv_and_it_tracks_the_group() {
    let (mut rig, mut clock) = casting().await;
    join(&rig, &rig.carol).await;

    rig.group
        .tx
        .send(GroupMsg::PauseShared {
            sender: rig.bob.member,
        })
        .await
        .unwrap();
    eventually_within("the TV paused", 10, || rig.receiver.state().paused).await;
    clock.observe(&mut rig.bob.rx);
    assert!(!clock.playing, "bob was told the group paused");
    assert!(
        tv_gap_ms(&rig, &clock) <= TOLERANCE_MS,
        "paused at {} vs group {}",
        rig.receiver.state().position_ms,
        clock.now()
    );
    let frozen = rig.receiver.state().position_ms;
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert_eq!(
        rig.receiver.state().position_ms,
        frozen,
        "a paused TV holds"
    );

    rig.group
        .tx
        .send(GroupMsg::SeekTo {
            sender: rig.carol.member,
            position_ms: 600_000,
        })
        .await
        .unwrap();
    // A Ready earlier than the seek's scheduled start does not count.
    tokio::time::sleep(Duration::from_millis(600)).await;
    ready(&rig, &[&rig.bob, &rig.carol], 600_000, false).await;
    eventually_within("the TV seeked to 600 s", 10, || {
        (rig.receiver.state().position_ms - 600_000).abs() <= TOLERANCE_MS
    })
    .await;
    assert!(rig.receiver.state().paused, "a seek keeps the group paused");

    rig.group
        .tx
        .send(GroupMsg::Unpause {
            sender: rig.bob.member,
        })
        .await
        .unwrap();
    ready(&rig, &[&rig.bob, &rig.carol], 600_000, true).await;
    eventually_within("the TV playing again", 10, || !rig.receiver.state().paused).await;
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    clock.observe(&mut rig.bob.rx);
    assert!(clock.playing);
    assert!(
        tv_gap_ms(&rig, &clock) <= TOLERANCE_MS,
        "TV at {} vs group {}",
        rig.receiver.state().position_ms,
        clock.now()
    );
    assert!(rig.mock.as_ref().unwrap().malformed().is_empty());
}

#[actix_web::test]
async fn a_late_joiner_and_a_buffering_occupant_never_hold_the_tv_up() {
    let (mut rig, mut clock) = casting().await;
    let mock_commands = rig.mock.as_ref().unwrap().commands().len();

    // Carol joins mid-cast and never acks anything.
    join(&rig, &rig.carol).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(rig.receiver.state().playing && !rig.receiver.state().paused);
    assert_ne!(rig.group_play_state().await, GroupPlayState::Waiting);
    assert_eq!(
        rig.service.snapshot().await.first().map(|c| c.state),
        Some(ConnState::Following)
    );

    // Bob stalls: the group freezes for a real Player and the TV follows it...
    rig.group
        .tx
        .send(GroupMsg::BufferingStart {
            member_id: rig.bob.member,
            position_ms: 0,
            playlist_item_id: None,
        })
        .await
        .unwrap();
    eventually_within("the TV follows the freeze", 10, || {
        rig.receiver.state().paused
    })
    .await;
    // ...and is released with the group when bob recovers: the TV is not left frozen.
    rig.group
        .tx
        .send(GroupMsg::BufferingEnd {
            member_id: rig.bob.member,
        })
        .await
        .unwrap();
    ready(&rig, &[&rig.bob, &rig.carol], 0, true).await;
    eventually_within("the TV resumed with the group", 10, || {
        !rig.receiver.state().paused
    })
    .await;
    clock.observe(&mut rig.bob.rx);

    // A gate opens on the Players' acks alone; nothing waits on the TV (V226),
    // and the TV still lands on the seek.
    rig.assert_players_not_frozen(&[rig.bob.member, rig.carol.member])
        .await;
    eventually_within("the TV on the seek target", 10, || {
        (rig.receiver.state().position_ms - 300_000).abs() <= TOLERANCE_MS
    })
    .await;
    assert!(
        rig.mock.as_ref().unwrap().commands().len() > mock_commands,
        "the TV was sent the group's changes"
    );
}

#[actix_web::test]
async fn drift_beyond_the_threshold_is_corrected_and_the_tv_converges() {
    let (mut rig, mut clock) = casting().await;
    join(&rig, &rig.carol).await;
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    clock.observe(&mut rig.bob.rx);
    // Poll rather than sample once: a loaded runner can delay the TV's first
    // position report past the fixed sleep.
    eventually_within("the TV in sync to begin", 10, || {
        tv_gap_ms(&rig, &clock) <= TOLERANCE_MS
    })
    .await;
    rig.exclusive_metrics().await;
    let before = correction_total();

    // The TV skips 20 s ahead without telling anyone.
    rig.receiver.inject_drift(20_000);
    assert!(tv_gap_ms(&rig, &clock) > 10_000, "the TV really drifted");

    eventually_within("a correction is sent", 15, || correction_total() > before).await;
    eventually_within("the TV converges on the group", 15, || {
        tv_gap_ms(&rig, &clock) <= TOLERANCE_MS
    })
    .await;
    assert!(rig.mock.as_ref().unwrap().malformed().is_empty());
}

#[actix_web::test]
async fn a_dropped_connection_inside_the_grace_reconnects_and_resyncs() {
    let (rig, clock) = casting().await;
    let token = rig.tv_token();
    let mock = rig.mock.as_ref().unwrap();
    let connections = mock.connections();
    rig.exclusive_metrics().await;
    let lost = seat_total("lost", "tv_unreachable");
    let back = seat_total("reconnected", "tv_unreachable");
    let ended = seat_total("ended", "grace_expired");

    mock.drop_connection();
    eventually_within("lost then reconnected", 10, || {
        seat_total("lost", "tv_unreachable") > lost
            && seat_total("reconnected", "tv_unreachable") > back
    })
    .await;
    rig.wait_cast_state(ConnState::Following).await;

    assert!(mock.connections() > connections, "a fresh TLS connection");
    assert_eq!(rig.service.snapshot().await.len(), 1);
    assert_eq!(seat_total("ended", "grace_expired"), ended);
    assert_eq!(rig.token_status(&token).await, 200, "token survives a blip");
    eventually_within("the TV back on the group position", 10, || {
        tv_gap_ms(&rig, &clock) <= TOLERANCE_MS && rig.receiver.state().playing
    })
    .await;
}

#[actix_web::test]
async fn stopping_on_the_tv_ends_the_cast_as_user_stop_and_revokes_the_token() {
    let (rig, _clock) = casting().await;
    let token = rig.tv_token();
    assert_eq!(rig.token_status(&token).await, 200);
    rig.exclusive_metrics().await;
    let before = seat_total("ended", "user_stop");

    rig.receiver.user_stop();

    rig.wait_cast_gone().await;
    assert_eq!(seat_total("ended", "user_stop"), before + 1);
    assert_eq!(rig.token_status(&token).await, 401, "V229");
}
