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

    // "Restart": a brand-new process has only the store contents.
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    stores
        .upsert_sync_group(
            &PersistedSyncGroup {
                group_id: gid.to_string(),
                epoch_unix_ms: 1_000,
                state_json: json,
                updated_at: 0,
            },
            1,
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
}
