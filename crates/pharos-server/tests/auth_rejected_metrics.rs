#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Every refused request is counted by reason in `pharos_auth_rejected_total`,
//! and the refusal costs no more database work than it must: nothing for a
//! request with no token, one token lookup for a token the store does not know.
//! Stale-token clients retry the progress route every ~10 s, so the 401 path is
//! as hot as the 204 one.

use actix_web::{test, web, App};
use metrics_util::debugging::{DebugValue, DebuggingRecorder};
use pharos_server::{
    api::jellyfin,
    middleware::LowercasePath,
    state::{AppState, Stores},
};
use tracing_subscriber::prelude::*;

#[path = "common/store_calls.rs"]
mod store_calls;
use store_calls::StoreCallLog;

fn rejected(snapshotter: &pharos_test_metrics::CumulativeSnapshotter, reason: &str) -> u64 {
    snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .find_map(|(key, _, _, value)| {
            let labelled = key
                .key()
                .labels()
                .any(|l| l.key() == "reason" && l.value() == reason);
            match value {
                DebugValue::Counter(n)
                    if key.key().name() == "pharos_auth_rejected_total" && labelled =>
                {
                    Some(n)
                }
                _ => None,
            }
        })
        .unwrap_or(0)
}

macro_rules! progress_with {
    ($app:expr, $token:expr) => {{
        let mut req = test::TestRequest::post()
            .uri("/Sessions/Playing/Progress")
            .insert_header(("content-type", "application/json"))
            .set_payload(r#"{"ItemId":"1","PlaySessionId":"s","PositionTicks":1}"#);
        if let Some(t) = $token {
            req = req.insert_header(("X-Emby-Token", t));
        }
        match test::try_call_service(&$app, req.to_request()).await {
            Ok(resp) => resp.status().as_u16(),
            Err(e) => e.as_response_error().status_code().as_u16(),
        }
    }};
}

#[actix_web::test]
async fn rejections_are_counted_by_reason_and_stay_cheap() {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let state = web::Data::new(AppState::new(stores, "srv".into()));
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;

    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);
    let calls = StoreCallLog::default();
    let _spans =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(calls.clone()));

    assert_eq!(progress_with!(app, None::<&str>), 401);
    assert_eq!(calls.take(), Vec::<String>::new(), "no token, no lookup");
    assert_eq!(progress_with!(app, Some("stale-token")), 401);
    assert_eq!(calls.take(), ["resolve"], "an unknown token is one lookup");
    assert_eq!(progress_with!(app, Some("stale-token")), 401);

    assert_eq!(rejected(&snapshotter, "missing_token"), 1);
    assert_eq!(rejected(&snapshotter, "invalid_token"), 2);
    assert_eq!(rejected(&snapshotter, "user_revoked"), 0);
}
