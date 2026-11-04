//! V361: `Session.changePassword` checks the current password through the
//! login throttle (V174), refuses an empty new one (V211), stores the new
//! one and signs the user's other sessions out while keeping the caller's.

use pharos_api::pharos_capnp::{done, error, session};
use pharos_core::TokenStore;

use crate::common::seed_rich;
use crate::login::{client, login, me_name, Kind};

/// How `changePassword` answered: ok, or the error's kind and reason.
#[derive(Debug, PartialEq)]
enum Answer {
    Ok,
    NotPermitted(String),
    Invalid { field: String },
    Throttled { name: String },
    Other(String),
}

async fn change(session: &session::Client, current: &str, new: &str) -> Answer {
    let mut req = session.change_password_request();
    req.get().init_ctx();
    req.get().set_current(current);
    req.get().set_new(new);
    let reply = req.send().promise.await.unwrap();
    match reply.get().unwrap().get_result().unwrap().which().unwrap() {
        done::Ok(()) => Answer::Ok,
        done::Error(e) => {
            let e = e.unwrap();
            let reason = e.get_reason().unwrap().to_string().unwrap();
            match e.get_kind().which().unwrap() {
                error::kind::NotPermitted(_) => Answer::NotPermitted(reason),
                error::kind::Invalid(i) => Answer::Invalid {
                    field: i.unwrap().get_field().unwrap().to_string().unwrap(),
                },
                error::kind::Limit(l) => Answer::Throttled {
                    name: l.unwrap().get_name().unwrap().to_string().unwrap(),
                },
                _ => Answer::Other(reason),
            }
        }
    }
}

#[actix_web::test]
async fn a_right_current_password_swaps_it_and_signs_out_only_the_other_sessions() {
    let fx = seed_rich().await;
    let (session, token) = login(&client(&fx, 16), "guest", "hunter2")
        .await
        .ok()
        .unwrap();
    let other = fx
        .state
        .stores
        .issue(fx.user_id, "other-device")
        .await
        .unwrap();

    assert_eq!(
        change(&session, "hunter2", "correct horse").await,
        Answer::Ok
    );

    let c = client(&fx, 16);
    assert_eq!(
        login(&c, "guest", "hunter2").await.err().map(|e| e.0),
        Some(Kind::NotPermitted),
        "the old password must stop working"
    );
    assert!(login(&c, "guest", "correct horse").await.is_ok());
    assert!(
        fx.state.stores.resolve(other.0.expose()).await.is_err(),
        "another device's session must be signed out (B235)"
    );
    assert!(fx.state.stores.resolve(&token).await.is_ok());
    assert_eq!(
        me_name(&session).await.as_deref(),
        Some("guest"),
        "the session that changed the password stays signed in"
    );
}

#[actix_web::test]
async fn a_wrong_current_password_changes_nothing() {
    let fx = seed_rich().await;
    let (session, _) = login(&client(&fx, 16), "guest", "hunter2")
        .await
        .ok()
        .unwrap();
    let other = fx
        .state
        .stores
        .issue(fx.user_id, "other-device")
        .await
        .unwrap();
    assert!(matches!(
        change(&session, "nope", "correct horse").await,
        Answer::NotPermitted(_)
    ));
    assert!(change(&session, "", "correct horse").await != Answer::Ok);
    assert!(login(&client(&fx, 16), "guest", "hunter2").await.is_ok());
    assert!(fx.state.stores.resolve(other.0.expose()).await.is_ok());
}

/// V211: no account may hold an empty password, so an empty new one is
/// refused as invalid, before the current one is checked.
#[actix_web::test]
async fn an_empty_new_password_is_refused() {
    let fx = seed_rich().await;
    let (session, _) = login(&client(&fx, 16), "guest", "hunter2")
        .await
        .ok()
        .unwrap();
    assert_eq!(
        change(&session, "hunter2", "").await,
        Answer::Invalid {
            field: "new".into()
        }
    );
    assert!(login(&client(&fx, 16), "guest", "hunter2").await.is_ok());
}

/// V174: a stolen session cannot buy unthrottled guesses at the password.
#[actix_web::test]
async fn repeated_wrong_current_passwords_are_throttled() {
    let fx = seed_rich().await;
    let (session, _) = login(&client(&fx, 16), "guest", "hunter2")
        .await
        .ok()
        .unwrap();
    for _ in 0..50 {
        match change(&session, "nope", "correct horse").await {
            Answer::Throttled { name } => {
                assert_eq!(name, "login_retry_after_secs");
                return;
            }
            Answer::NotPermitted(_) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }
    panic!("never throttled");
}
