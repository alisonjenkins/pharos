//! Spec 016 US5 / FR-008: `Telemetry.report` stores a pressed "Report a
//! problem" whatever the diagnostics opt-in, logs one line linking the report
//! id to the user and trace ids, and only the reporter or an admin reaches a
//! stored report. Disarm checks are noted on each test.
use metrics_util::debugging::DebuggingRecorder;
use pharos_api::pharos_capnp::{
    done, report_document_result, report_list_result, report_result, telemetry,
};
use pharos_server::client_reports::ReportStore;

use crate::common::{seed_rich, seed_rich_with, Fixture};
use crate::telemetry::{connect, counter, session_for, subscriber, Captured};

const TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";

async fn with_store() -> (Fixture, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = ReportStore::new(dir.path().join("reports"), 30);
    let fx = seed_rich_with(|state| state.with_client_reports(Some(store))).await;
    (fx, dir)
}

async fn telemetry_for(fx: &Fixture, token: &str) -> telemetry::Client {
    let client = connect(fx);
    session_for(&client, token)
        .await
        .telemetry_request()
        .send()
        .pipeline
        .get_telemetry()
}

fn bundle() -> Vec<u8> {
    serde_json::json!({
        "route": "/play/42",
        "traceIds": [TRACE],
        "breadcrumbs": ["clicked play", "GET /Videos/1/master.m3u8?api_key=leakme"],
    })
    .to_string()
    .into_bytes()
}

/// `Ok(id)` or `Err(reason)`.
async fn report(tel: &telemetry::Client, note: &str, bundle: &[u8]) -> Result<String, String> {
    let mut req = tel.report_request();
    req.get().init_ctx();
    req.get().set_note(note);
    req.get().set_bundle(bundle);
    let reply = req.send().promise.await.expect("report answers");
    match reply.get().unwrap().get_result().unwrap().which().unwrap() {
        report_result::ReportId(id) => Ok(id.unwrap().to_str().unwrap().to_owned()),
        report_result::Error(e) => Err(e
            .unwrap()
            .get_reason()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned()),
    }
}

async fn list(tel: &telemetry::Client, everyone: bool) -> Result<Vec<(String, String)>, String> {
    let mut req = tel.list_reports_request();
    req.get().init_ctx();
    req.get().set_everyone(everyone);
    let reply = req.send().promise.await.expect("list answers");
    match reply.get().unwrap().get_result().unwrap().which().unwrap() {
        report_list_result::Reports(rows) => Ok(rows
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r.get_id().unwrap().to_str().unwrap().to_owned(),
                    r.get_user_name().unwrap().to_str().unwrap().to_owned(),
                )
            })
            .collect()),
        report_list_result::Error(e) => Err(e
            .unwrap()
            .get_reason()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned()),
    }
}

async fn get(tel: &telemetry::Client, id: &str) -> Result<serde_json::Value, String> {
    let mut req = tel.get_report_request();
    req.get().init_ctx();
    req.get().set_id(id);
    let reply = req.send().promise.await.expect("get answers");
    match reply.get().unwrap().get_result().unwrap().which().unwrap() {
        report_document_result::Document(d) => Ok(serde_json::from_slice(d.unwrap()).unwrap()),
        report_document_result::Error(e) => Err(e
            .unwrap()
            .get_reason()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned()),
    }
}

async fn delete(tel: &telemetry::Client, id: &str) -> Result<(), String> {
    let mut req = tel.delete_report_request();
    req.get().init_ctx();
    req.get().set_id(id);
    let reply = req.send().promise.await.expect("delete answers");
    match reply.get().unwrap().get_result().unwrap().which().unwrap() {
        done::Ok(()) => Ok(()),
        done::Error(e) => Err(e
            .unwrap()
            .get_reason()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned()),
    }
}

/// A report from a user who has NOT opted in is stored, redacted, logged with
/// its id and trace ids, and counted. Disarm checks: gating `report` on the
/// opt-in, dropping the INFO line, or skipping `scrub` each fails this.
#[actix_web::test]
async fn a_pressed_report_is_stored_logged_and_counted_without_opt_in() {
    let (fx, _dir) = with_store().await;
    let logs = Captured::default();
    let _log = tracing::subscriber::set_default(subscriber(&logs));
    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);

    let (id, doc) = tokio::task::LocalSet::new()
        .run_until(async {
            let tel = telemetry_for(&fx, &fx.user_token).await;
            let id = report(&tel, "subtitles vanished", &bundle())
                .await
                .expect("stored");
            let doc = get(&tel, &id).await.expect("the reporter reads it");
            (id, doc)
        })
        .await;

    assert_eq!(id.len(), 9, "{id}");
    assert_eq!(doc["note"], "subtitles vanished");
    assert_eq!(doc["traceIds"][0], TRACE);
    assert!(!doc.to_string().contains("leakme"), "{doc}");
    let text = logs.text();
    assert!(text.contains("client problem report stored"), "{text}");
    assert!(text.contains(&format!("report_id={id}")), "{text}");
    assert!(text.contains(TRACE), "{text}");
    assert!(
        !text.contains("subtitles vanished"),
        "the note stays out of the log:\n{text}"
    );
    assert_eq!(
        counter(
            &snapshotter,
            "pharos_ui_reports_total",
            &[("outcome", "stored")]
        ),
        1
    );
}

/// With no report directory configured a report is refused as unavailable
/// and counted, never silently dropped. Disarm check: without the `NoStore`
/// branch the call panics on a missing store or reports success.
#[actix_web::test]
async fn without_a_store_a_report_is_refused_and_counted() {
    let fx = seed_rich().await;
    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);

    let refused = tokio::task::LocalSet::new()
        .run_until(async {
            let tel = telemetry_for(&fx, &fx.user_token).await;
            report(&tel, "", &bundle()).await
        })
        .await;

    assert!(refused.is_err());
    assert_eq!(
        counter(
            &snapshotter,
            "pharos_ui_reports_total",
            &[("outcome", "no_store")]
        ),
        1
    );
}

/// A bundle that is not a JSON object is refused and counted by why.
#[actix_web::test]
async fn a_bundle_that_is_not_an_object_is_refused() {
    let (fx, _dir) = with_store().await;
    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);

    let refused = tokio::task::LocalSet::new()
        .run_until(async {
            let tel = telemetry_for(&fx, &fx.user_token).await;
            report(&tel, "", b"[1,2,3]").await
        })
        .await;

    assert!(refused.is_err());
    assert_eq!(
        counter(
            &snapshotter,
            "pharos_ui_reports_total",
            &[("outcome", "invalid_bundle")]
        ),
        1
    );
}

/// The reporter lists and deletes their own report; they cannot list
/// everyone's, nor read or delete an admin's report, which reads as not
/// found. The admin lists everyone's and reads the user's. Disarm check:
/// without the owner check in `reachable`, the user reads the admin's report.
#[actix_web::test]
async fn only_the_reporter_or_an_admin_reaches_a_report() {
    let (fx, _dir) = with_store().await;

    tokio::task::LocalSet::new()
        .run_until(async {
            let user = telemetry_for(&fx, &fx.user_token).await;
            let admin = telemetry_for(&fx, &fx.admin_token).await;
            let mine = report(&user, "mine", &bundle()).await.unwrap();
            let theirs = report(&admin, "admin's", &bundle()).await.unwrap();

            let own = list(&user, false).await.unwrap();
            assert_eq!(
                own.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
                [mine.as_str()]
            );
            assert!(
                list(&user, true).await.is_err(),
                "a user can't list everyone's"
            );
            assert!(
                get(&user, &theirs).await.is_err(),
                "a user can't read an admin's"
            );
            assert!(
                delete(&user, &theirs).await.is_err(),
                "a user can't delete an admin's"
            );

            let all = list(&admin, true).await.unwrap();
            assert_eq!(all.len(), 2, "{all:?}");
            assert_eq!(get(&admin, &mine).await.unwrap()["note"], "mine");

            assert!(
                get(&user, "../../etc").await.is_err(),
                "a path is not an id"
            );
            delete(&user, &mine).await.unwrap();
            assert!(list(&user, false).await.unwrap().is_empty());
            assert!(
                get(&admin, &theirs).await.is_ok(),
                "the admin's report is untouched"
            );
        })
        .await;
}
