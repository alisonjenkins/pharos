#![allow(clippy::unwrap_used, clippy::expect_used)]
//! How many database calls one `POST /Sessions/Playing/Progress` costs.
//!
//! The route is the busiest one in production, so its store-call count is a
//! budget. Counted from the `#[tracing::instrument]` span every store method
//! opens (one span per round trip), so a regression shows up by name.

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore, UserId, UserPolicy,
    UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};
use std::sync::{Arc, Mutex};
use tracing_subscriber::{layer::Context, prelude::*, Layer};

#[derive(Clone, Default)]
struct StoreCallLog(Arc<Mutex<Vec<String>>>);

impl StoreCallLog {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

impl<S: tracing::Subscriber> Layer<S> for StoreCallLog {
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::span::Id,
        _ctx: Context<'_, S>,
    ) {
        let meta = attrs.metadata();
        if meta.target().starts_with("pharos_store_sqlx") {
            self.0.lock().unwrap().push(meta.name().to_string());
        }
    }
}

async fn seed() -> (web::Data<AppState>, String) {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("p")).unwrap();
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "u".into(),
            password_hash: hash,
            policy: UserPolicy::default(),
        })
        .await
        .unwrap();
    stores
        .put(MediaItem {
            id: 1,
            path: "/m/a.mkv".into(),
            title: "A".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(3_600_000),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
    let token = stores.issue(uid, "t").await.unwrap();
    let state = web::Data::new(AppState::new(stores, "srv".into()));
    (state, token.0.expose().to_string())
}

#[actix_web::test]
async fn a_progress_report_store_call_count() {
    let (state, token) = seed().await;
    let app = test::init_service(
        App::new()
            .app_data(state.clone())
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    let log = StoreCallLog::default();
    let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(log.clone()));

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/Sessions/Playing/Progress")
            .insert_header(("X-Emby-Token", token.as_str()))
            .insert_header(("content-type", "application/json"))
            .set_payload(r#"{"ItemId":"1","PlaySessionId":"s","PositionTicks":100000000}"#)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 204);
    let calls = log.take();
    for s in &calls {
        eprintln!("CALL: {s}");
    }
    eprintln!("STORE CALLS: {}", calls.len());
    assert_eq!(
        calls,
        [
            "resolve",
            "get",
            "get",
            "get_user_data",
            "set_user_data",
            "get"
        ],
        "one progress report: token, user, item (kind), user data read, write, item (runtime)"
    );
}
