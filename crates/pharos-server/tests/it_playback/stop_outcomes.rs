//! Spec 038 FR-107 / FR-108: every stop is counted in
//! `pharos_play_stop_total{outcome,reason}`, the failures as well as the
//! saves, so a stop that stored nothing can be found by query.

use actix_web::{test, web, App};
use metrics_util::debugging::{DebugValue, DebuggingRecorder};
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore, UserDataStore, UserId,
    UserPolicy, UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    services::play_state::StopOutcome,
    state::{AppState, Stores},
};

const ITEM: u64 = 7;
/// One hour, in ticks.
const RUNTIME_TICKS: u64 = 36_000_000_000;

async fn seed() -> (web::Data<AppState>, String, UserId) {
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
    let token = stores.issue(uid, "t").await.unwrap();
    stores
        .put(MediaItem {
            id: ITEM,
            path: "/fake/film.mkv".into(),
            title: "A Film".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(3_600_000),
                ..Default::default()
            },
            ..MediaItem::default()
        })
        .await
        .unwrap();
    let state = web::Data::new(AppState::new(stores, "t".into()));
    (state, token.0.expose().to_string(), uid)
}

fn stops(snap: &pharos_test_metrics::CumulativeSnapshotter, outcome: StopOutcome) -> u64 {
    let (want_outcome, want_reason) = outcome.label();
    snap.snapshot()
        .into_vec()
        .into_iter()
        .filter(|(ck, ..)| {
            let key = ck.key();
            key.name() == "pharos_play_stop_total"
                && key
                    .labels()
                    .any(|l| l.key() == "outcome" && l.value() == want_outcome)
                && key
                    .labels()
                    .any(|l| l.key() == "reason" && l.value() == want_reason)
        })
        .map(|(.., v)| match v {
            DebugValue::Counter(n) => n,
            _ => 0,
        })
        .sum()
}

fn item_wire() -> String {
    pharos_jellyfin_api::dto::WireId::Real(ITEM).to_string()
}

macro_rules! stop {
    ($app:expr, $token:expr, $body:expr) => {{
        let req = test::TestRequest::post()
            .uri("/Sessions/Playing/Stopped")
            .insert_header(("X-Emby-Token", $token))
            .insert_header(("content-type", "application/json"))
            .set_payload($body)
            .to_request();
        match test::try_call_service(&$app, req).await {
            Ok(resp) => resp.status().as_u16(),
            Err(e) => e.as_response_error().status_code().as_u16(),
        }
    }};
}

#[::core::prelude::v1::test]
fn stop_outcome_labels_are_distinct_and_stable() {
    let labels: Vec<_> = StopOutcome::ALL.iter().map(|o| o.label()).collect();
    assert_eq!(
        labels,
        [
            ("saved", "position"),
            ("finished", "past_threshold"),
            ("skipped", "no_item"),
            ("store_error", "item_read"),
            ("store_error", "user_data_read"),
            ("store_error", "user_data_write"),
        ]
    );
}

/// Disarm check: with `StopOutcome::record` a no-op, every count below
/// reads 0 and the test fails.
#[actix_web::test]
async fn each_stop_is_counted_by_what_it_stored() {
    let (state, token, uid) = seed().await;
    let app = test::init_service(
        App::new()
            .app_data(state.clone())
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    let recorder = DebuggingRecorder::new();
    let snap = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);
    let id = item_wire();

    // Five seconds into an hour-long film.
    let five_s = format!(r#"{{"ItemId":"{id}","PositionTicks":50000000}}"#);
    assert_eq!(stop!(app, token.as_str(), five_s), 204);
    assert_eq!(stops(&snap, StopOutcome::Saved), 1);
    let stored = state.stores.get_user_data(uid, ITEM).await.unwrap();
    assert_eq!(stored.last_played_position_ticks, 50_000_000);

    let near_end = format!(r#"{{"ItemId":"{id}","PositionTicks":{RUNTIME_TICKS}}}"#);
    assert_eq!(stop!(app, token.as_str(), near_end), 204);
    assert_eq!(stops(&snap, StopOutcome::Finished), 1);

    assert_eq!(stop!(app, token.as_str(), r#"{"PositionTicks":1}"#), 204);
    assert_eq!(stops(&snap, StopOutcome::NoItem), 1);
}

fn sqlite_of(stores: &Stores) -> &pharos_store_sqlx::sqlite::SqliteStore {
    #[cfg(feature = "postgres")]
    let sqlite = match stores {
        pharos_store_sqlx::any::AnyStore::Sqlite(s) => s,
        pharos_store_sqlx::any::AnyStore::Postgres(_) => panic!("the fixture is sqlite"),
    };
    #[cfg(not(feature = "postgres"))]
    let sqlite = stores;
    sqlite
}

/// A stop whose play-state read fails is counted as a store error and
/// answered 503, so the client can send it again (FR-108).
#[actix_web::test]
async fn a_stop_whose_user_data_read_fails_is_counted() {
    let (state, token, _uid) = seed().await;
    let app = test::init_service(
        App::new()
            .app_data(state.clone())
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    let recorder = DebuggingRecorder::new();
    let snap = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);
    sqlx::query("DROP TABLE user_data")
        .execute(sqlite_of(&state.stores).pool())
        .await
        .unwrap();

    let body = format!(r#"{{"ItemId":"{}","PositionTicks":50000000}}"#, item_wire());
    assert_eq!(stop!(app, token.as_str(), body), 503);
    assert_eq!(stops(&snap, StopOutcome::UserDataReadFailed), 1);
    assert_eq!(stops(&snap, StopOutcome::Saved), 0);
}

/// A stop whose play-state write fails is counted and answered 503 (FR-108).
#[actix_web::test]
async fn a_stop_whose_user_data_write_fails_is_counted() {
    let (state, token, _uid) = seed().await;
    let app = test::init_service(
        App::new()
            .app_data(state.clone())
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    let recorder = DebuggingRecorder::new();
    let snap = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);
    sqlx::query(
        "CREATE TRIGGER refuse_user_data BEFORE INSERT ON user_data
         BEGIN SELECT RAISE(FAIL, 'refused by the test'); END",
    )
    .execute(sqlite_of(&state.stores).pool())
    .await
    .unwrap();

    let body = format!(r#"{{"ItemId":"{}","PositionTicks":50000000}}"#, item_wire());
    assert_eq!(stop!(app, token.as_str(), body), 503);
    assert_eq!(stops(&snap, StopOutcome::UserDataWriteFailed), 1);
    assert_eq!(stops(&snap, StopOutcome::Saved), 0);
}
