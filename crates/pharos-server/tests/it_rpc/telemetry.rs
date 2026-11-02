//! Spec 017 T069: `Telemetry.submit` logs a client's events at their level,
//! counts them, bounds each batch and each session's rate, and keeps
//! credentials out of the log. Driven over an in-memory pipe on the test
//! thread, so the thread-local recorder and subscriber see the handler.
//! Disarm checks are noted on each test.
use std::sync::{Arc, Mutex};

use capnp_rpc::{rpc_twoparty_capnp::Side, twoparty, RpcSystem};
use metrics_util::debugging::{DebugValue, DebuggingRecorder};
use pharos_api::pharos_capnp::{bootstrap, done, session, telemetry, EventKind, Level};
use pharos_api::Connection;
use pharos_server::api::pharos::rpc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::bootstrap::LOCAL;
use crate::common::{seed_rich, Fixture};

/// Every log line the test thread emits, as formatted text.
#[derive(Clone, Default)]
pub(crate) struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl std::io::Write for Captured {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Captured {
        self.clone()
    }
}

pub(crate) fn subscriber(logs: &Captured) -> impl tracing::Subscriber + Send + Sync {
    tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .finish()
}

/// Serves the bootstrap on this thread. Call inside a `LocalSet`.
pub(crate) fn connect(fx: &Fixture) -> bootstrap::Client {
    let (server_end, client_end) = tokio::io::duplex(64 * 1024);
    let (sr, sw) = tokio::io::split(server_end);
    let ctx = rpc::ConnectionCtx::admit(None, LOCAL, &rpc::limits::ConnectionLimits::default(), 16)
        .unwrap();
    let boot = rpc::bootstrap_client(fx.state.clone(), ctx, &Connection::new());
    tokio::task::spawn_local(rpc::serve(sr, sw, boot));
    let (cr, cw) = tokio::io::split(client_end);
    let network = twoparty::VatNetwork::new(
        cr.compat(),
        cw.compat_write(),
        Side::Client,
        Default::default(),
    );
    let mut system = RpcSystem::new(Box::new(network), None);
    let client: bootstrap::Client = system.bootstrap(Side::Server);
    tokio::task::spawn_local(system);
    client
}

pub(crate) async fn session_for(client: &bootstrap::Client, token: &str) -> session::Client {
    let mut req = client.resume_request();
    req.get().init_ctx();
    req.get().set_device_token(token);
    req.send().pipeline.get_result().get_session()
}

async fn telemetry_of(session: &session::Client) -> telemetry::Client {
    session.telemetry_request().send().pipeline.get_telemetry()
}

/// One event to submit: its kind, level, route, play session and fields.
#[derive(Clone)]
struct Ev {
    kind: EventKind,
    level: Level,
    route: &'static str,
    play_session: Option<&'static str>,
    fields: Vec<(&'static str, &'static str)>,
}

fn ev(kind: EventKind, level: Level) -> Ev {
    Ev {
        kind,
        level,
        route: "/native/player",
        play_session: None,
        fields: Vec::new(),
    }
}

/// `Ok` when the batch was accepted; `Err` with the refusal's reason.
async fn submit(tel: &telemetry::Client, events: &[Ev]) -> Result<(), String> {
    let mut req = tel.submit_request();
    req.get().init_ctx();
    let mut list = req.get().init_events(u32::try_from(events.len()).unwrap());
    for (i, e) in events.iter().enumerate() {
        let mut out = list.reborrow().get(u32::try_from(i).unwrap());
        out.set_kind(e.kind);
        out.set_level(e.level);
        out.set_route(e.route);
        if let Some(ps) = e.play_session {
            out.reborrow().init_play_session().set_value(ps);
        }
        let mut fields = out.init_fields(u32::try_from(e.fields.len()).unwrap());
        for (j, (k, v)) in e.fields.iter().enumerate() {
            let mut f = fields.reborrow().get(u32::try_from(j).unwrap());
            f.set_key(k);
            f.set_value(v);
        }
    }
    req.get().init_spans(0);
    let reply = req.send().promise.await.expect("submit answers");
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

/// The value of `pharos_client_events_total` (or the dropped counter) for
/// the given labels, 0 when no such series was recorded.
pub(crate) fn counter(
    snapshotter: &pharos_test_metrics::CumulativeSnapshotter,
    name: &str,
    labels: &[(&str, &str)],
) -> u64 {
    snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .filter(|(k, ..)| {
            let key = k.key();
            key.name() == name
                && labels
                    .iter()
                    .all(|(lk, lv)| key.labels().any(|l| l.key() == *lk && l.value() == *lv))
        })
        .map(|(.., v)| match v {
            DebugValue::Counter(n) => n,
            other => panic!("{other:?}"),
        })
        .sum()
}

/// Opts the fixture's user in to sending diagnostics, as `Prefs.set` would.
async fn opt_in(fx: &Fixture) {
    use pharos_core::UserPrefStore;
    fx.state
        .stores
        .set_user_pref(fx.user_id, "diagnostics.opt_in", b"true")
        .await
        .unwrap();
}

/// A warn event is logged with its kind and route, and counted under its
/// kind and level. Disarm check: without the `tracing` call or the counter
/// in `log_event`, either assertion fails.
#[actix_web::test]
async fn a_warn_event_is_logged_with_its_kind_and_route_and_counted() {
    let fx = seed_rich().await;
    opt_in(&fx).await;
    let logs = Captured::default();
    let _log = tracing::subscriber::set_default(subscriber(&logs));
    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);

    tokio::task::LocalSet::new()
        .run_until(async {
            let client = connect(&fx);
            let tel = telemetry_of(&session_for(&client, &fx.user_token).await).await;
            let warn = Ev {
                fields: vec![("reason", "decode")],
                ..ev(EventKind::PlaybackError, Level::Warn)
            };
            submit(&tel, &[warn]).await.expect("accepted");
        })
        .await;

    let text = logs.text();
    assert!(
        text.contains("WARN") && text.contains("client event"),
        "{text}"
    );
    assert!(text.contains("kind=\"playbackError\""), "{text}");
    assert!(text.contains("route=/native/player"), "{text}");
    assert_eq!(
        counter(
            &snapshotter,
            "pharos_client_events_total",
            &[("kind", "playbackError"), ("level", "warn")]
        ),
        1
    );
}

/// A credential-looking key or value never reaches the log, nor a play
/// session's full id; the id keeps its 8-character prefix. Disarm check:
/// with the `secret` decision in `render_fields` forced to `false`, the
/// Bearer value and the token-shaped value appear in the log and this fails.
#[actix_web::test]
async fn a_credential_looking_field_is_not_logged() {
    let fx = seed_rich().await;
    opt_in(&fx).await;
    let logs = Captured::default();
    let _log = tracing::subscriber::set_default(subscriber(&logs));
    let recorder = DebuggingRecorder::new();
    let _metrics = metrics::set_default_local_recorder(&recorder);
    let token_shaped = "aB3dE5fG7hJ9kL1mN3pQ5rS7tU9vW1xY3zA5";
    let play_session = "ps-7f3a9c2e-deadbeef-0123456789";

    tokio::task::LocalSet::new()
        .run_until(async {
            let client = connect(&fx);
            let tel = telemetry_of(&session_for(&client, &fx.user_token).await).await;
            let event = Ev {
                play_session: Some(play_session),
                fields: vec![
                    ("authorization", "Bearer abc123plain"),
                    ("accessToken", "plain-looking"),
                    ("detail", token_shaped),
                    ("reason", "decode"),
                ],
                ..ev(EventKind::PlaybackError, Level::Error)
            };
            submit(&tel, &[event]).await.expect("accepted");
        })
        .await;

    let text = logs.text();
    assert!(text.contains("reason=\"decode\""), "{text}");
    assert!(text.contains("<redacted>"), "{text}");
    for secret in [
        "Bearer abc123plain",
        "plain-looking",
        token_shaped,
        play_session,
    ] {
        assert!(!text.contains(secret), "{secret} reached the log:\n{text}");
    }
    assert!(text.contains("play_session=ps-7f3a9 "), "{text}");
}

/// A batch over the event bound is refused with a `Limit` and counted as
/// dropped, and the refusal spends no rate token: a small batch after it is
/// still accepted. Disarm check: without the size check, the 51 events are
/// accepted and the dropped counter reads 0.
#[actix_web::test]
async fn a_batch_over_the_bound_is_dropped_and_counted() {
    let fx = seed_rich().await;
    opt_in(&fx).await;
    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);

    let refused = tokio::task::LocalSet::new()
        .run_until(async {
            let client = connect(&fx);
            let tel = telemetry_of(&session_for(&client, &fx.user_token).await).await;
            let big = vec![ev(EventKind::Seek, Level::Info); 51];
            let refused = submit(&tel, &big).await;
            submit(&tel, &[ev(EventKind::Seek, Level::Info)])
                .await
                .expect("a small batch after a refusal is accepted");
            refused
        })
        .await;

    assert!(refused.is_err(), "a 51-event batch must be refused");
    assert_eq!(
        counter(
            &snapshotter,
            "pharos_client_events_dropped_total",
            &[("reason", "batch_too_large")]
        ),
        51
    );
    assert_eq!(
        counter(
            &snapshotter,
            "pharos_client_events_total",
            &[("kind", "seek"), ("level", "info")]
        ),
        1,
        "only the accepted event is counted"
    );
}

/// Batches beyond the session's burst are refused and counted as
/// `rate_limited`. Disarm check: without the bucket in `submit`, all 21
/// batches are accepted and the rate-limited counter reads 0.
#[actix_web::test]
async fn the_rate_limit_drops_and_counts_the_batches_beyond_the_burst() {
    let fx = seed_rich().await;
    opt_in(&fx).await;
    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);

    let outcomes = tokio::task::LocalSet::new()
        .run_until(async {
            let client = connect(&fx);
            let tel = telemetry_of(&session_for(&client, &fx.user_token).await).await;
            let started = std::time::Instant::now();
            let mut outcomes = Vec::new();
            for _ in 0..40 {
                outcomes.push(submit(&tel, &[ev(EventKind::Seek, Level::Info)]).await);
            }
            (outcomes, started.elapsed())
        })
        .await;
    let (outcomes, elapsed) = outcomes;

    // The bucket refills one batch a second, so a slow runner may admit a few
    // past the burst; it must still refuse well before 40.
    let accepted = outcomes.iter().filter(|o| o.is_ok()).count();
    let refill = usize::try_from(elapsed.as_secs()).unwrap_or(usize::MAX) + 1;
    assert!(
        accepted >= 20,
        "the burst is 20 batches, accepted {accepted}"
    );
    assert!(
        accepted <= 20 + refill,
        "accepted {accepted} in {elapsed:?}: the rate limit did not hold"
    );
    let refused = outcomes.len() - accepted;
    assert_eq!(
        counter(
            &snapshotter,
            "pharos_client_events_dropped_total",
            &[("reason", "rate_limited")]
        ),
        u64::try_from(refused).unwrap()
    );
}

/// A session that never signed in (a refused resume) gets the cap, but every
/// `submit` on it is refused and nothing is logged or counted. Disarm check:
/// without the `current_user` check in `submit`, the event is accepted.
#[actix_web::test]
async fn an_unauthenticated_session_cannot_submit() {
    let fx = seed_rich().await;
    let logs = Captured::default();
    let _log = tracing::subscriber::set_default(subscriber(&logs));
    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);

    let refused = tokio::task::LocalSet::new()
        .run_until(async {
            let client = connect(&fx);
            let tel = telemetry_of(&session_for(&client, "not-a-live-token").await).await;
            submit(&tel, &[ev(EventKind::Panic, Level::Error)]).await
        })
        .await;

    assert!(
        refused.is_err(),
        "an unauthenticated submit must be refused"
    );
    assert!(!logs.text().contains("client event"), "{}", logs.text());
    assert_eq!(
        counter(
            &snapshotter,
            "pharos_client_events_total",
            &[("kind", "panic")]
        ),
        0
    );
}

/// A user who has not opted in has the batch refused, nothing logged or
/// counted as an event, and the events counted as dropped `not_opted_in`;
/// the decision is logged with the user id and no event content. Disarm
/// checks: without `count_dropped(NotOptedIn, ..)` the counter reads 0;
/// with the `Ok(false)` arm treated as opted in, the event is logged.
#[actix_web::test]
async fn a_user_who_has_not_opted_in_is_dropped_and_counted() {
    let fx = seed_rich().await;
    let logs = Captured::default();
    let _log = tracing::subscriber::set_default(subscriber(&logs));
    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);

    let refused = tokio::task::LocalSet::new()
        .run_until(async {
            let client = connect(&fx);
            let tel = telemetry_of(&session_for(&client, &fx.user_token).await).await;
            let event = Ev {
                fields: vec![("reason", "private-detail")],
                ..ev(EventKind::PlaybackError, Level::Error)
            };
            submit(&tel, &[event.clone(), event]).await
        })
        .await;

    assert_eq!(
        refused,
        Err("diagnostics not enabled for this user".to_owned())
    );
    let text = logs.text();
    assert!(!text.contains("client event"), "{text}");
    assert!(!text.contains("private-detail"), "{text}");
    assert!(
        text.contains("client telemetry batch dropped")
            && text.contains("reason=\"not_opted_in\"")
            && text.contains(&fx.user_id.0.to_string()),
        "{text}"
    );
    assert_eq!(
        counter(
            &snapshotter,
            "pharos_client_events_dropped_total",
            &[("reason", "not_opted_in")]
        ),
        2
    );
    assert_eq!(
        counter(&snapshotter, "pharos_client_events_total", &[]),
        0,
        "a dropped event is not counted as received"
    );
}

/// Opting in and out through `Prefs` changes what the same session's next
/// batch does, at once: the cached opt-in is dropped on the write. Disarm
/// check: without `forget_opt_in` in `Prefs.set`, the batch after opting out
/// is still accepted.
#[actix_web::test]
async fn opting_in_and_out_through_prefs_takes_effect_on_the_next_batch() {
    let fx = seed_rich().await;
    tokio::task::LocalSet::new()
        .run_until(async {
            let client = connect(&fx);
            let session = session_for(&client, &fx.user_token).await;
            let tel = telemetry_of(&session).await;
            let prefs = session.prefs_request().send().pipeline.get_prefs();
            let seek = [ev(EventKind::Seek, Level::Info)];
            let set = |value: &'static [u8]| {
                let prefs = prefs.clone();
                async move {
                    let mut req = prefs.set_request();
                    req.get().init_ctx();
                    req.get().set_key("diagnostics.opt_in");
                    req.get().set_value(value);
                    req.send().promise.await.expect("set answers");
                }
            };
            submit(&tel, &seek).await.expect_err("off by default");
            set(b"true").await;
            submit(&tel, &seek).await.expect("accepted once opted in");
            set(b"false").await;
            submit(&tel, &seek)
                .await
                .expect_err("refused once opted out");
        })
        .await;
}
