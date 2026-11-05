//! T025: native Quick Connect. `quickConnectInitiate` returns a code to show
//! and a secret to keep; once a signed-in user authorizes the code (on the
//! Jellyfin side, `/QuickConnect/Authorize`), `quickConnectPoll(secret)`
//! answers once with a session and device token, as `login` does. The poll
//! may come over a new connection. An unauthenticated connection closes at
//! its auth deadline, which a pending poll re-arms up to
//! `max_unauthenticated_secs` in all (B541).

use std::num::NonZeroU32;
use std::time::Duration;

use pharos_api::pharos_capnp::{
    bootstrap, done, error, login_result, me_result, quick_connect_result, session,
};
use pharos_api::Connection;
use pharos_core::{TokenStore, UserId, UserStore};
use pharos_server::api::pharos::rpc;
use pharos_server::config::NativeApiConfig;
use pharos_server::quick_connect::{QcMsg, MAX_AUTHORIZE_FAILURES};

use crate::bootstrap::LOCAL;
use crate::common::{seed_rich, Fixture};

fn client(fx: &Fixture) -> bootstrap::Client {
    let ctx = rpc::ConnectionCtx::admit(None, LOCAL, &rpc::limits::ConnectionLimits::default(), 16)
        .unwrap();
    rpc::bootstrap_client(fx.state.clone(), ctx, &Connection::new())
}

async fn initiate(client: &bootstrap::Client) -> (String, String) {
    let mut req = client.quick_connect_initiate_request();
    req.get().init_ctx();
    let reply = req.send().promise.await.unwrap();
    match reply.get().unwrap().get_result().unwrap().which().unwrap() {
        quick_connect_result::Pending(p) => {
            let p = p.unwrap();
            (
                p.get_code().unwrap().to_string().unwrap(),
                p.get_secret().unwrap().to_string().unwrap(),
            )
        }
        quick_connect_result::Error(e) => {
            panic!("initiate refused: {:?}", e.unwrap().get_reason())
        }
    }
}

async fn authorize(fx: &Fixture, code: &str, by: UserId) {
    let (reply, rx) = tokio::sync::oneshot::channel();
    fx.state
        .quick_connect
        .tx
        .send(QcMsg::Authorize {
            code: code.to_owned(),
            by,
            reply,
        })
        .await
        .unwrap();
    assert!(rx.await.unwrap().unwrap(), "authorize {code}");
}

/// How `quickConnectAuthorize` answered: ok, or the error's kind and reason.
#[derive(Debug, PartialEq)]
enum AuthorizeAnswer {
    Ok,
    NotFound,
    Revoked,
    Invalid { field: String },
    Throttled { name: String },
    Other(String),
}

/// Calls `quickConnectAuthorize` on `session` with `code` and sorts the answer.
async fn authorize_session(session: &session::Client, code: &str) -> AuthorizeAnswer {
    let mut req = session.quick_connect_authorize_request();
    req.get().init_ctx();
    req.get().set_code(code);
    let reply = req.send().promise.await.unwrap();
    match reply.get().unwrap().get_result().unwrap().which().unwrap() {
        done::Ok(()) => AuthorizeAnswer::Ok,
        done::Error(e) => {
            let e = e.unwrap();
            let reason = e.get_reason().unwrap().to_string().unwrap();
            match e.get_kind().which().unwrap() {
                error::kind::NotFound(_) => AuthorizeAnswer::NotFound,
                error::kind::Revoked(_) => AuthorizeAnswer::Revoked,
                error::kind::Invalid(i) => AuthorizeAnswer::Invalid {
                    field: i.unwrap().get_field().unwrap().to_string().unwrap(),
                },
                error::kind::Limit(l) => AuthorizeAnswer::Throttled {
                    name: l.unwrap().get_name().unwrap().to_string().unwrap(),
                },
                _ => AuthorizeAnswer::Other(reason),
            }
        }
    }
}

#[derive(Debug, PartialEq)]
enum Poll {
    Ok { name: String, token: String },
    NotPermitted(String),
    NotFound,
    Other(String),
}

async fn try_poll(client: &bootstrap::Client, secret: &str) -> Result<Poll, capnp::Error> {
    let mut req = client.quick_connect_poll_request();
    req.get().init_ctx();
    req.get().set_secret(secret);
    let reply = req.send().promise.await?;
    let res = match reply.get()?.get_result()?.which()? {
        login_result::Ok(ok) => {
            let ok = ok?;
            let mut me = ok.get_session()?.me_request();
            me.get().init_ctx();
            let me = me.send().promise.await?;
            let name = match me.get()?.get_result()?.which()? {
                me_result::Me(me) => me?.get_name()?.to_string().unwrap_or_default(),
                me_result::Error(_) => panic!("the quick connect session must serve"),
            };
            Poll::Ok {
                name,
                token: ok.get_device_token()?.to_string().unwrap_or_default(),
            }
        }
        login_result::Error(e) => {
            let e = e?;
            let reason = e.get_reason()?.to_string().unwrap_or_default();
            match e.get_kind().which()? {
                error::kind::NotPermitted(_) => Poll::NotPermitted(reason),
                error::kind::NotFound(_) => Poll::NotFound,
                _ => Poll::Other(reason),
            }
        }
    };
    Ok(res)
}

async fn poll(client: &bootstrap::Client, secret: &str) -> Poll {
    try_poll(client, secret).await.unwrap()
}

#[actix_web::test]
async fn an_authorized_code_signs_in_once_even_over_a_new_connection() {
    let fx = seed_rich().await;
    let (code, secret) = initiate(&client(&fx)).await;
    assert_eq!(code.len(), 6);

    let poller = client(&fx);
    assert!(
        matches!(poll(&poller, &secret).await, Poll::NotPermitted(r) if r.contains("not yet authorized")),
        "before authorization the poll says so, and the client keeps polling"
    );

    authorize(&fx, &code, fx.user_id).await;
    let Poll::Ok { name, token } = poll(&poller, &secret).await else {
        panic!("an authorized code must sign in");
    };
    assert_eq!(name, "guest");
    assert_eq!(fx.state.stores.resolve(&token).await.unwrap(), fx.user_id);
    let devices = fx.state.stores.tokens_for(fx.user_id).await.unwrap();
    assert!(devices
        .iter()
        .any(|d| d.device_id.starts_with("pharos-native-")));

    assert_eq!(
        poll(&poller, &secret).await,
        Poll::NotFound,
        "a code signs in once"
    );
}

#[actix_web::test]
async fn an_unknown_secret_is_not_found() {
    let fx = seed_rich().await;
    assert_eq!(poll(&client(&fx), "no-such-secret").await, Poll::NotFound);
}

/// Same rule as the Jellyfin finalize (V176): a disabled authorizer gets no
/// token, and the code is spent.
#[actix_web::test]
async fn a_disabled_authorizer_gets_no_token() {
    let fx = seed_rich().await;
    let c = client(&fx);
    let (code, secret) = initiate(&c).await;
    authorize(&fx, &code, fx.user_id).await;
    let mut record = fx.state.stores.get(fx.user_id).await.unwrap();
    record.policy.is_disabled = true;
    fx.state
        .stores
        .set_policy(fx.user_id, record.policy)
        .await
        .unwrap();
    let before = fx.state.stores.tokens_for(fx.user_id).await.unwrap().len();

    assert_eq!(poll(&c, &secret).await, Poll::NotFound);
    assert_eq!(
        fx.state.stores.tokens_for(fx.user_id).await.unwrap().len(),
        before
    );
    assert_eq!(poll(&c, &secret).await, Poll::NotFound, "the code is spent");
}

/// ODD: a Quick Connect sign-in is counted like a password one, under
/// `method="quick_connect"`; pending polls are not counted.
#[actix_web::test]
async fn quick_connect_sign_ins_are_counted_and_pending_polls_are_not() {
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    let fx = seed_rich().await;
    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);

    let c = client(&fx);
    let (code, secret) = initiate(&c).await;
    let _ = poll(&c, &secret).await;
    authorize(&fx, &code, fx.user_id).await;
    let _ = poll(&c, &secret).await;
    let _ = poll(&c, "no-such-secret").await;

    let count = |outcome: &str| {
        snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(k, ..)| {
                let key = k.key();
                key.name() == "pharos_native_login_total"
                    && key
                        .labels()
                        .any(|l| l.key() == "method" && l.value() == "quick_connect")
                    && key
                        .labels()
                        .any(|l| l.key() == "outcome" && l.value() == outcome)
            })
            .map(|(.., v)| match v {
                DebugValue::Counter(n) => n,
                other => panic!("{other:?}"),
            })
            .sum::<u64>()
    };
    assert_eq!(count("ok"), 1);
    assert_eq!(count("invalid"), 1, "the unknown secret");
    assert_eq!(count("pending"), 0);
}

/// Two connections polling one authorized secret at once: one signs in, the
/// other is told to start over and keeps no place in the user's connection
/// count (FR-010). Disarm check: without `settle_slot` on the refused poll,
/// the user holds 2.
#[actix_web::test]
async fn racing_polls_sign_in_once_and_leave_no_slot_behind() {
    let fx = seed_rich().await;
    let limits = rpc::limits::ConnectionLimits::default();
    let mk = || {
        let ctx = rpc::ConnectionCtx::admit(None, LOCAL, &limits, 16).unwrap();
        rpc::bootstrap_client(fx.state.clone(), ctx, &Connection::new())
    };
    let (a, b) = (mk(), mk());
    let (code, secret) = initiate(&a).await;
    authorize(&fx, &code, fx.user_id).await;

    let (ra, rb) = futures_util::join!(poll(&a, &secret), poll(&b, &secret));
    let oks = [&ra, &rb]
        .iter()
        .filter(|r| matches!(r, Poll::Ok { .. }))
        .count();
    assert_eq!(oks, 1, "{ra:?} {rb:?}");
    assert!([&ra, &rb].contains(&&Poll::NotFound), "{ra:?} {rb:?}");
    assert_eq!(limits.held_by(fx.user_id), 1);
    assert_eq!(limits.unauthenticated(), 1);
}

/// V428: a signed-in session approves a code, and the device then signs in
/// once, as that user.
#[actix_web::test]
async fn a_signed_in_user_approves_a_code_over_the_session() {
    let fx = seed_rich().await;
    let b = client(&fx);
    let (code, secret) = initiate(&b).await;
    let (session, _) = crate::login::login(&b, "guest", "hunter2")
        .await
        .ok()
        .unwrap();

    assert_eq!(
        authorize_session(&session, &code).await,
        AuthorizeAnswer::Ok
    );

    let poller = client(&fx);
    let Poll::Ok { name, token } = poll(&poller, &secret).await else {
        panic!("an approved code must sign in");
    };
    assert_eq!(name, "guest");
    assert_eq!(fx.state.stores.resolve(&token).await.unwrap(), fx.user_id);
    assert_eq!(
        poll(&poller, &secret).await,
        Poll::NotFound,
        "a code signs in once"
    );
}

/// A session that has signed out is refused as revoked, and the code stays
/// pending.
#[actix_web::test]
async fn a_signed_out_session_cannot_approve_a_code() {
    let fx = seed_rich().await;
    let b = client(&fx);
    let (code, secret) = initiate(&b).await;
    let (session, _) = crate::login::login(&b, "guest", "hunter2")
        .await
        .ok()
        .unwrap();

    let mut logout = session.logout_request();
    logout.get().init_ctx();
    let reply = logout.send().promise.await.unwrap();
    assert!(matches!(
        reply.get().unwrap().get_result().unwrap().which().unwrap(),
        done::Ok(())
    ));

    assert_eq!(
        authorize_session(&session, &code).await,
        AuthorizeAnswer::Revoked
    );

    let poller = client(&fx);
    assert!(
        matches!(poll(&poller, &secret).await, Poll::NotPermitted(r) if r.contains("not yet authorized")),
        "polling a code after refused authorization must still be pending"
    );
}

/// A code no device is showing answers notFound.
#[actix_web::test]
async fn an_unknown_code_is_not_found() {
    let fx = seed_rich().await;
    let (session, _) = crate::login::login(&client(&fx), "guest", "hunter2")
        .await
        .ok()
        .unwrap();
    assert_eq!(
        authorize_session(&session, "000000").await,
        AuthorizeAnswer::NotFound
    );
}

/// A blank code is refused as invalid, naming the `code` field.
#[actix_web::test]
async fn a_blank_code_is_invalid() {
    let fx = seed_rich().await;
    let (session, _) = crate::login::login(&client(&fx), "guest", "hunter2")
        .await
        .ok()
        .unwrap();
    assert_eq!(
        authorize_session(&session, "  ").await,
        AuthorizeAnswer::Invalid {
            field: "code".into()
        }
    );
}

/// After MAX_AUTHORIZE_FAILURES wrong codes the session is throttled, so
/// codes cannot be guessed.
#[actix_web::test]
async fn wrong_codes_are_throttled() {
    let fx = seed_rich().await;
    let (session, _) = crate::login::login(&client(&fx), "guest", "hunter2")
        .await
        .ok()
        .unwrap();
    for _ in 0..MAX_AUTHORIZE_FAILURES {
        assert_eq!(
            authorize_session(&session, "000000").await,
            AuthorizeAnswer::NotFound
        );
    }
    match authorize_session(&session, "000000").await {
        AuthorizeAnswer::Throttled { name } => {
            assert_eq!(name, "quick_connect_retry_after_secs");
        }
        other => panic!("expected throttled, got {other:?}"),
    }
}

/// A test server whose native API uses the given deadlines, recording
/// metrics into `recorder` when one is given.
async fn start_with_auth_deadline(
    auth_deadline_secs: NonZeroU32,
    max_unauthenticated_secs: NonZeroU32,
    recorder: Option<&'static metrics_util::debugging::DebuggingRecorder>,
) -> (actix_test::TestServer, Fixture) {
    let fx = seed_rich().await;
    let state = fx.state.clone();
    let limits = rpc::limits::ConnectionLimits::default();
    let server = actix_test::start(move || {
        if let Some(recorder) = recorder {
            std::mem::forget(metrics::set_default_local_recorder(recorder));
        }
        let cfg = NativeApiConfig {
            enabled: true,
            auth_deadline_secs,
            max_unauthenticated_secs,
            ..NativeApiConfig::default()
        };
        actix_web::App::new()
            .app_data(state.clone())
            .configure(rpc::configure(cfg, limits.clone()))
    });
    (server, fx)
}

/// How many `quick_connect_pending` auth deadline extensions `snapshotter`
/// has seen.
fn extended_count(snapshotter: &pharos_test_metrics::CumulativeSnapshotter) -> u64 {
    counter(
        snapshotter,
        "pharos_rpc_auth_deadline_extended_total",
        "quick_connect_pending",
    )
}

/// The summed value of counter `name` with label `reason` in `snapshotter`
/// (0 when it was never recorded).
fn counter(
    snapshotter: &pharos_test_metrics::CumulativeSnapshotter,
    name: &str,
    reason: &str,
) -> u64 {
    snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .filter(|(k, ..)| {
            let key = k.key();
            key.name() == name
                && key
                    .labels()
                    .any(|l| l.key() == "reason" && l.value() == reason)
        })
        .map(|(.., v)| match v {
            metrics_util::debugging::DebugValue::Counter(n) => n,
            other => panic!("{other:?}"),
        })
        .sum::<u64>()
}

/// B541, V436: a connection that initiates QuickConnect and polls every 500 ms
/// stays open past its auth deadline (2 s) for 5 s, every poll answers
/// notPermitted, and the extension counter increments for each pending poll.
#[actix_web::test]
async fn polling_pending_quick_connect_keeps_connection_open_past_deadline() {
    use metrics_util::debugging::DebuggingRecorder;
    let recorder = Box::leak(Box::new(DebuggingRecorder::new()));
    let snapshotter = pharos_test_metrics::snapshotter(recorder);
    let _metrics = metrics::set_default_local_recorder(recorder);

    let (server, _fx) = start_with_auth_deadline(
        NonZeroU32::new(2).unwrap(),
        NonZeroU32::new(300).unwrap(),
        Some(recorder),
    )
    .await;
    let (client, mut closed) = crate::revocation::connect(&server).await;
    let (_code, secret) = initiate(&client).await;

    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(closed.try_recv().is_err(), "connection closed unexpectedly");
        let res = tokio::time::timeout(Duration::from_secs(1), try_poll(&client, &secret))
            .await
            .expect("poll timed out")
            .expect("poll failed");
        assert!(
            matches!(res, Poll::NotPermitted(ref r) if r.contains("not yet authorized")),
            "every poll answers notPermitted while pending: {res:?}"
        );
    }
    assert!(
        closed.try_recv().is_err(),
        "connection must stay open after 5 s"
    );
    // The counter is bumped by the socket's timer task after the reply is
    // sent, so the last poll's increment may not have landed yet.
    assert!(
        extended_count(&snapshotter) >= 9,
        "extension counter must go up on pending polls: {}",
        extended_count(&snapshotter)
    );
}

/// B541, V436: a connection that initiates QuickConnect and never polls closes
/// with reason "auth_deadline" within ~4 s (auth deadline = 2 s).
#[actix_web::test]
async fn an_initiator_that_never_polls_closes_at_the_auth_deadline() {
    let (server, _fx) = start_with_auth_deadline(
        NonZeroU32::new(2).unwrap(),
        NonZeroU32::new(300).unwrap(),
        None,
    )
    .await;
    let (client, closed) = crate::revocation::connect(&server).await;
    let (_code, _secret) = initiate(&client).await;

    let reason = tokio::time::timeout(Duration::from_secs(4), closed)
        .await
        .expect("connection must close within ~4 s")
        .expect("close frame");
    assert_eq!(reason.as_deref(), Some("auth_deadline"));
}

/// B541, V436: a connection that polls only with an unknown secret (answers
/// notFound) closes with "auth_deadline" within ~4 s, and the extension counter
/// does not increment.
#[actix_web::test]
async fn polling_with_an_unknown_secret_does_not_extend_auth_deadline() {
    use metrics_util::debugging::DebuggingRecorder;
    let recorder = Box::leak(Box::new(DebuggingRecorder::new()));
    let snapshotter = pharos_test_metrics::snapshotter(recorder);
    let _metrics = metrics::set_default_local_recorder(recorder);

    let (server, _fx) = start_with_auth_deadline(
        NonZeroU32::new(2).unwrap(),
        NonZeroU32::new(300).unwrap(),
        Some(recorder),
    )
    .await;
    let (client, closed) = crate::revocation::connect(&server).await;

    let reason = tokio::select! {
        _ = async {
            loop {
                if let Ok(res) = try_poll(&client, "unknown-secret").await {
                    assert_eq!(res, Poll::NotFound);
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        } => unreachable!(),
        r = tokio::time::timeout(Duration::from_secs(4), closed) => {
            r.expect("connection must close within ~4 s").expect("close frame")
        }
    };
    assert_eq!(reason.as_deref(), Some("auth_deadline"));
    assert_eq!(
        extended_count(&snapshotter),
        0,
        "extension counter must not increment on unknown secret"
    );
}

/// B541, V436: pending polls stop extending the deadline once the connection
/// would stay unauthenticated past `max_unauthenticated_secs`, so re-minting
/// codes cannot hold one socket open forever. Deadline 2 s, cap 3 s, a poll
/// every 500 ms: the poll at 1.5 s would run to 3.5 s and is refused, so the
/// connection closes at about 3 s.
#[actix_web::test]
async fn pending_polls_stop_extending_at_the_unauthenticated_cap() {
    use metrics_util::debugging::DebuggingRecorder;
    let recorder = Box::leak(Box::new(DebuggingRecorder::new()));
    let snapshotter = pharos_test_metrics::snapshotter(recorder);
    let _metrics = metrics::set_default_local_recorder(recorder);

    let (server, _fx) = start_with_auth_deadline(
        NonZeroU32::new(2).unwrap(),
        NonZeroU32::new(3).unwrap(),
        Some(recorder),
    )
    .await;
    let (client, closed) = crate::revocation::connect(&server).await;
    let (_code, secret) = initiate(&client).await;

    let reason = tokio::select! {
        _ = async {
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let _ = try_poll(&client, &secret).await;
            }
        } => unreachable!(),
        r = tokio::time::timeout(Duration::from_secs(5), closed) => {
            r.expect("connection must close at the cap").expect("close frame")
        }
    };
    assert_eq!(reason.as_deref(), Some("auth_deadline"));
    assert!(
        counter(
            &snapshotter,
            "pharos_rpc_auth_deadline_extension_refused_total",
            "max_unauthenticated",
        ) >= 1,
        "the refused extension must be counted"
    );
}
