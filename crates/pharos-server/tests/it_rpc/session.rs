//! T023: `POST /pharos/session` signs a browser in with an `HttpOnly`
//! cookie the WebSocket upgrade then carries (research R6, FR-003a);
//! `DELETE /pharos/session` revokes it and clears it. Both are browser-only:
//! they need an allowed `Origin`, which a cross-site page cannot forge, and
//! a JSON body, which a cross-site form cannot send.

use actix_web::cookie::SameSite;
use actix_web::dev::ServiceResponse;
use actix_web::{http::StatusCode, test, App};
use pharos_core::TokenStore;
use pharos_server::api::pharos::rpc;
use pharos_server::config::NativeApiConfig;

use crate::common::{seed_rich, Fixture};

const ORIGIN: &str = "https://pharos.test";

async fn call(fx: &Fixture, req: test::TestRequest) -> ServiceResponse {
    let cfg = NativeApiConfig {
        enabled: true,
        allowed_origins: vec![ORIGIN.into()],
        ..NativeApiConfig::default()
    };
    let app = test::init_service(
        App::new()
            .app_data(fx.state.clone())
            .configure(rpc::configure(
                cfg,
                rpc::limits::ConnectionLimits::default(),
            )),
    )
    .await;
    test::call_service(&app, req.to_request()).await
}

fn login(name: &str, password: &str) -> test::TestRequest {
    test::TestRequest::post()
        .uri("/pharos/session")
        .insert_header(("Origin", ORIGIN))
        .set_json(serde_json::json!({"name": name, "password": password}))
}

fn session_cookie(resp: &ServiceResponse) -> Option<actix_web::cookie::Cookie<'static>> {
    resp.response()
        .cookies()
        .find(|c| c.name() == rpc::SESSION_COOKIE)
        .map(|c| c.into_owned())
}

#[actix_web::test]
async fn a_right_password_sets_a_cookie_scripts_cannot_read() {
    let fx = seed_rich().await;
    let resp = call(&fx, login("guest", "hunter2")).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        resp.headers().get("cache-control").unwrap(),
        "no-store",
        "a login response must not be cached"
    );
    let cookie = session_cookie(&resp).expect("pharos_session cookie");
    assert_eq!(cookie.http_only(), Some(true));
    assert_eq!(cookie.secure(), Some(true));
    assert_eq!(cookie.path(), Some("/pharos"));
    assert_eq!(cookie.same_site(), Some(SameSite::Strict));
    // The cookie is an ordinary device token for the user (ADR-0011), so
    // `resume("")` on the upgrade that carries it signs the user in.
    assert_eq!(
        fx.state.stores.resolve(cookie.value()).await.unwrap(),
        fx.user_id
    );
}

#[actix_web::test]
async fn a_wrong_password_or_unknown_user_is_the_same_401_with_no_cookie() {
    let fx = seed_rich().await;
    for (name, password) in [("guest", "nope"), ("nobody", "hunter2")] {
        let resp = call(&fx, login(name, password)).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{name}");
        assert!(session_cookie(&resp).is_none());
    }
}

#[actix_web::test]
async fn without_an_allowed_origin_nothing_is_checked_or_set() {
    let fx = seed_rich().await;
    let no_origin = test::TestRequest::post()
        .uri("/pharos/session")
        .set_json(serde_json::json!({"name": "guest", "password": "hunter2"}));
    let elsewhere = login("guest", "hunter2").insert_header(("Origin", "https://evil.test"));
    for req in [no_origin, elsewhere] {
        let resp = call(&fx, req).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(session_cookie(&resp).is_none());
    }
}

/// A cross-site HTML form can only send form or text bodies; neither is
/// accepted, so it cannot sign the victim in (login CSRF).
#[actix_web::test]
async fn a_form_body_is_refused() {
    let fx = seed_rich().await;
    let req = test::TestRequest::post()
        .uri("/pharos/session")
        .insert_header(("Origin", ORIGIN))
        .insert_header(("Content-Type", "application/x-www-form-urlencoded"))
        .set_payload("name=guest&password=hunter2");
    let resp = call(&fx, req).await;
    assert!(resp.status().is_client_error(), "{}", resp.status());
    assert!(session_cookie(&resp).is_none());
}

/// V174: the cookie login is gated on the same throttle as every other.
#[actix_web::test]
async fn repeated_failures_are_throttled_with_retry_after() {
    let fx = seed_rich().await;
    let mut last = StatusCode::OK;
    for _ in 0..50 {
        let resp = call(&fx, login("guest", "nope")).await;
        last = resp.status();
        if last == StatusCode::TOO_MANY_REQUESTS {
            assert!(resp.headers().contains_key("retry-after"));
            break;
        }
    }
    assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
}

#[actix_web::test]
async fn delete_revokes_the_cookie_clears_it_and_announces_the_user() {
    let fx = seed_rich().await;
    let token = session_cookie(&call(&fx, login("guest", "hunter2")).await)
        .unwrap()
        .value()
        .to_owned();
    let mut announced = fx.state.revocations.subscribe();

    let resp = call(
        &fx,
        test::TestRequest::delete()
            .uri("/pharos/session")
            .insert_header(("Origin", ORIGIN))
            .cookie(actix_web::cookie::Cookie::new(
                rpc::SESSION_COOKIE,
                token.clone(),
            )),
    )
    .await;

    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let cleared = session_cookie(&resp).expect("a clearing Set-Cookie");
    assert_eq!(cleared.value(), "");
    assert_eq!(
        cleared.max_age(),
        Some(actix_web::cookie::time::Duration::ZERO)
    );
    assert_eq!(cleared.path(), Some("/pharos"));
    assert!(fx.state.stores.resolve(&token).await.is_err());
    assert_eq!(announced.try_recv().unwrap(), fx.user_id);
}

#[actix_web::test]
async fn delete_without_a_cookie_still_clears_it() {
    let fx = seed_rich().await;
    let resp = call(
        &fx,
        test::TestRequest::delete()
            .uri("/pharos/session")
            .insert_header(("Origin", ORIGIN)),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert!(session_cookie(&resp).is_some());
}

#[actix_web::test]
async fn delete_from_another_origin_is_refused() {
    let fx = seed_rich().await;
    let token = session_cookie(&call(&fx, login("guest", "hunter2")).await)
        .unwrap()
        .value()
        .to_owned();
    let resp = call(
        &fx,
        test::TestRequest::delete()
            .uri("/pharos/session")
            .insert_header(("Origin", "https://evil.test"))
            .cookie(actix_web::cookie::Cookie::new(
                rpc::SESSION_COOKIE,
                token.clone(),
            )),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(fx.state.stores.resolve(&token).await.is_ok());
}

/// ODD: every sign-in is counted by outcome, so a query shows browsers
/// failing to sign in (`pharos_native_login_total{transport="cookie"}`).
/// Disarm check: without `record_login` in `password_login` and in the origin
/// refusal, every count below is 0.
#[actix_web::test]
async fn each_sign_in_is_counted_by_outcome() {
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    let fx = seed_rich().await;
    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);

    call(&fx, login("guest", "hunter2")).await;
    call(&fx, login("guest", "nope")).await;
    call(
        &fx,
        login("guest", "hunter2").insert_header(("Origin", "https://evil.test")),
    )
    .await;

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
                        .any(|l| l.key() == "transport" && l.value() == "cookie")
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
    assert_eq!(count("origin_refused"), 1);
}

/// The origin is judged before the body: a cross-site request is refused and
/// counted as such even when its body would not parse.
#[actix_web::test]
async fn a_cross_site_request_is_refused_on_its_origin_whatever_its_body() {
    let fx = seed_rich().await;
    let req = test::TestRequest::post()
        .uri("/pharos/session")
        .insert_header(("Origin", "https://evil.test"))
        .insert_header(("Content-Type", "application/json"))
        .set_payload("not json");
    assert_eq!(call(&fx, req).await.status(), StatusCode::FORBIDDEN);
}

/// T023 end to end: the cookie POST sets rides on a browser's upgrade, and
/// `resume("")` turns it into the user's session.
#[actix_web::test]
async fn the_cookie_signs_in_a_browsers_websocket() {
    let fx = seed_rich().await;
    let token = session_cookie(&call(&fx, login("guest", "hunter2")).await)
        .unwrap()
        .value()
        .to_owned();
    let state = fx.state.clone();
    let server = actix_test::start(move || {
        let cfg = NativeApiConfig {
            enabled: true,
            allowed_origins: vec![ORIGIN.into()],
            ..NativeApiConfig::default()
        };
        App::new().app_data(state.clone()).configure(rpc::configure(
            cfg,
            rpc::limits::ConnectionLimits::default(),
        ))
    });
    let (client, _closed) =
        crate::revocation::connect_with(&server, Some(ORIGIN), Some(&token)).await;
    let mut resume = client.resume_request();
    resume.get().init_ctx();
    resume.get().set_device_token("");
    let session = resume.send().pipeline.get_result().get_session();
    assert!(crate::revocation::me_works(&session).await);
}

/// ODD: sign-outs are counted by outcome too (symmetry with sign-in).
#[actix_web::test]
async fn each_sign_out_is_counted_by_outcome() {
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    let fx = seed_rich().await;
    let token = session_cookie(&call(&fx, login("guest", "hunter2")).await)
        .unwrap()
        .value()
        .to_owned();
    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);

    let delete = || test::TestRequest::delete().uri("/pharos/session");
    let with_cookie = |req: test::TestRequest| {
        req.cookie(actix_web::cookie::Cookie::new(
            rpc::SESSION_COOKIE,
            token.clone(),
        ))
    };
    call(
        &fx,
        with_cookie(delete().insert_header(("Origin", "https://evil.test"))),
    )
    .await;
    call(&fx, with_cookie(delete().insert_header(("Origin", ORIGIN)))).await;
    call(&fx, with_cookie(delete().insert_header(("Origin", ORIGIN)))).await;

    let count = |outcome: &str| {
        snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .find(|(k, ..)| {
                k.key().name() == "pharos_native_logout_total"
                    && k.key()
                        .labels()
                        .any(|l| l.key() == "outcome" && l.value() == outcome)
            })
            .map_or(0, |(.., v)| match v {
                DebugValue::Counter(n) => n,
                other => panic!("{other:?}"),
            })
    };
    assert_eq!(count("origin_refused"), 1);
    assert_eq!(count("revoked"), 1);
    assert_eq!(
        count("no_session"),
        1,
        "the second sign-out finds it revoked"
    );
}
