#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `/socket` push subscriptions: `SessionsStart` and `ActivityLogEntryStart`.

use std::time::Duration;

use actix_web::{web, App};
use awc::ws::{Frame, Message};
use futures_util::{SinkExt, StreamExt};
use pharos_core::{SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    sessions::SessionEvent,
    state::{AppState, Stores},
};
use pharos_sync::{GroupRegistry, LocalDelivery, MemberSinks, SessionHub};
use serde_json::Value;

type Ws = actix_codec::Framed<awc::BoxedSocket, awc::ws::Codec>;

struct Fixture {
    server: actix_test::TestServer,
    state: web::Data<AppState>,
    admin_id: UserId,
    admin_token: String,
    user_id: UserId,
    user_token: String,
}

async fn fixture() -> Fixture {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("p")).unwrap();
    let mut ids = Vec::new();
    for (name, admin) in [("admin", true), ("plain", false)] {
        let id = UserId::new();
        let mut policy = UserPolicy::default();
        policy.admin = admin;
        stores
            .create(UserRecord {
                id,
                name: name.into(),
                password_hash: hash.clone(),
                policy,
            })
            .await
            .unwrap();
        let token = stores.issue(id, name).await.unwrap();
        ids.push((id, token.0.expose().to_string()));
    }
    let state = web::Data::new(AppState::new(stores, "t".into()));
    let member_sinks = MemberSinks::new();
    let registry = web::Data::new(GroupRegistry::spawn(std::sync::Arc::new(
        LocalDelivery::new(member_sinks.clone()),
    )));
    let hub = web::Data::new(SessionHub::new());
    let sinks = web::Data::new(member_sinks);
    let s = state.clone();
    let server = actix_test::start(move || {
        App::new()
            .app_data(s.clone())
            .app_data(registry.clone())
            .app_data(hub.clone())
            .app_data(sinks.clone())
            .configure(jellyfin::configure)
    });
    let (user_id, user_token) = ids.pop().unwrap();
    let (admin_id, admin_token) = ids.pop().unwrap();
    Fixture {
        server,
        state,
        admin_id,
        admin_token,
        user_id,
        user_token,
    }
}

impl Fixture {
    async fn start_playing(&self, user: UserId, name: &str, session: &str) {
        self.state
            .sessions
            .apply(SessionEvent::Started {
                session_id: session.into(),
                user_id: user,
                user_name: name.into(),
                device_id: format!("dev-{name}"),
                device_name: format!("dev-{name}"),
                client: "test".into(),
                version: "1".into(),
                item_id: "1".into(),
                position_ticks: 0,
            })
            .await
            .unwrap();
    }

    async fn connect(&self, token: &str) -> Ws {
        let (_resp, ws) = awc::Client::new()
            .ws(self
                .server
                .url(&format!("/socket?ApiKey={token}"))
                .replacen("http", "ws", 1))
            .connect()
            .await
            .expect("authenticated upgrade");
        ws
    }
}

async fn send(ws: &mut Ws, message_type: &str, data: Value) {
    let frame = serde_json::json!({"MessageType": message_type, "Data": data});
    ws.send(Message::Text(frame.to_string().into()))
        .await
        .unwrap();
}

/// Next JSON frame of `message_type` within `wait`, skipping others.
async fn next_of(ws: &mut Ws, message_type: &str, wait: Duration) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let frame = tokio::time::timeout_at(deadline, ws.next()).await.ok()??;
        if let Ok(Frame::Text(t)) = frame {
            let v: Value = serde_json::from_slice(&t).unwrap();
            if v["MessageType"] == message_type {
                return Some(v);
            }
        }
    }
}

const WAIT: Duration = Duration::from_secs(3);
const QUIET: Duration = Duration::from_millis(1500);

#[actix_web::test]
async fn sessions_start_pushes_all_sessions_to_admin_until_stop() {
    let f = fixture().await;
    f.start_playing(f.admin_id, "admin", "s-admin").await;
    f.start_playing(f.user_id, "plain", "s-plain").await;
    let mut ws = f.connect(&f.admin_token).await;

    send(&mut ws, "SessionsStart", Value::String("0,500".into())).await;
    let push = next_of(&mut ws, "Sessions", WAIT).await.expect("a push");
    assert_eq!(push["Data"].as_array().unwrap().len(), 2, "{push}");
    // periodic, not one-shot
    assert!(next_of(&mut ws, "Sessions", WAIT).await.is_some());

    send(&mut ws, "SessionsStop", Value::Null).await;
    // drain a push already in flight, then expect silence
    let _ = next_of(&mut ws, "Sessions", Duration::from_millis(100)).await;
    assert!(next_of(&mut ws, "Sessions", QUIET).await.is_none());
}

#[actix_web::test]
async fn sessions_start_never_leaks_other_users_sessions_to_non_admin() {
    let f = fixture().await;
    f.start_playing(f.admin_id, "admin", "s-admin").await;
    f.start_playing(f.user_id, "plain", "s-plain").await;
    let mut ws = f.connect(&f.user_token).await;

    send(&mut ws, "SessionsStart", Value::String("0,500".into())).await;
    let push = next_of(&mut ws, "Sessions", WAIT).await.expect("a push");
    let list = push["Data"].as_array().unwrap();
    assert_eq!(list.len(), 1, "{push}");
    assert_eq!(list[0]["UserId"], f.user_id.0.simple().to_string());
}
