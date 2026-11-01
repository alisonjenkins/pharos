//! `POST /Users/AuthenticateByName` is the one internet-reachable password
//! check: it must slow online guessing (429 + Retry-After) without ever
//! getting in the way of a household that mistypes a password now and then.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{http::StatusCode, test, web, App};
use metrics_util::debugging::{DebugValue, DebuggingRecorder};
use pharos_core::{SecretString, UserId, UserPolicy, UserRecord, UserStore};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    client_ip::TrustedProxies,
    config::LoginThrottleConfig,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

const TV_HEADER: &str = r#"MediaBrowser Client="Jellyfin Android TV", Device="Shield", DeviceId="tv-1", Version="0.18.0""#;

fn policy(user: u32, ip: u32) -> LoginThrottleConfig {
    LoginThrottleConfig {
        max_failures_per_user: user,
        max_failures_per_ip: ip,
        window_secs: 900,
        max_backoff_secs: 300,
    }
}

async fn state(throttle: Option<(LoginThrottleConfig, &[&str])>) -> web::Data<AppState> {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("hunter2")).unwrap();
    stores
        .create(UserRecord {
            id: UserId::new(),
            name: "ali".into(),
            password_hash: hash,
            policy: UserPolicy::default(),
        })
        .await
        .unwrap();
    let state = AppState::new(stores, "srv".into());
    web::Data::new(match throttle {
        None => state,
        Some((cfg, proxies)) => {
            let proxies: Vec<String> = proxies.iter().map(|p| p.to_string()).collect();
            state.with_login_throttle(cfg, TrustedProxies::parse(&proxies).unwrap())
        }
    })
}

macro_rules! app {
    ($state:expr) => {
        test::init_service(
            App::new()
                .app_data($state.clone())
                .wrap(LowercasePath)
                .configure(jellyfin::configure),
        )
        .await
    };
}

fn outcome(
    status: StatusCode,
    headers: &actix_web::http::header::HeaderMap,
) -> (StatusCode, Option<String>) {
    let retry = headers
        .get("retry-after")
        .map(|v| v.to_str().unwrap().to_string());
    (status, retry)
}

/// One login attempt; yields the status and the `Retry-After` header.
macro_rules! login {
    ($app:expr, $user:expr, $pw:expr, $peer:expr, $xff:expr) => {
        async {
            let mut req = test::TestRequest::post()
                .uri("/Users/AuthenticateByName")
                .peer_addr(format!("{}:50000", $peer).parse().unwrap())
                .insert_header(("X-Emby-Authorization", TV_HEADER))
                .set_json(serde_json::json!({"Username": $user, "Pw": $pw}));
            let xff: Option<&str> = $xff;
            if let Some(x) = xff {
                req = req.insert_header(("X-Forwarded-For", x));
            }
            match test::try_call_service(&$app, req.to_request()).await {
                Ok(r) => outcome(r.status(), r.headers()),
                Err(e) => {
                    let r = e.error_response();
                    outcome(r.status(), r.headers())
                }
            }
        }
    };
}

fn throttled(rec: &DebuggingRecorder, reason: &str) -> u64 {
    rec.snapshotter()
        .snapshot()
        .into_vec()
        .into_iter()
        .find_map(|(key, _, _, v)| {
            let hit = key.key().name() == "pharos_login_throttled_total"
                && key
                    .key()
                    .labels()
                    .any(|l| l.key() == "reason" && l.value() == reason);
            match v {
                DebugValue::Counter(n) if hit => Some(n),
                _ => None,
            }
        })
        .unwrap_or(0)
}

#[actix_web::test]
async fn repeated_bad_passwords_get_429_with_retry_after_even_for_the_right_one() {
    let rec = DebuggingRecorder::new();
    let _m = metrics::set_default_local_recorder(&rec);
    let st = state(Some((policy(3, 100), &[]))).await;
    let app = app!(st);
    for _ in 0..3 {
        let (s, retry) = login!(app, "ali", "wrong", "198.51.100.1", None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        assert_eq!(retry, None);
    }
    let (s, retry) = login!(app, "ali", "hunter2", "198.51.100.1", None).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    let secs: u64 = retry.expect("Retry-After header").parse().unwrap();
    assert!((1..=300).contains(&secs), "{secs}");
    assert_eq!(throttled(&rec, "user"), 1);
}

#[actix_web::test]
async fn unknown_usernames_count_too_so_account_names_cannot_be_probed_freely() {
    let st = state(Some((policy(3, 100), &[]))).await;
    let app = app!(st);
    for _ in 0..3 {
        let (s, _) = login!(app, "nobody", "x", "198.51.100.1", None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
    }
    let (s, _) = login!(app, "nobody", "x", "198.51.100.1", None).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
}

#[actix_web::test]
async fn a_correct_login_before_the_threshold_succeeds_and_clears_the_count() {
    let st = state(Some((policy(3, 100), &[]))).await;
    let app = app!(st);
    for _ in 0..2 {
        login!(app, "ali", "wrong", "198.51.100.1", None).await;
    }
    assert_eq!(
        login!(app, "ali", "hunter2", "198.51.100.1", None).await.0,
        StatusCode::OK
    );
    // Two more typos would have crossed the threshold without the reset.
    for _ in 0..2 {
        login!(app, "ali", "wrong", "198.51.100.1", None).await;
    }
    assert_eq!(
        login!(app, "ali", "hunter2", "198.51.100.1", None).await.0,
        StatusCode::OK
    );
}

#[actix_web::test]
async fn a_normal_client_with_default_limits_is_never_throttled() {
    let st = state(None).await;
    let app = app!(st);
    for round in 0..5 {
        // An occasional typo, then the right password, as a TV app would.
        if round % 2 == 0 {
            assert_eq!(
                login!(app, "ali", "typo", "192.168.1.20", None).await.0,
                StatusCode::UNAUTHORIZED
            );
        }
        assert_eq!(
            login!(app, "ali", "hunter2", "192.168.1.20", None).await.0,
            StatusCode::OK,
            "round {round}"
        );
    }
}

#[actix_web::test]
async fn one_address_spraying_usernames_is_held_but_other_addresses_are_not() {
    let rec = DebuggingRecorder::new();
    let _m = metrics::set_default_local_recorder(&rec);
    let st = state(Some((policy(100, 3), &[]))).await;
    let app = app!(st);
    for i in 0..3 {
        let (s, _) = login!(app, &format!("guess{i}"), "x", "198.51.100.1", None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
    }
    let (s, retry) = login!(app, "ali", "hunter2", "198.51.100.1", None).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    assert!(retry.is_some());
    assert_eq!(throttled(&rec, "ip"), 1);
    assert_eq!(
        login!(app, "ali", "hunter2", "198.51.100.2", None).await.0,
        StatusCode::OK
    );
}

#[actix_web::test]
async fn logging_into_your_own_account_does_not_reset_the_address_budget() {
    let st = state(Some((policy(100, 3), &[]))).await;
    let app = app!(st);
    for i in 0..2 {
        login!(app, format!("guess{i}"), "x", "198.51.100.1", None).await;
    }
    // A valid login from the same address in between...
    assert_eq!(
        login!(app, "ali", "hunter2", "198.51.100.1", None).await.0,
        StatusCode::OK
    );
    // ...does not give the guesser a fresh budget: one more miss exhausts it.
    login!(app, "guess2", "x", "198.51.100.1", None).await;
    assert_eq!(
        login!(app, "ali", "hunter2", "198.51.100.1", None).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[actix_web::test]
async fn x_forwarded_for_is_ignored_from_an_untrusted_peer() {
    let st = state(Some((policy(100, 3), &["10.0.0.0/8"]))).await;
    let app = app!(st);
    // The attacker rotates a forged XFF; it must not buy a fresh budget.
    for i in 0..3 {
        let xff = format!("203.0.113.{i}");
        login!(app, &format!("g{i}"), "x", "198.51.100.1", Some(&xff)).await;
    }
    let (s, _) = login!(app, "ali", "hunter2", "198.51.100.1", Some("203.0.113.99")).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
}

#[actix_web::test]
async fn x_forwarded_for_from_a_trusted_proxy_separates_clients_behind_it() {
    let st = state(Some((policy(100, 3), &["10.0.0.0/8"]))).await;
    let app = app!(st);
    // Client A, behind the ingress, burns its own per-address budget.
    for i in 0..3 {
        login!(app, &format!("g{i}"), "x", "10.0.0.1", Some("203.0.113.1")).await;
    }
    let (s, _) = login!(app, "ali", "hunter2", "10.0.0.1", Some("203.0.113.1")).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    // Client B behind the same ingress is unaffected.
    let (s, _) = login!(app, "ali", "hunter2", "10.0.0.1", Some("203.0.113.2")).await;
    assert_eq!(s, StatusCode::OK);
}
