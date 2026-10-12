//! T021 / T025 / T038 at service level: the cast hand-off driven against a
//! real `GroupRegistry` + `SessionHub` with `FakeReceiver` as the TV.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use actix_web::{http::StatusCode, test, web};
use metrics_util::debugging::{DebugValue, DebuggingRecorder, Snapshotter};
use pharos_cast::castv2::Occupancy;
use pharos_cast::events::CastReason;
use pharos_cast::fake::FakeReceiverHandle;
use pharos_cast::messages::CommandBody;
use pharos_cast::obs::ConnState;
use pharos_core::{SecretString, UserId, UserPolicy, UserRecord, UserStore};
use pharos_server::auth::BuiltinAuth;
use pharos_server::cast_service::{
    self, CastDeps, CastRejectReason, CastServiceHandle, FakeTransportFactory, StartCast,
};
use pharos_server::config::CastConfig;
use pharos_server::state::{AppState, Stores};
use pharos_sync::group::{GroupHandle, GroupMsg};
use pharos_sync::hub::member_id_for_device;
use pharos_sync::messages::{GroupPlayState, MemberId, MemberSummary, ServerMsg};
use pharos_sync::{GroupRegistry, LocalDelivery, MemberSinks, SessionHub};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

const TV: &str = "Living Room";
const ITEM: &str = "item-1";
const START_MS: u64 = 120_000;

fn cast_config(other_user: UserId) -> CastConfig {
    toml::from_str(&format!(
        r#"
        server_url = "http://pharos.lan:8096"
        reconnect_grace_secs = 1
        [[tvs]]
        name = "Living Room"
        address = "192.168.1.60:8009"
        [[tvs]]
        name = "Den"
        address = "192.168.1.61:8009"
        [[tvs]]
        name = "Locked"
        address = "192.168.1.62:8009"
        allowed_users = ["{}"]
        "#,
        other_user.0.simple()
    ))
    .unwrap()
}

struct Person {
    id: UserId,
    name: String,
    key: String,
    member: MemberId,
    rx: mpsc::Receiver<ServerMsg>,
}

struct Rig {
    stores: Stores,
    hub: SessionHub,
    sinks: MemberSinks,
    group: GroupHandle,
    service: CastServiceHandle,
    factory: FakeTransportFactory,
    living_room: FakeReceiverHandle,
    snap: Snapshotter,
    owner: Person,
    bob: Person,
    _recorder_guard: metrics::LocalRecorderGuard<'static>,
}

async fn new_user(stores: &Stores, name: &str) -> UserId {
    let auth = BuiltinAuth::new(stores.clone());
    let id = UserId::new();
    stores
        .create(UserRecord {
            id,
            name: name.into(),
            password_hash: auth.hash_password(&SecretString::new("p")).unwrap(),
            policy: UserPolicy::default(),
        })
        .await
        .unwrap();
    id
}

async fn join(
    stores_hub: (&SessionHub, &MemberSinks),
    group: &GroupHandle,
    id: UserId,
    name: &str,
) -> Person {
    let (hub, sinks) = stores_hub;
    let key = format!("{}:dev-{name}", id.0.simple());
    let (tx, rx) = mpsc::channel(256);
    let reg = hub.register(key.clone(), name.into(), tx.clone());
    hub.attach_group(&key, group.clone());
    sinks.insert(reg.member_id, reg.gen, tx);
    let (reply, joined) = oneshot::channel();
    group
        .tx
        .send(GroupMsg::AddMember {
            member_id: reg.member_id,
            name: name.into(),
            reply,
        })
        .await
        .unwrap();
    joined.await.unwrap();
    Person {
        id,
        name: name.into(),
        key,
        member: reg.member_id,
        rx,
    }
}

impl Rig {
    async fn new() -> Self {
        let recorder = DebuggingRecorder::new();
        let snap = recorder.snapshotter();
        // Leaked so the guard can be 'static; one small recorder per test.
        let recorder: &'static DebuggingRecorder = Box::leak(Box::new(recorder));
        let guard = metrics::set_default_local_recorder(recorder);

        let stores = Stores::connect("sqlite::memory:").await.unwrap();
        let sinks = MemberSinks::new();
        let hub = SessionHub::new();
        let registry = GroupRegistry::spawn(Arc::new(LocalDelivery::new(sinks.clone())));
        let group = registry.create().await.unwrap();

        let owner_id = new_user(&stores, "ali").await;
        let bob_id = new_user(&stores, "bob").await;
        let owner = join((&hub, &sinks), &group, owner_id, "ali").await;
        group
            .tx
            .send(GroupMsg::SeedQueue {
                item_id: ITEM.into(),
                runtime_ms: Some(3_600_000),
                position_ms: START_MS,
                is_paused: false,
            })
            .await
            .unwrap();
        let bob = join((&hub, &sinks), &group, bob_id, "bob").await;

        let factory = FakeTransportFactory::new();
        let living_room = factory.add_tv(TV);
        factory.add_tv("Den");
        factory.add_tv("Locked");
        let service = cast_service::spawn(
            CastDeps {
                hub: hub.clone(),
                registry,
                sinks: sinks.clone(),
                stores: stores.clone(),
            },
            cast_config(UserId::new()),
            factory.clone(),
        );
        Self {
            stores,
            hub,
            sinks,
            group,
            service,
            factory,
            living_room,
            snap,
            owner,
            bob,
            _recorder_guard: guard,
        }
    }

    fn request(p: &Person, tv: &str) -> StartCast {
        StartCast {
            user_id: p.id,
            user_name: p.name.clone(),
            owner_sync_key: p.key.clone(),
            tv_name: tv.into(),
        }
    }

    async fn start_owner(&self) {
        self.service
            .start(Self::request(&self.owner, TV))
            .await
            .unwrap();
    }

    fn seat_member(&self) -> MemberId {
        member_id_for_device(&format!("{}:cast:{TV}", self.owner.id.0.simple()))
    }

    /// The roster as `person` sees it after a resync (Joined carries it).
    async fn roster_seen_by(&mut self, who: Who) -> Vec<MemberSummary> {
        let person = match who {
            Who::Bob => &mut self.bob,
            Who::Owner => &mut self.owner,
        };
        while person.rx.try_recv().is_ok() {}
        self.group
            .tx
            .send(GroupMsg::ResyncMember {
                member_id: person.member,
            })
            .await
            .unwrap();
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(3), person.rx.recv())
                .await
                .expect("a Joined after resync")
                .expect("sink open");
            if let ServerMsg::Joined { members, .. } = msg {
                return members;
            }
        }
    }

    fn seat_total(&self, event: &str, reason: &str) -> u64 {
        self.snap
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(ck, _, _, _)| ck.key().name() == "pharos_cast_seat_total")
            .filter(|(ck, _, _, _)| {
                let l: Vec<_> = ck.key().labels().collect();
                l.iter().any(|x| x.key() == "event" && x.value() == event)
                    && l.iter().any(|x| x.key() == "reason" && x.value() == reason)
            })
            .map(|(_, _, _, v)| match v {
                DebugValue::Counter(c) => c,
                _ => 0,
            })
            .sum()
    }

    fn event_total(&self, event: &str) -> u64 {
        self.snap
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(ck, _, _, _)| ck.key().name() == "pharos_cast_seat_total")
            .filter(|(ck, _, _, _)| {
                ck.key()
                    .labels()
                    .any(|x| x.key() == "event" && x.value() == event)
            })
            .map(|(_, _, _, v)| match v {
                DebugValue::Counter(c) => c,
                _ => 0,
            })
            .sum()
    }

    fn tv_token(&self) -> String {
        self.living_room
            .commands()
            .first()
            .map(|c| c.access_token.expose().to_owned())
            .expect("the TV was sent a command carrying its token")
    }

    async fn token_status(&self, token: &str) -> StatusCode {
        let state = web::Data::new(AppState::new(self.stores.clone(), "srv".into()));
        let app = test::init_service(common::build_app(state)).await;
        let req = test::TestRequest::get()
            .uri("/Users/Me")
            .insert_header((
                "Authorization",
                format!(
                    "MediaBrowser Client=\"Chromecast\", Device=\"Chromecast\", \
                     DeviceId=\"dGlr\", Version=\"3.0\", Token=\"{token}\""
                ),
            ))
            .to_request();
        test::call_service(&app, req).await.status()
    }
}

#[derive(Clone, Copy)]
enum Who {
    Bob,
    Owner,
}

async fn eventually<F: FnMut() -> bool>(what: &str, mut f: F) {
    for _ in 0..250 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for: {what}");
}

async fn wait_msg(
    rx: &mut mpsc::Receiver<ServerMsg>,
    what: &str,
    pred: impl Fn(&ServerMsg) -> bool,
) -> ServerMsg {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Some(m)) if pred(&m) => return m,
            Ok(Some(_)) => {}
            _ => panic!("timed out waiting for: {what}"),
        }
    }
}

fn play_now_position_ms(h: &FakeReceiverHandle) -> Option<(String, i64)> {
    h.commands().into_iter().find_map(|c| match c.body {
        CommandBody::PlayNow(o) => {
            Some((o.items.first()?.id.clone(), o.start_position_ticks / 10_000))
        }
        _ => None,
    })
}

fn has_command(h: &FakeReceiverHandle, name: &str) -> bool {
    h.commands().iter().any(|c| c.body.name() == name)
}

#[tokio::test]
async fn start_plays_the_group_item_on_the_tv_and_shows_the_owner_once() {
    let mut rig = Rig::new().await;
    rig.start_owner().await;

    eventually("PlayNow on the emulator", || {
        play_now_position_ms(&rig.living_room).is_some()
    })
    .await;
    let (item, pos) = play_now_position_ms(&rig.living_room).unwrap();
    assert_eq!(item, ITEM);
    assert!(
        (pos - START_MS as i64).abs() < 15_000,
        "PlayNow at the group position, got {pos} ms"
    );

    let roster = rig.roster_seen_by(Who::Bob).await;
    let ali: Vec<_> = roster.iter().filter(|m| m.name == "ali").collect();
    assert_eq!(ali.len(), 1, "the owner appears once: {roster:?}");
    assert_eq!(ali[0].device.as_deref(), Some(TV));
    assert_eq!(roster.len(), 2, "owner + bob, no extra seat: {roster:?}");

    let casts = rig.service.snapshot().await;
    assert_eq!(casts.len(), 1);
    assert_eq!(casts[0].tv, TV);
    assert_eq!(casts[0].seat_member, rig.seat_member());
    assert_eq!(casts[0].owner_member, rig.owner.member);
    assert_eq!(rig.seat_total("started", "user_stop"), 1);
}

#[tokio::test]
async fn other_members_never_keep_a_stale_seat_entry() {
    let mut rig = Rig::new().await;
    while rig.bob.rx.try_recv().is_ok() {}
    rig.start_owner().await;
    let seat = rig.seat_member();
    wait_msg(
        &mut rig.bob.rx,
        "the seat's join announcement",
        |m| matches!(m, ServerMsg::MemberJoined { member } if member.member_id == seat),
    )
    .await;
    wait_msg(
        &mut rig.bob.rx,
        "the seat's retraction (T059)",
        |m| matches!(m, ServerMsg::MemberLeft { member_id, .. } if *member_id == seat),
    )
    .await;
}

#[tokio::test]
async fn each_reject_carries_its_reason_and_is_recorded() {
    let mut rig = Rig::new().await;

    // not_in_group: a user with a socket but no group.
    let loner_id = new_user(&rig.stores, "loner").await;
    let key = format!("{}:dev-loner", loner_id.0.simple());
    let (tx, _rx) = mpsc::channel(8);
    rig.hub.register(key.clone(), "loner".into(), tx);
    let err = rig
        .service
        .start(StartCast {
            user_id: loner_id,
            user_name: "loner".into(),
            owner_sync_key: key,
            tv_name: TV.into(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.reason, CastRejectReason::NotInGroup);
    assert!(!err.message.is_empty());
    assert_eq!(rig.seat_total("rejected", "not_in_group"), 1);

    // not_permitted: the TV is limited to someone else.
    let err = rig
        .service
        .start(Rig::request(&rig.owner, "Locked"))
        .await
        .unwrap_err();
    assert_eq!(err.reason, CastRejectReason::NotPermitted);
    assert!(err.message.contains("Locked"), "{}", err.message);
    assert_eq!(rig.seat_total("rejected", "not_permitted"), 1);

    // tv_unreachable
    rig.factory.set_unreachable("Den", true);
    let err = rig
        .service
        .start(Rig::request(&rig.owner, "Den"))
        .await
        .unwrap_err();
    assert_eq!(err.reason, CastRejectReason::TvUnreachable);
    assert_eq!(rig.seat_total("rejected", "tv_unreachable"), 1);
    rig.factory.set_unreachable("Den", false);

    // tv_busy: another app owns the screen; the message names it.
    rig.factory.set_occupancy(
        "Den",
        Occupancy::ForeignApp {
            app_id: "CC1AD845".into(),
            display_name: Some("Default Media Receiver".into()),
        },
    );
    let err = rig
        .service
        .start(Rig::request(&rig.owner, "Den"))
        .await
        .unwrap_err();
    assert_eq!(err.reason, CastRejectReason::TvBusy);
    assert!(err.message.contains("CC1AD845"), "{}", err.message);
    assert_eq!(rig.seat_total("rejected", "tv_busy"), 1);
    rig.factory.set_occupancy("Den", Occupancy::Idle);

    // user_has_seat, then tv_busy for a TV another seat already holds.
    rig.start_owner().await;
    let err = rig
        .service
        .start(Rig::request(&rig.owner, "Den"))
        .await
        .unwrap_err();
    assert_eq!(err.reason, CastRejectReason::UserHasSeat);
    assert!(err.message.contains(TV), "{}", err.message);
    assert_eq!(rig.seat_total("rejected", "user_has_seat"), 1);

    let err = rig
        .service
        .start(Rig::request(&rig.bob, TV))
        .await
        .unwrap_err();
    assert_eq!(err.reason, CastRejectReason::TvBusy);
    assert_eq!(rig.seat_total("rejected", "tv_busy"), 2);
    let _ = rig.roster_seen_by(Who::Owner).await;
}

#[tokio::test]
async fn group_pause_and_seek_reach_the_tv() {
    let rig = Rig::new().await;
    rig.start_owner().await;
    eventually("TV loaded", || rig.living_room.state().playing).await;

    rig.group
        .tx
        .send(GroupMsg::PauseShared {
            sender: rig.bob.member,
        })
        .await
        .unwrap();
    eventually("TV paused", || rig.living_room.state().paused).await;

    rig.group
        .tx
        .send(GroupMsg::SeekTo {
            sender: rig.bob.member,
            position_ms: 900_000,
        })
        .await
        .unwrap();
    eventually("TV seek", || {
        has_command(&rig.living_room, "Seek")
            && (rig.living_room.state().position_ms - 900_000).abs() < 15_000
    })
    .await;
}

#[tokio::test]
async fn pausing_on_the_tv_remote_pauses_the_whole_group() {
    let mut rig = Rig::new().await;
    rig.start_owner().await;
    eventually("TV playing", || rig.living_room.state().playing).await;
    while rig.bob.rx.try_recv().is_ok() {}

    // The settle window ignores reports that echo our own PlayNow.
    tokio::time::sleep(Duration::from_millis(2_200)).await;
    rig.living_room.user_pause();
    wait_msg(&mut rig.bob.rx, "group Pause after the TV remote", |m| {
        matches!(m, ServerMsg::Pause { .. })
    })
    .await;

    rig.living_room.user_unpause();
    wait_msg(&mut rig.bob.rx, "group Play after the TV remote", |m| {
        matches!(m, ServerMsg::Play { .. })
    })
    .await;
}

#[tokio::test]
async fn seeking_on_the_tv_remote_seeks_the_whole_group() {
    let mut rig = Rig::new().await;
    rig.start_owner().await;
    eventually("TV playing", || rig.living_room.state().playing).await;
    tokio::time::sleep(Duration::from_millis(2_200)).await;
    while rig.bob.rx.try_recv().is_ok() {}

    rig.living_room.user_seek(Duration::from_secs(1_800));
    let seek = wait_msg(&mut rig.bob.rx, "group Seek after the TV remote", |m| {
        matches!(m, ServerMsg::Seek { .. })
    })
    .await;
    match seek {
        ServerMsg::Seek { position_ms, .. } => {
            assert!(
                (position_ms as i64 - 1_800_000).abs() < 5_000,
                "{position_ms}"
            )
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn stopping_on_the_tv_ends_the_cast_with_user_stop() {
    let mut rig = Rig::new().await;
    rig.start_owner().await;
    eventually("TV playing", || rig.living_room.state().playing).await;
    tokio::time::sleep(Duration::from_millis(2_200)).await;

    rig.living_room.user_stop();
    let svc = rig.service.clone();
    for _ in 0..250 {
        if svc.snapshot().await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(rig.service.snapshot().await.is_empty(), "cast ended");
    assert_eq!(rig.seat_total("ended", "user_stop"), 1);
    assert_eq!(rig.event_total("ended"), 1);
    let roster = rig.roster_seen_by(Who::Owner).await;
    assert!(roster.iter().all(|m| m.device.is_none()), "{roster:?}");
}

#[tokio::test]
async fn end_restores_everything_and_revokes_the_token() {
    let mut rig = Rig::new().await;
    rig.start_owner().await;
    eventually("PlayNow", || has_command(&rig.living_room, "PlayNow")).await;
    let token = rig.tv_token();
    assert_eq!(rig.token_status(&token).await, StatusCode::OK);

    rig.service.stop(rig.owner.id, CastReason::UserStop).await;
    rig.service.stop(rig.owner.id, CastReason::UserStop).await; // idempotent

    assert!(rig.service.snapshot().await.is_empty());
    assert_eq!(
        rig.token_status(&token).await,
        StatusCode::UNAUTHORIZED,
        "the TV token no longer authenticates"
    );
    eventually("Stop sent to the TV", || {
        has_command(&rig.living_room, "Stop")
    })
    .await;
    let roster = rig.roster_seen_by(Who::Bob).await;
    assert_eq!(roster.len(), 2, "{roster:?}");
    assert!(roster.iter().all(|m| m.device.is_none()), "{roster:?}");
    assert!(
        !rig.sinks.contains(rig.seat_member()),
        "the seat's sink is gone"
    );

    // The owner is a Player again, so a Seek waits for the owner's Ready.
    while rig.bob.rx.try_recv().is_ok() {}
    while rig.owner.rx.try_recv().is_ok() {}
    rig.group
        .tx
        .send(GroupMsg::SeekTo {
            sender: rig.bob.member,
            position_ms: 300_000,
        })
        .await
        .unwrap();
    wait_msg(&mut rig.owner.rx, "Seek reaches the owner", |m| {
        matches!(m, ServerMsg::Seek { .. })
    })
    .await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    rig.group
        .tx
        .send(GroupMsg::MemberReady {
            member_id: rig.bob.member,
            position_ms: 300_000,
            playlist_item_id: None,
            is_playing: false,
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (tx, rx) = oneshot::channel();
    rig.group
        .tx
        .send(GroupMsg::Snapshot { reply: tx })
        .await
        .unwrap();
    assert_eq!(
        rx.await.unwrap().play_state,
        GroupPlayState::Waiting,
        "the restored owner must hold the gate like any Player"
    );

    // The owner can cast again: the seat slot and the user slot were freed.
    // (The emulator was consumed, so register a fresh TV for the second run.)
    let again = rig.factory.add_tv(TV);
    rig.start_owner().await;
    eventually("second PlayNow", || has_command(&again, "PlayNow")).await;
}

#[tokio::test]
async fn a_dropped_connection_that_comes_back_keeps_the_seat() {
    let rig = Rig::new().await;
    rig.start_owner().await;
    eventually("TV playing", || rig.living_room.state().playing).await;

    rig.living_room.drop_connection();
    eventually("seat back to Following", || {
        has_command_count(&rig.living_room, "PlayNow") >= 2
    })
    .await;
    let casts = rig.service.snapshot().await;
    assert_eq!(casts.len(), 1, "the seat survives a reconnect inside grace");
    for _ in 0..100 {
        if rig.service.snapshot().await[0].state == ConnState::Following {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the seat never returned to Following");
}

fn has_command_count(h: &FakeReceiverHandle, name: &str) -> usize {
    h.commands()
        .iter()
        .filter(|c| c.body.name() == name)
        .count()
}

#[tokio::test]
async fn grace_expiry_ends_the_cast_and_restores_the_owner() {
    let mut rig = Rig::new().await;
    rig.start_owner().await;
    eventually("PlayNow", || has_command(&rig.living_room, "PlayNow")).await;
    let token = rig.tv_token();

    rig.living_room.configure(|c| c.refuse_connect = true);
    rig.living_room.drop_connection();

    for _ in 0..400 {
        if rig.service.snapshot().await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(rig.service.snapshot().await.is_empty(), "cast ended");
    assert_eq!(rig.seat_total("ended", "grace_expired"), 1);
    assert_eq!(rig.event_total("ended"), 1);
    assert_eq!(rig.token_status(&token).await, StatusCode::UNAUTHORIZED);
    let roster = rig.roster_seen_by(Who::Bob).await;
    assert!(roster.iter().all(|m| m.device.is_none()), "{roster:?}");
    assert_eq!(roster.len(), 2, "{roster:?}");
}

#[tokio::test]
async fn a_dissolved_group_ends_the_cast_with_group_gone() {
    let rig = Rig::new().await;
    rig.start_owner().await;
    eventually("PlayNow", || has_command(&rig.living_room, "PlayNow")).await;
    let token = rig.tv_token();

    for m in [rig.bob.member, rig.owner.member, rig.seat_member()] {
        rig.group
            .tx
            .send(GroupMsg::RemoveMember { member_id: m })
            .await
            .unwrap();
    }
    for _ in 0..250 {
        if rig.service.snapshot().await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(rig.service.snapshot().await.is_empty(), "cast ended");
    assert_eq!(rig.seat_total("ended", "group_gone"), 1);
    assert_eq!(rig.event_total("ended"), 1);
    assert_eq!(rig.token_status(&token).await, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_service_ended_cast_is_recorded_once_with_its_true_reason() {
    for (reason, label) in [
        (CastReason::UserStop, "user_stop"),
        (CastReason::OwnerLeft, "owner_left"),
    ] {
        let rig = Rig::new().await;
        rig.start_owner().await;
        eventually("PlayNow", || has_command(&rig.living_room, "PlayNow")).await;

        rig.service.stop(rig.owner.id, reason).await;

        assert!(rig.service.snapshot().await.is_empty());
        assert_eq!(rig.seat_total("ended", label), 1, "{label}");
        assert_eq!(rig.event_total("ended"), 1, "{label}: one Ended per end");
    }
}
