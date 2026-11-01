//! T025: native Quick Connect. `quickConnectInitiate` returns a code to show
//! and a secret to keep; once a signed-in user authorizes the code (on the
//! Jellyfin side, `/QuickConnect/Authorize`), `quickConnectPoll(secret)`
//! answers once with a session and device token, as `login` does. The poll
//! may come over a new connection: an unauthenticated one closes after
//! `AUTH_DEADLINE`, well inside the code's lifetime.

use pharos_api::pharos_capnp::{bootstrap, error, login_result, me_result, quick_connect_result};
use pharos_api::Connection;
use pharos_core::{TokenStore, UserId, UserStore};
use pharos_server::api::pharos::rpc;
use pharos_server::quick_connect::QcMsg;

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

#[derive(Debug, PartialEq)]
enum Poll {
    Ok { name: String, token: String },
    NotPermitted(String),
    NotFound,
    Other(String),
}

async fn poll(client: &bootstrap::Client, secret: &str) -> Poll {
    let mut req = client.quick_connect_poll_request();
    req.get().init_ctx();
    req.get().set_secret(secret);
    let reply = req.send().promise.await.unwrap();
    match reply.get().unwrap().get_result().unwrap().which().unwrap() {
        login_result::Ok(ok) => {
            let ok = ok.unwrap();
            let mut me = ok.get_session().unwrap().me_request();
            me.get().init_ctx();
            let me = me.send().promise.await.unwrap();
            let name = match me.get().unwrap().get_result().unwrap().which().unwrap() {
                me_result::Me(me) => me.unwrap().get_name().unwrap().to_string().unwrap(),
                me_result::Error(_) => panic!("the quick connect session must serve"),
            };
            Poll::Ok {
                name,
                token: ok.get_device_token().unwrap().to_string().unwrap(),
            }
        }
        login_result::Error(e) => {
            let e = e.unwrap();
            let reason = e.get_reason().unwrap().to_string().unwrap();
            match e.get_kind().which().unwrap() {
                error::kind::NotPermitted(_) => Poll::NotPermitted(reason),
                error::kind::NotFound(_) => Poll::NotFound,
                _ => Poll::Other(reason),
            }
        }
    }
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
