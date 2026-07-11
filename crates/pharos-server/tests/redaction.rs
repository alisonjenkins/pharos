#![allow(clippy::unwrap_used, clippy::expect_used)]
//! T44 — V8 ("auth tokens never logged") integration test.
//!
//! Drives a full Jellyfin auth flow (AuthenticateByName -> /Users/Me)
//! against an in-process pharos, capturing every `tracing` event the
//! handlers + extractors + sqlx layer emit. Asserts the issued bearer
//! token never appears in any captured event byte. Catches future
//! regressions where someone fmt-prints a SecretString via `.expose()`
//! into a tracing field.

use actix_test::TestServer;
use actix_web::{web, App};
use pharos_core::{SecretString, UserId, UserPolicy, UserRecord, UserStore};
use pharos_jellyfin_test_client::{DeviceInfo, JellyfinClient};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};
use pharos_sync::GroupRegistry;
use std::sync::{Arc, Mutex};
use tracing_subscriber::layer::SubscriberExt;

/// `MakeWriter` impl that funnels every formatted record into a shared
/// byte buffer. Lets the assertion at the end of the test grep the
/// complete captured stream.
#[derive(Clone)]
struct VecWriter(Arc<Mutex<Vec<u8>>>);

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for VecWriter {
    type Writer = SharedBuf;
    fn make_writer(&'a self) -> Self::Writer {
        SharedBuf(self.0.clone())
    }
}

struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for SharedBuf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

async fn boot() -> (TestServer, String) {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("hunter2")).unwrap();
    stores
        .create(UserRecord {
            id: UserId::new(),
            name: "ali".into(),
            password_hash: hash,
            policy: UserPolicy { admin: true },
        })
        .await
        .unwrap();
    let state = web::Data::new(AppState::new(stores, "pharos-redact".into()));
    let member_sinks = pharos_sync::MemberSinks::new();
    let registry = web::Data::new(GroupRegistry::spawn(std::sync::Arc::new(
        pharos_sync::LocalDelivery::new(member_sinks.clone()),
    )));
    let member_sinks = web::Data::new(member_sinks);
    let server = actix_test::start(move || {
        App::new()
            .app_data(state.clone())
            .app_data(registry.clone())
            .app_data(web::Data::new(pharos_sync::SessionHub::new()))
            .app_data(member_sinks.clone())
            .wrap(LowercasePath)
            .configure(jellyfin::configure)
    });
    let url = server.url("").trim_end_matches('/').to_string();
    (server, url)
}

#[test]
fn auth_flow_never_logs_token() {
    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let writer = VecWriter(buf.clone());

    // Subscriber routes every event/span field through `writer` at
    // DEBUG so even diagnostic spans the handlers may emit are
    // captured. set_global_default panics if another subscriber is
    // already installed in the test process — we live with that for a
    // single dedicated redaction test binary.
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_writer(writer)
            .with_target(true)
            .with_level(true),
    );
    let _guard = tracing::subscriber::set_default(subscriber);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let issued_token = rt.block_on(async {
        let (_server, base) = boot().await;
        let mut client = JellyfinClient::new(base, DeviceInfo::default());
        let auth = client
            .authenticate_by_name("ali", "hunter2")
            .await
            .expect("authenticate");
        let token = auth.access_token.clone();
        let _ = client.users_me().await.unwrap();
        let _ = client.items().await.unwrap();
        let _ = client.sessions().await.unwrap();
        token
    });

    drop(_guard);

    let captured = String::from_utf8_lossy(&buf.lock().unwrap().clone()).into_owned();

    // The exact issued token must not appear in any captured event.
    assert!(
        !captured.contains(&issued_token),
        "token bytes leaked into tracing stream:\n{captured}",
    );

    // Defence-in-depth: regex-scan for any 32-char lowercase hex chunk
    // outside known-public id contexts (user.id, server_id, etc.).
    let re = regex_lite::Regex::new(r"\b[0-9a-f]{32}\b").unwrap();
    for hit in re.find_iter(&captured) {
        let lo = hit.start().saturating_sub(40);
        let hi = (hit.end() + 40).min(captured.len());
        let context = &captured[lo..hi];
        if context.contains("user.id")
            || context.contains("user_id")
            || context.contains("server_id")
            || context.contains("session.id")
            || context.contains("session_id")
            || context.contains("UserId")
            || context.contains("SessionInfo")
            || context.contains("media.id")
            || context.contains("device")
        {
            continue;
        }
        panic!("32-char hex matched a non-id context — possible token leak:\n  …{context}…",);
    }
}
