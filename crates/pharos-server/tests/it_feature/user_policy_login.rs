//! T68 login-time `UserPolicy` enforcement (V577-V580), driven over the
//! Jellyfin wire: `POST /Users/AuthenticateByName` with policies written by
//! `POST /Users/{id}/Policy`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::test;
use serde_json::{json, Value};

use crate::common;
use common::{build_app, seed_rich, Fixture};

const AUTH_HEADER: &str =
    r#"MediaBrowser Client="Test", Device="Rig", DeviceId="rig-1", Version="1.0""#;

fn guest(f: &Fixture) -> String {
    f.user_id.0.simple().to_string()
}

async fn post_policy(f: &Fixture, policy: Value) -> u16 {
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::post()
        .uri(&format!("/Users/{}/Policy", guest(f)))
        .insert_header(("X-Emby-Token", f.admin_token.as_str()))
        .set_json(&policy)
        .to_request();
    test::call_service(&app, req).await.status().as_u16()
}

/// One `AuthenticateByName`; yields the status and the raw body.
async fn login(f: &Fixture, name: &str, pw: &str) -> (u16, Vec<u8>) {
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::post()
        .uri("/Users/AuthenticateByName")
        .insert_header(("X-Emby-Authorization", AUTH_HEADER))
        .set_json(json!({ "Username": name, "Pw": pw }))
        .to_request();
    match test::try_call_service(&app, req).await {
        Ok(r) => {
            let status = r.status().as_u16();
            (status, test::read_body(r).await.to_vec())
        }
        Err(e) => {
            let r = e.error_response();
            let status = r.status().as_u16();
            let body = actix_web::body::to_bytes(r.into_body()).await.ok();
            (status, body.map(|b| b.to_vec()).unwrap_or_default())
        }
    }
}

async fn guest_policy(f: &Fixture) -> Value {
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::get()
        .uri("/Users")
        .insert_header(("X-Emby-Token", f.admin_token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let users: Value = serde_json::from_slice(&body).unwrap();
    let id = guest(f);
    users
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["Id"] == id)
        .expect("guest present in /Users")["Policy"]
        .clone()
}

// ---- V577: LoginAttemptsBeforeLockout ----

#[actix_web::test]
async fn lockout_disables_the_user_at_the_limit_and_success_resets_the_count() {
    let f = seed_rich().await;
    assert_eq!(
        post_policy(&f, json!({ "LoginAttemptsBeforeLockout": 3 })).await,
        204
    );

    // Two misses, a hit, two misses, a hit: the hit zeroes the count, so
    // four misses in total never reach three in a row.
    for _ in 0..2 {
        assert_eq!(login(&f, "guest", "wrong").await.0, 401);
    }
    assert_eq!(login(&f, "guest", "hunter2").await.0, 200);
    for _ in 0..2 {
        assert_eq!(login(&f, "guest", "wrong").await.0, 401);
    }
    assert_eq!(login(&f, "guest", "hunter2").await.0, 200);
    assert_eq!(guest_policy(&f).await["IsDisabled"], false);

    // Three in a row lock the account: now even the right password is refused.
    for _ in 0..3 {
        assert_eq!(login(&f, "guest", "wrong").await.0, 401);
    }
    assert_eq!(guest_policy(&f).await["IsDisabled"], true);
    assert_eq!(login(&f, "guest", "hunter2").await.0, 401);
}

#[actix_web::test]
async fn an_admin_can_re_enable_a_locked_out_user_with_a_clean_count() {
    let f = seed_rich().await;
    let limit = json!({ "LoginAttemptsBeforeLockout": 2 });
    assert_eq!(post_policy(&f, limit.clone()).await, 204);
    for _ in 0..2 {
        assert_eq!(login(&f, "guest", "wrong").await.0, 401);
    }
    assert_eq!(login(&f, "guest", "hunter2").await.0, 401);

    // Re-enable (IsDisabled false, same limit).
    assert_eq!(post_policy(&f, limit).await, 204);
    assert_eq!(guest_policy(&f).await["IsDisabled"], false);
    // One miss would re-lock a user whose count was left at the limit.
    assert_eq!(login(&f, "guest", "wrong").await.0, 401);
    assert_eq!(login(&f, "guest", "hunter2").await.0, 200);
}

#[actix_web::test]
async fn a_limit_of_zero_means_jellyfins_default_of_three() {
    let f = seed_rich().await;
    assert_eq!(
        post_policy(&f, json!({ "LoginAttemptsBeforeLockout": 0 })).await,
        204
    );
    for _ in 0..2 {
        assert_eq!(login(&f, "guest", "wrong").await.0, 401);
    }
    assert_eq!(login(&f, "guest", "hunter2").await.0, 200);
    for _ in 0..3 {
        assert_eq!(login(&f, "guest", "wrong").await.0, 401);
    }
    assert_eq!(login(&f, "guest", "hunter2").await.0, 401);
}

#[actix_web::test]
async fn minus_one_never_locks_out() {
    let f = seed_rich().await;
    for _ in 0..8 {
        assert_eq!(login(&f, "guest", "wrong").await.0, 401);
    }
    assert_eq!(login(&f, "guest", "hunter2").await.0, 200);
    assert_eq!(guest_policy(&f).await["IsDisabled"], false);
}

#[actix_web::test]
async fn a_failed_login_answers_alike_for_a_locked_known_and_unknown_user() {
    let f = seed_rich().await;
    assert_eq!(
        post_policy(&f, json!({ "LoginAttemptsBeforeLockout": 1 })).await,
        204
    );
    let unknown = login(&f, "nobody", "wrong").await;
    let known_wrong = login(&f, "guest", "wrong").await; // locks the account
    let locked_right = login(&f, "guest", "hunter2").await;
    assert_eq!(unknown, known_wrong, "unknown vs wrong password differ");
    assert_eq!(unknown, locked_right, "unknown vs locked account differ");
}

/// A lockout disables the user, so the revocation bus must tell their open
/// native sockets to re-check, as an admin disabling them does.
#[actix_web::test]
async fn a_lockout_announces_the_users_revocation() {
    let f = seed_rich().await;
    assert_eq!(
        post_policy(&f, json!({ "LoginAttemptsBeforeLockout": 2 })).await,
        204
    );
    let mut rx = f.state.revocations.subscribe();
    assert_eq!(login(&f, "guest", "wrong").await.0, 401);
    assert!(rx.try_recv().is_err(), "a miss below the limit announced");
    assert_eq!(login(&f, "guest", "wrong").await.0, 401);
    assert_eq!(rx.try_recv().unwrap(), f.user_id);
    assert!(rx.try_recv().is_err(), "announced more than once");
}

/// A wrong CurrentPw on the signed-in user's own password change is
/// throttled but never counts toward lockout: a stolen bearer token must not
/// be able to disable the real user.
#[actix_web::test]
async fn wrong_current_passwords_on_change_password_never_lock_the_user_out() {
    let f = seed_rich().await;
    assert_eq!(
        post_policy(&f, json!({ "LoginAttemptsBeforeLockout": 2 })).await,
        204
    );
    let app = test::init_service(build_app(f.state.clone())).await;
    for _ in 0..5 {
        let req = test::TestRequest::post()
            .uri(&format!("/Users/{}/Password", guest(&f)))
            .insert_header(("X-Emby-Token", f.user_token.as_str()))
            .set_json(json!({ "CurrentPw": "wrong", "NewPw": "new-pass" }))
            .to_request();
        assert_eq!(test::call_service(&app, req).await.status().as_u16(), 401);
    }
    assert_eq!(guest_policy(&f).await["IsDisabled"], false);
    assert_eq!(login(&f, "guest", "hunter2").await.0, 200);
}

// ---- V578: AccessSchedules ----

const DAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

/// A schedule that is closed now: a whole day three days from today, so a
/// midnight rollover during the test cannot open it.
fn closed_schedule() -> Value {
    let (today, _) = pharos_server::local_time::now_local();
    json!([{
        "DayOfWeek": DAYS[usize::from((today + 3) % 7)],
        "StartHour": 0.0,
        "EndHour": 24.0
    }])
}

fn open_schedule() -> Value {
    json!([{ "DayOfWeek": "Everyday", "StartHour": 0.0, "EndHour": 24.0 }])
}

async fn get_me(f: &Fixture, token: &str) -> u16 {
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::get()
        .uri("/Users/Me")
        .insert_header(("X-Emby-Token", token))
        .to_request();
    test::call_service(&app, req).await.status().as_u16()
}

#[actix_web::test]
async fn login_outside_the_access_schedule_is_403_and_inside_is_200() {
    let f = seed_rich().await;
    assert_eq!(
        post_policy(&f, json!({ "AccessSchedules": closed_schedule() })).await,
        204
    );
    // Jellyfin: SecurityException -> 403, after the password was checked.
    assert_eq!(login(&f, "guest", "hunter2").await.0, 403);
    // A wrong password stays a plain 401: the schedule is not revealed.
    assert_eq!(login(&f, "guest", "wrong").await.0, 401);

    assert_eq!(
        post_policy(&f, json!({ "AccessSchedules": open_schedule() })).await,
        204
    );
    assert_eq!(login(&f, "guest", "hunter2").await.0, 200);
}

#[actix_web::test]
async fn an_existing_token_is_403_outside_the_schedule_and_works_inside_it() {
    let f = seed_rich().await;
    assert_eq!(get_me(&f, &f.user_token).await, 200);
    assert_eq!(
        post_policy(&f, json!({ "AccessSchedules": closed_schedule() })).await,
        204
    );
    assert_eq!(get_me(&f, &f.user_token).await, 403);
    assert_eq!(
        post_policy(&f, json!({ "AccessSchedules": open_schedule() })).await,
        204
    );
    assert_eq!(get_me(&f, &f.user_token).await, 200);
}

// ---- V579: MaxActiveSessions ----

async fn login_from(f: &Fixture, device: &str, pw: &str) -> u16 {
    let app = test::init_service(build_app(f.state.clone())).await;
    let header = format!(
        r#"MediaBrowser Client="Test", Device="{device}", DeviceId="{device}", Version="1.0""#
    );
    let req = test::TestRequest::post()
        .uri("/Users/AuthenticateByName")
        .insert_header(("X-Emby-Authorization", header))
        .set_json(json!({ "Username": "guest", "Pw": pw }))
        .to_request();
    test::call_service(&app, req).await.status().as_u16()
}

#[actix_web::test]
async fn a_new_device_is_403_at_the_session_cap_and_200_below_it() {
    let f = seed_rich().await;
    // The fixture already holds one session for guest.
    assert_eq!(
        post_policy(&f, json!({ "MaxActiveSessions": 2 })).await,
        204
    );
    assert_eq!(login_from(&f, "dev-a", "hunter2").await, 200);
    // Two held now: a third device is refused, after the password check.
    assert_eq!(login_from(&f, "dev-b", "hunter2").await, 403);
    assert_eq!(login_from(&f, "dev-b", "wrong").await, 401);
    // Signing in again from a device that already has a session is not a
    // new session.
    assert_eq!(login_from(&f, "dev-a", "hunter2").await, 200);
    // Raising the cap, or removing it, lets the third device in.
    assert_eq!(
        post_policy(&f, json!({ "MaxActiveSessions": 3 })).await,
        204
    );
    assert_eq!(login_from(&f, "dev-b", "hunter2").await, 200);
    assert_eq!(login_from(&f, "dev-c", "hunter2").await, 403);
    assert_eq!(
        post_policy(&f, json!({ "MaxActiveSessions": 0 })).await,
        204
    );
    assert_eq!(login_from(&f, "dev-c", "hunter2").await, 200);
}

/// The cap is checked and the token inserted as one step: of two devices
/// racing for the last session, exactly one gets it.
#[actix_web::test]
async fn two_concurrent_logins_at_the_last_session_let_exactly_one_in() {
    let f = seed_rich().await;
    // The fixture already holds one session for guest: one slot is left.
    assert_eq!(
        post_policy(&f, json!({ "MaxActiveSessions": 2 })).await,
        204
    );
    let (a, b) = tokio::join!(
        login_from(&f, "dev-a", "hunter2"),
        login_from(&f, "dev-b", "hunter2")
    );
    let mut got = [a, b];
    got.sort_unstable();
    assert_eq!(got, [200, 403]);
}

/// A refused gate must not burn the one-time Quick Connect code (as on the
/// native path): the same code works once the gate opens.
#[actix_web::test]
async fn a_gated_quick_connect_finalize_keeps_the_code() {
    let f = seed_rich().await;
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::post()
        .uri("/QuickConnect/Initiate")
        .insert_header(("X-Emby-Authorization", AUTH_HEADER))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: Value = serde_json::from_slice(&body).unwrap();
    let code = v["Code"].as_str().unwrap().to_string();
    let secret = v["Secret"].as_str().unwrap().to_string();
    let req = test::TestRequest::post()
        .uri(&format!("/QuickConnect/Authorize?Code={code}"))
        .insert_header(("X-Emby-Token", f.user_token.as_str()))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 200);

    // The fixture's session already fills a cap of one.
    assert_eq!(
        post_policy(&f, json!({ "MaxActiveSessions": 1 })).await,
        204
    );
    let finalize = || {
        test::TestRequest::post()
            .uri("/Users/AuthenticateWithQuickConnect")
            .set_json(json!({ "Secret": secret }))
            .to_request()
    };
    assert_eq!(
        test::call_service(&app, finalize()).await.status().as_u16(),
        403
    );
    assert_eq!(
        post_policy(&f, json!({ "MaxActiveSessions": 0 })).await,
        204
    );
    assert_eq!(
        test::call_service(&app, finalize()).await.status().as_u16(),
        200
    );
}
