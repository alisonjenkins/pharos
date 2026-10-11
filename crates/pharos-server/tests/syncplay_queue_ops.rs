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
