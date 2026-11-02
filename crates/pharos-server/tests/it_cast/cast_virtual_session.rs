#![allow(clippy::unwrap_used, clippy::expect_used)]
//! 009 US1 — configured TVs appear to stock clients as sessions, so
//! jellyfin-web's "play on another device" picker can list them, with the
//! same values on `GET /Sessions` and the socket `Sessions` patch.

use actix_test::TestServer;
use actix_web::{web, App};
use futures_util::StreamExt;
use pharos_core::{SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    cast_sessions::{tv_session_id, ActiveCastInfo, CastDirectory},
    config::{CastConfig, Config},
    middleware::LowercasePath,
    state::{AppState, Stores},
};
use std::collections::HashMap;
use std::sync::Arc;

struct FakeDirectory {
    cfg: CastConfig,
    reachable: HashMap<String, bool>,
    active: Vec<ActiveCastInfo>,
}

impl CastDirectory for FakeDirectory {
    fn config(&self) -> &CastConfig {
        &self.cfg
    }
    fn reachable(&self) -> HashMap<String, bool> {
        self.reachable.clone()
    }
    fn active(&self) -> Vec<ActiveCastInfo> {
        self.active.clone()
    }
}

fn cast_config(restricted_to: &str) -> CastConfig {
    Config::from_toml_str(&format!(
        r#"
        [server]
        bind = "0.0.0.0:8096"
        name = "pharos-test"
        [obs]
        log_level = "debug"
        [media]
        roots = ["/srv/media"]
        [database]
        url = "sqlite::memory:"
        [cast]
        server_url = "http://pharos.lan:8096"
        [[cast.tvs]]
        name = "Living room"
        address = "192.168.1.60:8009"
        [[cast.tvs]]
        name = "Bedroom"
        address = "192.168.1.61:8009"
        allowed_users = ["{restricted_to}"]
        "#
    ))
    .unwrap()
    .cast
}

struct Rig {
    srv: TestServer,
    admin_token: String,
    alice_token: String,
    alice_id: String,
    bob_token: String,
}

async fn add_user(stores: &Stores, name: &str, admin: bool) -> (UserId, String) {
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("p")).unwrap();
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: name.into(),
            password_hash: hash,
            policy: UserPolicy {
                admin,
                ..Default::default()
            },
        })
        .await
        .unwrap();
    let token = stores.issue(uid, "test").await.unwrap();
    (uid, token.0.expose().to_string())
}

/// Alice and an admin may use the restricted Bedroom TV; Bob may not.
/// `directory` receives the users' ids so a test can mark a cast as theirs.
async fn boot(
    reachable: &[(&str, bool)],
    active: impl FnOnce(&str) -> Vec<ActiveCastInfo>,
    configured: bool,
) -> Rig {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let (_, admin_token) = add_user(&stores, "root", true).await;
    let (alice, alice_token) = add_user(&stores, "alice", false).await;
    let (_, bob_token) = add_user(&stores, "bob", false).await;
    let alice_id = alice.0.simple().to_string();
    let mut state = AppState::new(stores, "t".into());
    if configured {
        state.cast_directory = Some(Arc::new(FakeDirectory {
            cfg: cast_config(&alice_id),
            reachable: reachable
                .iter()
                .map(|(n, r)| ((*n).to_string(), *r))
                .collect(),
            active: active(&alice_id),
        }));
    }
    let state = web::Data::new(state);
    let member_sinks = pharos_sync::MemberSinks::new();
    let registry = web::Data::new(pharos_sync::GroupRegistry::spawn(Arc::new(
        pharos_sync::LocalDelivery::new(member_sinks.clone()),
    )));
    let member_sinks = web::Data::new(member_sinks);
    let srv = actix_test::start(move || {
        App::new()
            .app_data(state.clone())
            .app_data(registry.clone())
            .app_data(web::Data::new(pharos_sync::SessionHub::new()))
            .app_data(member_sinks.clone())
            .wrap(LowercasePath)
            .configure(jellyfin::configure)
    });
    Rig {
        srv,
        admin_token,
        alice_token,
        alice_id,
        bob_token,
    }
}

async fn get_sessions(rig: &Rig, token: &str) -> String {
    let mut resp = awc::Client::default()
        .get(rig.srv.url("/Sessions"))
        .insert_header(("X-Emby-Token", token))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    String::from_utf8(resp.body().await.unwrap().to_vec()).unwrap()
}

async fn rest_sessions(rig: &Rig, token: &str) -> Vec<serde_json::Value> {
    serde_json::from_str(&get_sessions(rig, token).await).unwrap()
}

fn names(sessions: &[serde_json::Value]) -> Vec<&str> {
    sessions
        .iter()
        .map(|s| s["DeviceName"].as_str().unwrap_or(""))
        .collect()
}

const BOTH_UP: [(&str, bool); 2] = [("Living room", true), ("Bedroom", true)];

#[actix_web::test]
async fn lists_a_configured_reachable_permitted_tv() {
    let rig = boot(&BOTH_UP, |_| Vec::new(), true).await;
    let list = rest_sessions(&rig, &rig.alice_token).await;
    let tv = list
        .iter()
        .find(|s| s["DeviceName"] == "Living room")
        .expect("Living room listed");
    assert_eq!(tv["Id"], tv_session_id("Living room"));
    assert_eq!(tv["Client"], "Chromecast");
    assert_eq!(tv["SupportsRemoteControl"], true);
    assert_eq!(tv["SupportsMediaControl"], true);
    assert_eq!(
        tv["PlayableMediaTypes"],
        serde_json::json!(["Audio", "Video"])
    );
    assert_eq!(tv["UserId"], "", "no owner while idle");
    assert_eq!(tv["UserName"], "");
    assert!(tv.get("NowPlayingItemId").is_none() || tv["NowPlayingItemId"].is_null());
}

#[actix_web::test]
async fn hidden_when_unreachable() {
    let rig = boot(
        &[("Living room", false), ("Bedroom", true)],
        |_| Vec::new(),
        true,
    )
    .await;
    let list = rest_sessions(&rig, &rig.alice_token).await;
    assert_eq!(names(&list), vec!["Bedroom"]);
    let missing = boot(&[("Bedroom", true)], |_| Vec::new(), true).await;
    assert_eq!(
        names(&rest_sessions(&missing, &missing.alice_token).await),
        vec!["Bedroom"],
        "a TV the directory has no verdict on is not listed"
    );
}

#[actix_web::test]
async fn hidden_from_a_non_permitted_user_but_visible_to_an_admin() {
    let rig = boot(&BOTH_UP, |_| Vec::new(), true).await;
    assert_eq!(
        names(&rest_sessions(&rig, &rig.bob_token).await),
        vec!["Living room"],
        "Bob is not in Bedroom's allowed_users (V9)"
    );
    assert_eq!(
        names(&rest_sessions(&rig, &rig.alice_token).await),
        vec!["Living room", "Bedroom"]
    );
    assert_eq!(
        names(&rest_sessions(&rig, &rig.admin_token).await),
        vec!["Living room", "Bedroom"]
    );
}

#[actix_web::test]
async fn an_active_cast_shows_its_owner_and_item() {
    let rig = boot(
        &BOTH_UP,
        |alice| {
            vec![ActiveCastInfo {
                tv: "Living room".into(),
                owner_user_id: alice.into(),
                owner_user_name: "alice".into(),
                now_playing_item_id: Some("100".into()),
                position_ticks: 42,
                is_paused: true,
            }]
        },
        true,
    )
    .await;
    let list = rest_sessions(&rig, &rig.bob_token).await;
    let tv = &list[0];
    assert_eq!(tv["UserId"], rig.alice_id);
    assert_eq!(tv["UserName"], "alice");
    assert_eq!(tv["NowPlayingItemId"], "100");
    assert_eq!(tv["PositionTicks"], 42);
    assert_eq!(tv["IsPaused"], true);
}

#[actix_web::test]
async fn rest_and_socket_carry_identical_values() {
    let rig = boot(
        &BOTH_UP,
        |alice| {
            vec![ActiveCastInfo {
                tv: "Living room".into(),
                owner_user_id: alice.into(),
                owner_user_name: "alice".into(),
                now_playing_item_id: Some("100".into()),
                position_ticks: 42,
                is_paused: true,
            }]
        },
        true,
    )
    .await;
    let mut srv = rig.srv;
    let token = rig.alice_token.as_str();
    let mut ws = srv
        .ws_at(&format!("/socket?api_key={token}&deviceId=d1"))
        .await
        .unwrap();

    let client = awc::Client::default();
    for (path, body) in [
        (
            "/Sessions/Playing",
            serde_json::json!({"ItemId": "7", "PlaySessionId": "s1", "PositionTicks": 0u64}),
        ),
        (
            "/Sessions/Playing/Progress",
            serde_json::json!({"ItemId": "7", "PlaySessionId": "s1", "PositionTicks": 5u64}),
        ),
    ] {
        let resp = client
            .post(srv.url(path))
            .insert_header(("X-Emby-Token", token))
            .send_json(&body)
            .await
            .unwrap();
        assert_eq!(resp.status(), 204);
    }

    let patch = loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
            .await
            .expect("a Sessions frame within 5s")
            .expect("socket open")
            .expect("frame");
        if let awc::ws::Frame::Text(t) = frame {
            let v: serde_json::Value = serde_json::from_slice(&t).unwrap();
            if v["MessageType"] == "Sessions" {
                break v["Data"].as_array().unwrap().clone();
            }
        }
    };
    let mut listed = client
        .get(srv.url("/Sessions"))
        .insert_header(("X-Emby-Token", token))
        .send()
        .await
        .unwrap();
    let rest: Vec<serde_json::Value> = listed.json().await.unwrap();

    let id = tv_session_id("Living room");
    let rest_tv = rest.iter().find(|s| s["Id"] == id).expect("REST lists TV");
    let socket_tv = patch
        .iter()
        .find(|s| s["Id"] == id)
        .expect("socket patch lists TV");
    for key in [
        "Id",
        "UserId",
        "UserName",
        "DeviceId",
        "DeviceName",
        "Client",
        "IsActive",
        "SupportsMediaControl",
        "SupportsRemoteControl",
        "HasCustomDeviceName",
        "PlayableMediaTypes",
    ] {
        assert_eq!(rest_tv[key], socket_tv[key], "{key} differs");
        assert!(!rest_tv[key].is_null(), "{key} missing on REST");
    }
    assert_eq!(
        rest_tv["PositionTicks"],
        socket_tv["PlayState"]["PositionTicks"]
    );
    assert_eq!(rest_tv["IsPaused"], socket_tv["PlayState"]["IsPaused"]);
    // The viewer's own progress entry is still first and unchanged.
    assert_eq!(patch[0]["Id"], "s1");
}

#[actix_web::test]
async fn id_is_stable_across_requests_and_distinct_per_tv() {
    let rig = boot(&BOTH_UP, |_| Vec::new(), true).await;
    let first = rest_sessions(&rig, &rig.admin_token).await;
    let second = rest_sessions(&rig, &rig.admin_token).await;
    let ids = |l: &[serde_json::Value]| -> Vec<String> {
        l.iter().map(|s| s["Id"].as_str().unwrap().into()).collect()
    };
    assert_eq!(ids(&first), ids(&second));
    assert_ne!(ids(&first)[0], ids(&first)[1]);
    assert_eq!(ids(&first)[0].len(), 32);
    assert!(ids(&first)[0].chars().all(|c| c.is_ascii_hexdigit()));
}

/// Dates and the user id vary per run; blank them so the rest is compared
/// byte for byte.
fn normalise(body: &str, user_id: &str) -> String {
    let v: Vec<serde_json::Value> = serde_json::from_str(body).unwrap();
    let mut out = body.replace(user_id, "<uid>");
    for s in &v {
        for k in ["LastActivityDate", "LastPlaybackCheckIn"] {
            out = out.replace(s[k].as_str().unwrap(), "<date>");
        }
    }
    out
}

async fn one_ordinary_session(rig: &Rig) -> String {
    let resp = awc::Client::default()
        .post(rig.srv.url("/Sessions/Playing"))
        .insert_header(("X-Emby-Token", rig.alice_token.as_str()))
        .send_json(
            &serde_json::json!({"ItemId": "100", "PlaySessionId": "sess-1", "PositionTicks": 0u64}),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    normalise(
        &get_sessions(rig, &rig.alice_token).await,
        rig.alice_id.as_str(),
    )
}

#[actix_web::test]
async fn without_cast_tvs_sessions_are_byte_identical_to_before() {
    // Captured from the build before this feature, with one ordinary session.
    const BASELINE: &str = r#"[{"Id":"sess-1","UserId":"<uid>","UserName":"alice","DeviceId":"unknown","DeviceName":"Unknown","Client":"Unknown","ApplicationVersion":"0","NowPlayingItemId":"100","PositionTicks":0,"IsPaused":false,"SupportsMediaControl":false,"IsActive":true,"SupportsRemoteControl":true,"HasCustomDeviceName":false,"LastActivityDate":"<date>","LastPlaybackCheckIn":"<date>"}]"#;

    let unconfigured = boot(&[], |_| Vec::new(), false).await;
    assert_eq!(one_ordinary_session(&unconfigured).await, BASELINE);

    // A directory with no TVs configured is just as inert.
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let (alice, token) = add_user(&stores, "alice", false).await;
    let mut state = AppState::new(stores, "t".into());
    state.cast_directory = Some(Arc::new(FakeDirectory {
        cfg: CastConfig::default(),
        reachable: HashMap::new(),
        active: Vec::new(),
    }));
    let state = web::Data::new(state);
    let srv = actix_test::start(move || {
        App::new()
            .app_data(state.clone())
            .wrap(LowercasePath)
            .configure(jellyfin::configure)
    });
    let rig = Rig {
        srv,
        admin_token: token.clone(),
        alice_token: token,
        alice_id: alice.0.simple().to_string(),
        bob_token: String::new(),
    };
    assert_eq!(one_ordinary_session(&rig).await, BASELINE);
}
