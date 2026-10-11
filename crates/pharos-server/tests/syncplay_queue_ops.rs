#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `/SyncPlay/Queue`, `/RemoveFromPlaylist` and `/MovePlaylistItem` over HTTP
//! with two members in one group: the command posted by one member must reach
//! BOTH sockets as a `PlayQueue` update carrying the edited queue.

use actix_web::{test, web, App};
use pharos_core::{SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};
use pharos_sync::group::GroupMsg;
use pharos_sync::messages::ServerMsg;
use pharos_sync::{GroupRegistry, LocalDelivery, MemberSinks, SessionHub};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

const DEV_A: &str = "dev-a";
const DEV_B: &str = "dev-b";

/// The queue as a client sees it after a `PlayQueue` update.
#[derive(Debug, PartialEq)]
struct QueueView {
    reason: String,
    item_ids: Vec<String>,
    playlist_item_ids: Vec<String>,
    playing_index: usize,
}

struct Party {
    token: String,
    state: web::Data<AppState>,
    registry: GroupRegistry,
    hub: SessionHub,
    member_sinks: MemberSinks,
    rx_a: mpsc::Receiver<ServerMsg>,
    rx_b: mpsc::Receiver<ServerMsg>,
    /// The hub key of device B, for tests that take it out of its group.
    key_b: String,
}

/// Two devices of one user, both members of one real group actor.
async fn party() -> Party {
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
    let token = stores.issue(uid, "t").await.unwrap().0.expose().to_string();
    let state = web::Data::new(AppState::new(stores, "t".into()));

    let hub = SessionHub::new();
    let member_sinks = MemberSinks::new();
    let registry = GroupRegistry::spawn(Arc::new(LocalDelivery::new(member_sinks.clone())));
    let handle = registry.create().await.unwrap();

    let mut rxs = Vec::new();
    for dev in [DEV_A, DEV_B] {
        let key = format!("{}:{dev}", uid.0.simple());
        let (tx, rx) = mpsc::channel(64);
        let reg = hub.register(key.clone(), "lace".into(), tx.clone());
        member_sinks.insert(reg.member_id, reg.gen, tx);
        hub.attach_group(&key, handle.clone());
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
        rxs.push(rx);
    }
    let rx_b = rxs.pop().unwrap();
    let rx_a = rxs.pop().unwrap();
    Party {
        token,
        state,
        registry,
        hub,
        member_sinks,
        rx_a,
        rx_b,
        key_b: format!("{}:{DEV_B}", uid.0.simple()),
    }
}

/// Build the real app over the party's state and bind `$app`, `$token`, and
/// the two receivers (an `App` has no nameable type to return from a helper).
macro_rules! serve {
    ($party:expr => $app:ident, $token:ident, $rx_a:ident, $rx_b:ident) => {
        let Party {
            token: $token,
            state,
            registry,
            hub,
            member_sinks,
            rx_a: mut $rx_a,
            rx_b: mut $rx_b,
            key_b: _,
        } = $party;
        let $app = test::init_service(
            App::new()
                .app_data(state)
                .app_data(web::Data::new(registry))
                .app_data(web::Data::new(hub))
                .app_data(web::Data::new(member_sinks))
                .wrap(LowercasePath)
                .configure(jellyfin::configure),
        )
        .await;
    };
}

/// POST `$body` to `$path` as `$device`; evaluates to the status code.
macro_rules! post {
    ($app:expr, $token:expr, $device:expr, $path:expr, $body:expr) => {
        test::call_service(
            &$app,
            test::TestRequest::post()
                .uri($path)
                .insert_header(("X-Emby-Token", $token.as_str()))
                .insert_header(("X-Emby-Device-Id", $device))
                .set_json($body)
                .to_request(),
        )
        .await
        .status()
        .as_u16()
    };
}

/// The next `PlayQueue` on `rx` with the given engine reason.
async fn next_queue(rx: &mut mpsc::Receiver<ServerMsg>, reason: &str) -> QueueView {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(ServerMsg::PlayQueue {
                reason: r,
                items,
                playing_index,
                ..
            })) if r == reason => {
                return QueueView {
                    reason: r,
                    item_ids: items.iter().map(|i| i.item_id.clone()).collect(),
                    playlist_item_ids: items.into_iter().map(|i| i.playlist_item_id).collect(),
                    playing_index,
                }
            }
            Ok(Some(_)) => continue,
            _ => panic!("no PlayQueue with reason {reason:?} within 2s"),
        }
    }
}

/// Member A sets a three-entry queue; evaluates to the queue both hold.
macro_rules! seeded {
    ($app:expr, $token:expr, $rx_a:expr, $rx_b:expr) => {{
        let status = post!(
            $app,
            $token,
            DEV_A,
            "/SyncPlay/SetNewQueue",
            serde_json::json!({
                "PlayingQueue": ["10", "11", "12"],
                "PlayingItemPosition": 0,
                "StartPositionTicks": 0,
            })
        );
        assert_eq!(status, 204);
        let a = next_queue(&mut $rx_a, "new_playlist").await;
        let b = next_queue(&mut $rx_b, "new_playlist").await;
        assert_eq!(a, b);
        a
    }};
}

#[actix_web::test]
async fn queue_posted_by_one_member_reaches_both_with_the_new_entries() {
    serve!(party().await => app, token, rx_a, rx_b);
    let before = seeded!(app, token, rx_a, rx_b);

    let status = post!(
        app,
        token,
        DEV_B,
        "/SyncPlay/Queue",
        serde_json::json!({ "ItemIds": ["20", "21"], "Mode": "Queue" })
    );
    assert_eq!(status, 204);

    for rx in [&mut rx_a, &mut rx_b] {
        let q = next_queue(rx, "queue").await;
        assert_eq!(q.item_ids, ["10", "11", "12", "20", "21"]);
        assert_eq!(
            &q.playlist_item_ids[..3],
            before.playlist_item_ids.as_slice()
        );
        assert_eq!(q.playing_index, 0);
    }
}

#[actix_web::test]
async fn queue_next_inserts_after_the_playing_entry_for_both_members() {
    serve!(party().await => app, token, rx_a, rx_b);
    let _ = seeded!(app, token, rx_a, rx_b);

    let status = post!(
        app,
        token,
        DEV_B,
        "/SyncPlay/Queue",
        serde_json::json!({ "ItemIds": ["20"], "Mode": "QueueNext" })
    );
    assert_eq!(status, 204);

    for rx in [&mut rx_a, &mut rx_b] {
        let q = next_queue(rx, "queue_next").await;
        assert_eq!(q.item_ids, ["10", "20", "11", "12"]);
        assert_eq!(q.playing_index, 0);
    }
}

#[actix_web::test]
async fn queue_rejects_oversized_and_malformed_requests_with_400() {
    serve!(party().await => app, token, rx_a, rx_b);
    let _ = seeded!(app, token, rx_a, rx_b);

    let too_many: Vec<String> = (0..1_001).map(|i| i.to_string()).collect();
    let status = post!(
        app,
        token,
        DEV_B,
        "/SyncPlay/Queue",
        serde_json::json!({ "ItemIds": too_many, "Mode": "Queue" })
    );
    assert_eq!(status, 400, "more items than one request may carry");

    let status = post!(
        app,
        token,
        DEV_B,
        "/SyncPlay/Queue",
        serde_json::json!({ "ItemIds": ["20", "not-an-item-id"], "Mode": "Queue" })
    );
    assert_eq!(status, 400, "an id that is not an item id");

    // Neither request may have reached the group.
    let _ = post!(
        app,
        token,
        DEV_B,
        "/SyncPlay/Queue",
        serde_json::json!({ "ItemIds": ["21"], "Mode": "Queue" })
    );
    let q = next_queue(&mut rx_a, "queue").await;
    assert_eq!(q.item_ids, ["10", "11", "12", "21"]);
}

#[actix_web::test]
async fn queue_rejects_requests_that_overflow_the_group_queue_with_400() {
    serve!(party().await => app, token, rx_a, rx_b);
    let _ = seeded!(app, token, rx_a, rx_b);

    // 3 seeded + 4 x 1000 fits under the 5000 cap; one more batch does not.
    let batch: Vec<String> = (0..1_000).map(|i| (100 + i).to_string()).collect();
    for _ in 0..4 {
        let status = post!(
            app,
            token,
            DEV_B,
            "/SyncPlay/Queue",
            serde_json::json!({ "ItemIds": batch, "Mode": "Queue" })
        );
        assert_eq!(status, 204);
        let _ = next_queue(&mut rx_a, "queue").await;
        let _ = next_queue(&mut rx_b, "queue").await;
    }
    let status = post!(
        app,
        token,
        DEV_B,
        "/SyncPlay/Queue",
        serde_json::json!({ "ItemIds": batch, "Mode": "Queue" })
    );
    assert_eq!(status, 400, "the group queue is at its cap");
}

#[actix_web::test]
async fn queue_from_a_caller_in_no_group_is_answered_not_in_group() {
    let p = party().await;
    p.hub.detach_group(&p.key_b);
    serve!(p => app, token, rx_a, rx_b);
    let _ = &mut rx_a;

    let status = post!(
        app,
        token,
        DEV_B,
        "/SyncPlay/Queue",
        serde_json::json!({ "ItemIds": ["20"], "Mode": "Queue" })
    );
    assert_eq!(status, 204);
    let deadline = Duration::from_secs(2);
    let got = tokio::time::timeout(deadline, async {
        while let Some(m) = rx_b.recv().await {
            if matches!(m, ServerMsg::NotInGroup) {
                return true;
            }
        }
        false
    })
    .await;
    assert_eq!(
        got,
        Ok(true),
        "the group-less caller must be told NotInGroup"
    );
}

/// The SDKs send playlist item ids as dashed `Guid`s; the engine mints them
/// dashless.
fn as_guid(pli: &str) -> String {
    uuid::Uuid::parse_str(pli).unwrap().hyphenated().to_string()
}

#[actix_web::test]
async fn remove_from_playlist_posted_by_one_member_reaches_both() {
    serve!(party().await => app, token, rx_a, rx_b);
    let before = seeded!(app, token, rx_a, rx_b);

    let status = post!(
        app,
        token,
        DEV_B,
        "/SyncPlay/RemoveFromPlaylist",
        serde_json::json!({ "PlaylistItemIds": [as_guid(&before.playlist_item_ids[1])] })
    );
    assert_eq!(status, 204);

    for rx in [&mut rx_a, &mut rx_b] {
        let q = next_queue(rx, "remove_items").await;
        assert_eq!(q.item_ids, ["10", "12"]);
        assert_eq!(
            q.playlist_item_ids,
            [
                before.playlist_item_ids[0].clone(),
                before.playlist_item_ids[2].clone()
            ]
        );
        assert_eq!(q.playing_index, 0);
    }
}

#[actix_web::test]
async fn move_playlist_item_posted_by_one_member_reaches_both() {
    serve!(party().await => app, token, rx_a, rx_b);
    let before = seeded!(app, token, rx_a, rx_b);

    let status = post!(
        app,
        token,
        DEV_B,
        "/SyncPlay/MovePlaylistItem",
        serde_json::json!({
            "PlaylistItemId": as_guid(&before.playlist_item_ids[2]),
            "NewIndex": 0,
        })
    );
    assert_eq!(status, 204);

    for rx in [&mut rx_a, &mut rx_b] {
        let q = next_queue(rx, "move_item").await;
        assert_eq!(q.item_ids, ["12", "10", "11"]);
        assert_eq!(q.playlist_item_ids[0], before.playlist_item_ids[2]);
        assert_eq!(q.playing_index, 1, "the playing entry follows its item");
    }
}

#[actix_web::test]
async fn move_playlist_item_without_an_id_is_a_no_op_204() {
    serve!(party().await => app, token, rx_a, rx_b);
    let _ = seeded!(app, token, rx_a, rx_b);

    let status = post!(
        app,
        token,
        DEV_B,
        "/SyncPlay/MovePlaylistItem",
        serde_json::json!({ "NewIndex": 1 })
    );
    assert_eq!(status, 204, "Jellyfin binds the missing id to Guid.Empty");
    let _ = post!(
        app,
        token,
        DEV_B,
        "/SyncPlay/Queue",
        serde_json::json!({ "ItemIds": ["20"], "Mode": "Queue" })
    );
    let q = next_queue(&mut rx_a, "queue").await;
    assert_eq!(q.item_ids, ["10", "11", "12", "20"], "nothing moved");
}

#[actix_web::test]
async fn clear_playlist_with_clear_playing_item_empties_the_queue_for_both() {
    serve!(party().await => app, token, rx_a, rx_b);
    let _ = seeded!(app, token, rx_a, rx_b);

    let status = post!(
        app,
        token,
        DEV_B,
        "/SyncPlay/RemoveFromPlaylist",
        serde_json::json!({ "ClearPlaylist": true, "ClearPlayingItem": true })
    );
    assert_eq!(status, 204);

    for rx in [&mut rx_a, &mut rx_b] {
        let q = next_queue(rx, "remove_items").await;
        assert!(q.item_ids.is_empty());
    }
}
