//! T024: `Bootstrap.login` checks a password through the login throttle
//! (V174) and answers with a session and the device token a native app keeps
//! for `resume`; `Session.logout` revokes that token and announces the user,
//! so the connection it signed in drops (T025).

use std::rc::Rc;

use pharos_api::pharos_capnp::{bootstrap, done, error, login_result, me_result, session};
use pharos_api::Connection;
use pharos_core::TokenStore;
use pharos_server::api::pharos::rpc;

use crate::bootstrap::LOCAL;
use crate::common::{seed_rich, Fixture};

fn client(fx: &Fixture, max_per_user: u32) -> bootstrap::Client {
    let ctx = rpc::ConnectionCtx::admit(
        None,
        LOCAL,
        &rpc::limits::ConnectionLimits::default(),
        max_per_user,
    )
    .unwrap();
    let conn = Rc::new(Connection::new());
    rpc::bootstrap_client(fx.state.clone(), ctx, &conn)
}

/// The kind of a login error, without its payload.
#[derive(Debug, PartialEq)]
enum Kind {
    NotPermitted,
    Limit { name: String, value: u64 },
    Other,
}

async fn login(
    client: &bootstrap::Client,
    name: &str,
    password: &str,
) -> Result<(session::Client, String), (Kind, String)> {
    let mut req = client.login_request();
    req.get().init_ctx();
    req.get().set_name(name);
    req.get().set_password(password);
    let reply = req.send().promise.await.unwrap();
    match reply.get().unwrap().get_result().unwrap().which().unwrap() {
        login_result::Ok(ok) => {
            let ok = ok.unwrap();
            Ok((
                ok.get_session().unwrap(),
                ok.get_device_token().unwrap().to_string().unwrap(),
            ))
        }
        login_result::Error(e) => {
            let e = e.unwrap();
            let reason = e.get_reason().unwrap().to_string().unwrap();
            let kind = match e.get_kind().which().unwrap() {
                error::kind::NotPermitted(_) => Kind::NotPermitted,
                error::kind::Limit(l) => {
                    let l = l.unwrap();
                    Kind::Limit {
                        name: l.get_name().unwrap().to_string().unwrap(),
                        value: l.get_value(),
                    }
                }
                _ => Kind::Other,
            };
            Err((kind, reason))
        }
    }
}

async fn me_name(session: &session::Client) -> Option<String> {
    let mut me = session.me_request();
    me.get().init_ctx();
    let reply = me.send().promise.await.ok()?;
    match reply.get().unwrap().get_result().unwrap().which().unwrap() {
        me_result::Me(me) => Some(me.unwrap().get_name().unwrap().to_string().unwrap()),
        me_result::Error(_) => None,
    }
}

#[actix_web::test]
async fn a_right_password_returns_a_session_and_a_device_token_for_resume() {
    let fx = seed_rich().await;
    let (session, token) = login(&client(&fx, 16), "guest", "hunter2")
        .await
        .unwrap_or_else(|(k, r)| panic!("login refused: {k:?} {r}"));
    assert_eq!(me_name(&session).await.as_deref(), Some("guest"));
    assert_eq!(fx.state.stores.resolve(&token).await.unwrap(), fx.user_id);

    // The token is the one a native app keeps: it resumes on a new connection.
    let other = client(&fx, 16);
    let mut req = other.resume_request();
    req.get().init_ctx();
    req.get().set_device_token(&token);
    let resumed = req.send().pipeline.get_result().get_session();
    assert_eq!(me_name(&resumed).await.as_deref(), Some("guest"));

    // The device is the server's choice, never the client's (B235).
    let devices = fx.state.stores.tokens_for(fx.user_id).await.unwrap();
    assert!(devices
        .iter()
        .any(|d| d.device_id.starts_with("pharos-native-")));
}

#[actix_web::test]
async fn a_wrong_password_or_unknown_user_is_the_same_refusal() {
    let fx = seed_rich().await;
    let c = client(&fx, 16);
    let wrong = login(&c, "guest", "nope").await.err().unwrap();
    let unknown = login(&c, "nobody", "hunter2").await.err().unwrap();
    assert_eq!(wrong.0, Kind::NotPermitted);
    assert_eq!(
        wrong, unknown,
        "the answer must not reveal which users exist"
    );
}

/// V174: the native login shares the throttle; the refusal carries the wait.
#[actix_web::test]
async fn repeated_failures_are_throttled_with_the_wait_in_seconds() {
    let fx = seed_rich().await;
    let c = client(&fx, 16);
    for _ in 0..50 {
        match login(&c, "guest", "nope").await {
            Err((Kind::Limit { name, value }, _)) => {
                assert_eq!(name, "login_retry_after_secs");
                assert!(value > 0);
                return;
            }
            Err((Kind::NotPermitted, _)) => {}
            other => panic!("unexpected: {:?}", other.err()),
        }
    }
    panic!("never throttled");
}

/// FR-010: login honours the per-user connection cap like `resume`, and the
/// token it issued is not left behind.
#[actix_web::test]
async fn login_beyond_the_per_user_cap_is_refused_and_leaves_no_token() {
    let fx = seed_rich().await;
    let limits = rpc::limits::ConnectionLimits::default();
    let mut open = Vec::new();
    for _ in 0..2 {
        let ctx = rpc::ConnectionCtx::admit(None, LOCAL, &limits, 1).unwrap();
        let conn = Rc::new(Connection::new());
        open.push(rpc::bootstrap_client(fx.state.clone(), ctx, &conn));
    }
    let before = fx.state.stores.tokens_for(fx.user_id).await.unwrap().len();
    login(&open[0], "guest", "hunter2").await.ok().unwrap();
    let refused = login(&open[1], "guest", "hunter2").await.err().unwrap();
    assert!(
        matches!(&refused.0, Kind::Limit { name, .. } if name == "connections_per_user"),
        "{refused:?}"
    );
    let after = fx.state.stores.tokens_for(fx.user_id).await.unwrap().len();
    assert_eq!(after, before + 1, "only the admitted login keeps a token");
}

#[actix_web::test]
async fn logout_revokes_the_sessions_token_and_announces_the_user() {
    let fx = seed_rich().await;
    let (session, token) = login(&client(&fx, 16), "guest", "hunter2")
        .await
        .ok()
        .unwrap();
    let mut announced = fx.state.revocations.subscribe();
    let mut req = session.logout_request();
    req.get().init_ctx();
    let reply = req.send().promise.await.unwrap();
    assert!(matches!(
        reply.get().unwrap().get_result().unwrap().which().unwrap(),
        done::Ok(())
    ));
    assert!(fx.state.stores.resolve(&token).await.is_err());
    assert_eq!(announced.try_recv().unwrap(), fx.user_id);
    assert_eq!(
        me_name(&session).await,
        None,
        "a logged-out session must not serve the user"
    );
}

/// `logout` on a resumed session revokes the token it resumed with, and
/// leaves the user's other devices signed in.
#[actix_web::test]
async fn logout_after_resume_revokes_only_that_token() {
    let fx = seed_rich().await;
    let second = fx
        .state
        .stores
        .issue(fx.user_id, "other-device")
        .await
        .unwrap();
    let c = client(&fx, 16);
    let mut req = c.resume_request();
    req.get().init_ctx();
    req.get().set_device_token(&fx.user_token);
    let session = req.send().pipeline.get_result().get_session();
    let mut logout = session.logout_request();
    logout.get().init_ctx();
    logout.send().promise.await.unwrap();
    assert!(fx.state.stores.resolve(&fx.user_token).await.is_err());
    assert!(fx.state.stores.resolve(second.0.expose()).await.is_ok());
}

/// ODD: native logins are counted by transport and outcome, like the cookie
/// one. Disarm check: without `record_login` in `password_login`, 0.
#[actix_web::test]
async fn each_native_login_is_counted_by_outcome() {
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    let fx = seed_rich().await;
    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);

    let c = client(&fx, 16);
    let _ = login(&c, "guest", "hunter2").await;
    let _ = login(&c, "guest", "nope").await;

    let count = |outcome: &str| {
        snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .find(|(k, ..)| {
                let key = k.key();
                key.name() == "pharos_native_login_total"
                    && key
                        .labels()
                        .any(|l| l.key() == "transport" && l.value() == "rpc")
                    && key
                        .labels()
                        .any(|l| l.key() == "outcome" && l.value() == outcome)
            })
            .map_or(0, |(.., v)| match v {
                DebugValue::Counter(n) => n,
                other => panic!("{other:?}"),
            })
    };
    assert_eq!(count("ok"), 1);
    assert_eq!(count("invalid"), 1);
}

/// FR-010: a logout gives back the user's place, though the connection stays
/// open. Disarm check: without `sign_out` in `end_token`, the second login is
/// refused `connections_per_user`.
#[actix_web::test]
async fn logout_releases_the_connection_cap() {
    let fx = seed_rich().await;
    let limits = rpc::limits::ConnectionLimits::default();
    let open: Vec<_> = (0..2)
        .map(|_| {
            let ctx = rpc::ConnectionCtx::admit(None, LOCAL, &limits, 1).unwrap();
            rpc::bootstrap_client(fx.state.clone(), ctx, &Connection::new())
        })
        .collect();
    let (session, _) = login(&open[0], "guest", "hunter2").await.ok().unwrap();
    let mut logout = session.logout_request();
    logout.get().init_ctx();
    logout.send().promise.await.unwrap();
    assert_eq!(limits.held_by(fx.user_id), 0);
    login(&open[1], "guest", "hunter2")
        .await
        .unwrap_or_else(|(k, r)| panic!("the logged-out place must be free: {k:?} {r}"));
}
