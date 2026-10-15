//! Shared rig for the cast HTTP suites (`cast_start`, `cast_commands`,
//! `cast_end`): a real HTTP server, `GroupRegistry`, `SessionHub`, the cast
//! service with `FakeReceiver` as the TV, and the real `CastDirectory`.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use actix_test::TestServer;
use actix_web::{web, App};
use metrics_util::debugging::{DebugValue, DebuggingRecorder};
use pharos_cast::fake::FakeReceiverHandle;
use pharos_core::{SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore};
use pharos_server::auth::BuiltinAuth;
use pharos_server::cast_directory::RuntimeCastDirectory;
use pharos_server::cast_service::{
    self, CastDeps, CastRuntime, CastServiceHandle, FakeTransportFactory,
};
use pharos_server::cast_sessions::tv_session_id;
use pharos_server::config::CastConfig;
use pharos_server::middleware::LowercasePath;
use pharos_server::state::{AppState, SocketBroadcast, Stores};
use pharos_sync::group::{GroupHandle, GroupMsg};
use pharos_sync::messages::{MemberId, ServerMsg};
use pharos_sync::{GroupRegistry, LocalDelivery, MemberSinks, SessionHub};
use pharos_test_metrics::CumulativeSnapshotter;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot};

pub const TV: &str = "Living Room";
pub const ITEM: &str = "item-1";
pub const START_MS: u64 = 120_000;

pub struct Person {
    pub id: UserId,
    pub name: String,
    pub token: String,
    pub device: String,
    pub key: String,
    pub member: MemberId,
    pub rx: mpsc::Receiver<ServerMsg>,
}

pub struct CastRig {
    pub srv: TestServer,
    pub state: web::Data<AppState>,
    pub hub: SessionHub,
    pub group: GroupHandle,
    pub registry: GroupRegistry,
    pub sinks: MemberSinks,
    pub service: CastServiceHandle,
    pub receiver: FakeReceiverHandle,
    pub factory: FakeTransportFactory,
    pub directory: Arc<RuntimeCastDirectory>,
    pub bus: broadcast::Receiver<SocketBroadcast>,
    pub alice: Person,
    pub bob: Person,
    pub carol: Person,
    pub tv_id: String,
    _tv_listener: std::net::TcpListener,
}

/// One process-wide recorder: handlers and the cast actor run on other
/// threads than the test, so a thread-local recorder would never see them.
pub fn snapshotter() -> &'static CumulativeSnapshotter {
    static SNAP: OnceLock<CumulativeSnapshotter> = OnceLock::new();
    SNAP.get_or_init(|| {
        let recorder = DebuggingRecorder::new();
        let snap = pharos_test_metrics::snapshotter(&recorder);
        recorder
            .install()
            .expect("no other recorder in this binary");
        snap
    })
}

/// Current value of `name`, summed over the series whose labels include all of `labels`.
pub fn counter(name: &str, labels: &[(&str, &str)]) -> u64 {
    snapshotter()
        .snapshot()
        .into_vec()
        .into_iter()
        .filter(|(ck, _, _, _)| ck.key().name() == name)
        .filter(|(ck, _, _, _)| {
            labels
                .iter()
                .all(|(k, v)| ck.key().labels().any(|l| l.key() == *k && l.value() == *v))
        })
        .map(|(_, _, _, v)| match v {
            DebugValue::Counter(c) => c,
            _ => 0,
        })
        .sum()
}

async fn new_user(stores: &Stores, name: &str) -> (UserId, String) {
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
    let token = stores.issue(id, "test").await.unwrap();
    (id, token.0.expose().to_string())
}

/// Register `name`'s player with the hub; `group` joins them to it.
async fn person(
    stores: &Stores,
    hub: &SessionHub,
    sinks: &MemberSinks,
    group: Option<&GroupHandle>,
    name: &str,
) -> Person {
    let (id, token) = new_user(stores, name).await;
    let device = format!("dev-{name}");
    let key = format!("{}:{device}", id.0.simple());
    let (tx, rx) = mpsc::channel(256);
    let reg = hub.register(key.clone(), name.into(), tx.clone());
    sinks.insert(reg.member_id, reg.gen, tx);
    if let Some(group) = group {
        hub.attach_group(&key, group.clone());
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
    }
    Person {
        id,
        name: name.into(),
        token,
        device,
        key,
        member: reg.member_id,
        rx,
    }
}

impl CastRig {
    pub async fn new() -> Self {
        Self::with_cast_settings("").await
    }

    /// `settings` are extra top-level `[cast]` keys, as TOML lines.
    pub async fn with_cast_settings(settings: &str) -> Self {
        Self::build(settings, None).await
    }

    /// A rig whose group queue holds a stored item of `kind`; returns its wire id too.
    pub async fn with_stored_item(kind: pharos_core::MediaKind) -> (Self, String) {
        let id = 7;
        let wire = pharos_jellyfin_api::dto::wire_item_id(id);
        (Self::build("", Some((id, kind))).await, wire)
    }

    async fn build(settings: &str, stored: Option<(u64, pharos_core::MediaKind)>) -> Self {
        snapshotter();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let cfg: CastConfig = toml::from_str(&format!(
            r#"
            server_url = "http://pharos.lan:8096"
            reconnect_grace_secs = 1
            {settings}
            [[tvs]]
            name = "{TV}"
            address = "127.0.0.1:{port}"
            "#
        ))
        .unwrap();

        let stores = Stores::connect("sqlite::memory:").await.unwrap();
        let queue_item = match stored {
            Some((id, kind)) => {
                use pharos_core::MediaStore;
                stores
                    .put(pharos_core::MediaItem {
                        id,
                        path: format!("/media/{id}").into(),
                        kind,
                        ..Default::default()
                    })
                    .await
                    .unwrap();
                pharos_jellyfin_api::dto::wire_item_id(id)
            }
            None => ITEM.to_string(),
        };
        let sinks = MemberSinks::new();
        let hub = SessionHub::new();
        let registry = GroupRegistry::spawn(Arc::new(LocalDelivery::new(sinks.clone())));
        let group = registry.create().await.unwrap();

        let alice = person(&stores, &hub, &sinks, Some(&group), "alice").await;
        group
            .tx
            .send(GroupMsg::SeedQueue {
                item_id: queue_item,
                runtime_ms: Some(3_600_000),
                position_ms: START_MS,
                is_paused: false,
            })
            .await
            .unwrap();
        let bob = person(&stores, &hub, &sinks, Some(&group), "bob").await;
        let carol = person(&stores, &hub, &sinks, None, "carol").await;

        let factory = FakeTransportFactory::new();
        let receiver = factory.add_tv(TV);
        let runtime = CastRuntime::start(&cfg.tvs);
        let service = cast_service::spawn(
            CastDeps {
                hub: hub.clone(),
                registry: registry.clone(),
                sinks: sinks.clone(),
                stores: stores.clone(),
            },
            cfg.clone(),
            factory.clone(),
        );
        runtime.install(service.clone());

        let directory = RuntimeCastDirectory::new(cfg, runtime.clone(), stores.clone());
        directory.bind_registry(registry.clone());
        let mut state = AppState::new(stores, "t".into()).with_cast_runtime(runtime);
        state.cast_directory = Some(directory.clone());
        directory.spawn_refresher(state.bus.clone());
        let bus = state.bus.subscribe();
        let state = web::Data::new(state);

        let (s, r, h, k) = (
            state.clone(),
            web::Data::new(registry.clone()),
            web::Data::new(hub.clone()),
            web::Data::new(sinks.clone()),
        );
        let sinks_for_rig = sinks;
        let srv = actix_test::start(move || {
            App::new()
                .app_data(s.clone())
                .app_data(r.clone())
                .app_data(h.clone())
                .app_data(k.clone())
                .wrap(LowercasePath)
                .configure(pharos_server::api::jellyfin::configure)
        });

        let rig = Self {
            srv,
            state,
            hub,
            group,
            registry,
            sinks: sinks_for_rig,
            service,
            receiver,
            factory,
            directory,
            bus,
            alice,
            bob,
            carol,
            tv_id: tv_session_id(TV),
            _tv_listener: listener,
        };
        rig.wait_until_listed().await;
        rig
    }

    /// The reachability prober's first probe is asynchronous.
    async fn wait_until_listed(&self) {
        use pharos_server::cast_sessions::CastDirectory;
        for _ in 0..250 {
            if self.directory.reachable().get(TV).copied() == Some(true) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the reachability prober never reported {TV} as up");
    }

    pub async fn post(
        &self,
        who: &Person,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> (u16, String) {
        let req = awc::Client::default()
            .post(self.srv.url(path))
            .insert_header(("X-Emby-Token", who.token.as_str()))
            .insert_header(("X-Emby-Device-Id", who.device.as_str()));
        let mut resp = match body {
            Some(b) => req.send_json(&b).await.unwrap(),
            None => req.send().await.unwrap(),
        };
        let text = String::from_utf8(resp.body().await.unwrap().to_vec()).unwrap();
        (resp.status().as_u16(), text)
    }

    pub async fn sessions(&self, who: &Person) -> Vec<serde_json::Value> {
        let mut resp = awc::Client::default()
            .get(self.srv.url("/Sessions"))
            .insert_header(("X-Emby-Token", who.token.as_str()))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        resp.json().await.unwrap()
    }

    pub async fn tv_session(&self, who: &Person) -> serde_json::Value {
        self.sessions(who)
            .await
            .into_iter()
            .find(|s| s["Id"] == self.tv_id.as_str())
            .expect("the TV session is listed")
    }

    pub async fn play_on_tv(&self, who: &Person) -> (u16, String) {
        self.post(
            who,
            &format!("/Sessions/{}/Playing", self.tv_id),
            Some(serde_json::json!({"ItemIds": [ITEM], "PlayCommand": "PlayNow"})),
        )
        .await
    }

    /// The token the TV was handed in its first command.
    pub fn tv_token(&self) -> String {
        self.receiver
            .commands()
            .first()
            .map(|c| c.access_token.expose().to_owned())
            .expect("the TV was sent a command carrying its token")
    }

    /// HTTP status of an authenticated request made with `token` as the TV does.
    pub async fn token_status(&self, token: &str) -> u16 {
        let resp = awc::Client::default()
            .get(self.srv.url("/Users/Me"))
            .insert_header((
                "Authorization",
                format!(
                    "MediaBrowser Client=\"Chromecast\", Device=\"Chromecast\", \
                     DeviceId=\"dGlr\", Version=\"3.0\", Token=\"{token}\""
                ),
            ))
            .send()
            .await
            .unwrap();
        resp.status().as_u16()
    }

    pub async fn group_play_state(&self) -> pharos_sync::messages::GroupPlayState {
        let (tx, rx) = oneshot::channel();
        self.group
            .tx
            .send(GroupMsg::Snapshot { reply: tx })
            .await
            .unwrap();
        rx.await.unwrap().play_state
    }

    pub async fn wait_cast_state(&self, want: pharos_cast::obs::ConnState) {
        for _ in 0..250 {
            if self.service.snapshot().await.first().map(|c| c.state) == Some(want) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the cast never reached {want:?}");
    }

    /// A Seek by Bob opens its gate as soon as every Player in `players` has
    /// acked: nothing waits on the TV, whether its seat is lost or gone.
    pub async fn assert_players_not_frozen(&self, players: &[MemberId]) {
        self.group
            .tx
            .send(GroupMsg::SeekTo {
                sender: self.bob.member,
                position_ms: 300_000,
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(600)).await;
        for member_id in players {
            self.group
                .tx
                .send(GroupMsg::MemberReady {
                    member_id: *member_id,
                    position_ms: 300_000,
                    playlist_item_id: None,
                    is_playing: false,
                })
                .await
                .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_ne!(
            self.group_play_state().await,
            pharos_sync::messages::GroupPlayState::Waiting,
            "a gate is still held after the only Player acked"
        );
    }

    /// Wait until the cast service holds no seat.
    pub async fn wait_cast_gone(&self) {
        for _ in 0..400 {
            if self.service.snapshot().await.is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("the cast never ended");
    }

    /// Drain the bus and return everything sent since the last call.
    pub fn bus_events(&mut self) -> Vec<SocketBroadcast> {
        let mut out = Vec::new();
        while let Ok(b) = self.bus.try_recv() {
            out.push(b);
        }
        out
    }
}

pub async fn eventually<F: FnMut() -> bool>(what: &str, mut f: F) {
    for _ in 0..250 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for: {what}");
}

/// Next message on `rx` satisfying `pred`, skipping the rest; panics after 5 s.
pub async fn wait_msg(
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

pub fn drain(rx: &mut mpsc::Receiver<ServerMsg>) -> Vec<ServerMsg> {
    let mut out = Vec::new();
    while let Ok(m) = rx.try_recv() {
        out.push(m);
    }
    out
}
