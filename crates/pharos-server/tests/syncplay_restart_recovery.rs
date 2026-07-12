#![allow(clippy::unwrap_used, clippy::expect_used)]
//! B24 — a watch party must survive a server restart / rolling deploy, and a
//! session the server no longer recognises must be TOLD, never silently
//! ignored.
//!
//! Live incident: a deploy restarted the pod mid-episode. Group snapshots
//! were persisted (Phase B4.3c) but the session hub's `deviceId → group` map
//! is in-memory, so the reconnecting sockets registered group-less and every
//! subsequent HTTP command hit "session is in no group — command dropped":
//! one member's Pause paused only their own player (jellyfin-web pauses
//! locally before POSTing) while the other kept playing — a silent desync
//! until the group was recreated by hand.
//!
//! Recovery chain under test:
//! 1. member ids are deterministic per device → a persisted roster still
//!    names a device that reconnects after a restart (pharos-sync unit),
//! 2. `find_persisted_group` maps that member id back to its group id from
//!    the `sync_groups` snapshots (this file),
//! 3. an unrecoverable no-group command sends the client `NotInGroup` so
//!    stock jellyfin-web visibly disables SyncPlay instead of desyncing
//!    (this file).

use actix_web::{test, web, App};
use pharos_core::{
    PersistedSyncGroup, SecretString, SyncGroupStore, TokenStore, UserId, UserPolicy, UserRecord,
    UserStore,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
    sync_recovery::find_persisted_group,
};
use pharos_sync::group::{GroupHandle, GroupMsg};
use pharos_sync::hub::member_id_for_device;
use pharos_sync::messages::{GroupId, MemberId, ServerMsg};
use pharos_sync::persistence::GroupPersistence;
use pharos_sync::{LocalDelivery, MemberSinks, SessionHub};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

/// Capture the actor's persisted snapshot json (the same blob the Postgres
/// path writes to `sync_groups`).
struct Capture(Mutex<Option<String>>);
impl GroupPersistence for Capture {
    fn persist(&self, _g: GroupId, _e: u64, state_json: String) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(state_json);
    }
    fn remove(&self, _g: GroupId) {}
}

/// Run a real group actor, add `device`'s member, and return the group id +
/// the snapshot json its persistence hook produced.
async fn persisted_snapshot_for(device: &str) -> (GroupId, String) {
    let capture = Arc::new(Capture(Mutex::new(None)));
    let sinks = MemberSinks::new();
    let delivery = Arc::new(LocalDelivery::new(sinks.clone()));
    let gid = GroupId::new();
    let h = GroupHandle::spawn_persistent(gid, 1_000, delivery, capture.clone(), None);
    let member = member_id_for_device(device);
    let (sink_tx, _sink_rx) = mpsc::channel(8);
    sinks.insert(member, sink_tx);
    let (rtx, rrx) = oneshot::channel();
    h.tx.send(GroupMsg::AddMember {
        member_id: member,
        name: "alison".into(),
        reply: rtx,
    })
    .await
    .unwrap();
    let _ = rrx.await.unwrap();
    for _ in 0..200 {
        if let Some(json) = capture
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            return (gid, json);
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("group actor never persisted a snapshot");
}

/// After a "restart" (fresh stores + fresh hub — nothing in memory), the
/// persisted snapshot alone must identify the reconnecting device's group.
#[actix_web::test]
async fn persisted_snapshot_recovers_device_membership_after_restart() {
    let (gid, json) = persisted_snapshot_for("alisons-firefox").await;

    // "Restart": a brand-new process has only the store contents. The row is
    // freshly updated (a live party's snapshot rewrites on every mutation),
    // so it sits inside the recovery window.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    stores
        .upsert_sync_group(
            &PersistedSyncGroup {
                group_id: gid.to_string(),
                epoch_unix_ms: 1_000,
                state_json: json,
                updated_at: 0,
            },
            now,
        )
        .await
        .unwrap();

    // The SAME device derives the SAME member id in the new process and finds
    // its group; a foreign device finds nothing.
    assert_eq!(
        find_persisted_group(&stores, member_id_for_device("alisons-firefox")).await,
        Some(gid),
        "reconnecting device recovers its persisted group"
    );
    assert_eq!(
        find_persisted_group(&stores, member_id_for_device("laces-firefox")).await,
        None,
        "device that never joined recovers nothing"
    );
    assert_eq!(
        find_persisted_group(&stores, MemberId::new()).await,
        None,
        "random member id recovers nothing"
    );
}

/// T83 — a snapshot stale past the recovery window must NOT be recovered
/// into: re-attaching a device to last week's leftover group would trap it in
/// a dead party. (`updated_at` is stamped by upsert's `now_unix_secs` param.)
#[actix_web::test]
async fn stale_snapshot_is_not_recovered() {
    let (gid, json) = persisted_snapshot_for("alisons-firefox").await;
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    // Last mutation 3 days ago — outside the 24h recovery window.
    let three_days_ago = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        - 3 * 24 * 3600;
    stores
        .upsert_sync_group(
            &PersistedSyncGroup {
                group_id: gid.to_string(),
                epoch_unix_ms: 1_000,
                state_json: json,
                updated_at: 0,
            },
            three_days_ago,
        )
        .await
        .unwrap();
    assert_eq!(
        find_persisted_group(&stores, member_id_for_device("alisons-firefox")).await,
        None,
        "stale snapshot must be ignored by recovery"
    );
}

/// A group command from a session the server does NOT consider grouped must
/// push `NotInGroup` down that session's socket (jellyfin-web then disables
/// SyncPlay visibly) — the old behaviour was a silent server-side WARN while
/// the sender's local player applied the command one-sidedly.
#[actix_web::test]
async fn no_group_command_sends_not_in_group_to_the_caller() {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("p")).unwrap();
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "alison".into(),
            password_hash: hash,
            policy: UserPolicy::default(),
        })
        .await
        .unwrap();
    let token = stores.issue(uid, "t").await.unwrap();
    let token = token.0.expose().to_string();
    let state = web::Data::new(AppState::new(stores, "t".into()));

    // The device's /socket is connected (registered in the hub) but the hub
    // has no group for it — the exact post-restart state.
    let hub = SessionHub::new();
    let (sink_tx, mut sink_rx) = mpsc::channel(8);
    hub.register("dev-restart".into(), "alison".into(), sink_tx);

    let member_sinks = MemberSinks::new();
    let registry =
        pharos_sync::GroupRegistry::spawn(Arc::new(LocalDelivery::new(member_sinks.clone())));
    let app = test::init_service(
        App::new()
            .app_data(state)
            .app_data(web::Data::new(registry))
            .app_data(web::Data::new(hub))
            .app_data(web::Data::new(member_sinks))
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/SyncPlay/Pause")
            .insert_header(("X-Emby-Token", token.as_str()))
            .insert_header(("X-Emby-Device-Id", "dev-restart"))
            .to_request(),
    )
    .await;
    assert_eq!(
        resp.status(),
        204,
        "command path still 204s (Jellyfin shape)"
    );

    let msg = tokio::time::timeout(std::time::Duration::from_secs(2), sink_rx.recv())
        .await
        .expect("caller's socket must receive a message")
        .expect("sink open");
    assert!(
        matches!(msg, ServerMsg::NotInGroup),
        "expected NotInGroup, got {msg:?}"
    );

    // B25 — /SyncPlay/Leave must acknowledge the LEAVER with GroupLeft even
    // when the server has no group for the session (lace's wedge: client in
    // group mode, server group-less; Leave was a silent 204 → no way out of
    // SyncPlay short of a page reload).
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/SyncPlay/Leave")
            .insert_header(("X-Emby-Token", token.as_str()))
            .insert_header(("X-Emby-Device-Id", "dev-restart"))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 204);
    let msg = tokio::time::timeout(std::time::Duration::from_secs(2), sink_rx.recv())
        .await
        .expect("leaver's socket must receive a message")
        .expect("sink open");
    assert!(
        matches!(msg, ServerMsg::GroupLeft),
        "expected GroupLeft, got {msg:?}"
    );
}

/// T83 — `/SyncPlay/Ping` from a group-less session heals the client with
/// NotInGroup (jellyfin-web pings periodically while it THINKS it's in a
/// group, so a wedged client now exits SyncPlay within one ping interval
/// without the user touching anything).
#[actix_web::test]
async fn ping_from_groupless_session_heals_with_not_in_group() {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("p")).unwrap();
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "lace".into(),
            password_hash: hash,
            policy: UserPolicy::default(),
        })
        .await
        .unwrap();
    let token = stores.issue(uid, "t").await.unwrap();
    let token = token.0.expose().to_string();
    let state = web::Data::new(AppState::new(stores, "t".into()));
    let hub = SessionHub::new();
    let (sink_tx, mut sink_rx) = mpsc::channel(8);
    hub.register("dev-pinger".into(), "lace".into(), sink_tx);
    let member_sinks = MemberSinks::new();
    let registry =
        pharos_sync::GroupRegistry::spawn(Arc::new(LocalDelivery::new(member_sinks.clone())));
    let app = test::init_service(
        App::new()
            .app_data(state)
            .app_data(web::Data::new(registry))
            .app_data(web::Data::new(hub))
            .app_data(web::Data::new(member_sinks))
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/SyncPlay/Ping")
            .insert_header(("X-Emby-Token", token.as_str()))
            .insert_header(("X-Emby-Device-Id", "dev-pinger"))
            .set_json(serde_json::json!({"Ping": 12}))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 204);
    let msg = tokio::time::timeout(std::time::Duration::from_secs(2), sink_rx.recv())
        .await
        .expect("pinger's socket must receive a message")
        .expect("sink open");
    assert!(
        matches!(msg, ServerMsg::NotInGroup),
        "expected NotInGroup, got {msg:?}"
    );
}

/// The NORMAL leave (session actually in a group) must also acknowledge the
/// leaver with GroupLeft — the group's own MemberLeft broadcast fires after
/// the roster removal, so it only ever reaches the remaining members.
#[actix_web::test]
async fn leave_with_group_acknowledges_the_leaver() {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("p")).unwrap();
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "lace".into(),
            password_hash: hash,
            policy: UserPolicy::default(),
        })
        .await
        .unwrap();
    let token = stores.issue(uid, "t").await.unwrap();
    let token = token.0.expose().to_string();
    let state = web::Data::new(AppState::new(stores, "t".into()));

    let hub = SessionHub::new();
    let (sink_tx, mut sink_rx) = mpsc::channel(32);
    let reg = hub.register("dev-leaver".into(), "lace".into(), sink_tx.clone());

    let member_sinks = MemberSinks::new();
    member_sinks.insert(reg.member_id, sink_tx);
    let registry =
        pharos_sync::GroupRegistry::spawn(Arc::new(LocalDelivery::new(member_sinks.clone())));

    // Join a real group the same way /SyncPlay/New does: attach + AddMember.
    let handle = registry.create().await.unwrap();
    hub.attach_group("dev-leaver", handle.clone());
    let (rtx, rrx) = oneshot::channel();
    handle
        .tx
        .send(GroupMsg::AddMember {
            member_id: reg.member_id,
            name: "lace".into(),
            reply: rtx,
        })
        .await
        .unwrap();
    let _ = rrx.await.unwrap();

    let app = test::init_service(
        App::new()
            .app_data(state)
            .app_data(web::Data::new(registry))
            .app_data(web::Data::new(hub))
            .app_data(web::Data::new(member_sinks))
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/SyncPlay/Leave")
            .insert_header(("X-Emby-Token", token.as_str()))
            .insert_header(("X-Emby-Device-Id", "dev-leaver"))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 204);

    // Drain the join-time messages (Joined + catch-up); GroupLeft must arrive.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let msg = tokio::time::timeout(remaining, sink_rx.recv())
            .await
            .expect("leaver must receive GroupLeft before timeout")
            .expect("sink open");
        if matches!(msg, ServerMsg::GroupLeft) {
            break;
        }
    }
}
