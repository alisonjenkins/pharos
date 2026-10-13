//! T052: a playback-scoped (TV) token is accepted only on the routes the stock
//! Chromecast receiver needs, through the real `AuthUser` / `AuthSession`
//! extractors on the real router.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use actix_web::{http::StatusCode, test, web};
use pharos_core::{
    SecretString, TokenScope, TokenStore, UserId, UserPolicy, UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin::auth_extractor::{scope_permits, PatternMatch, PLAYBACK_ALLOWLIST},
    auth::BuiltinAuth,
    cast::{mint_tv_token, DEFAULT_TV_TOKEN_LIFETIME},
    state::{AppState, Stores},
};
use pharos_sync::{GroupRegistry, LocalDelivery, MemberSinks, SessionHub};

struct Seeded {
    state: web::Data<AppState>,
    uid: UserId,
    full: String,
    playback: String,
}

/// An ADMIN owner, so a refusal cannot be mistaken for the handler's own
/// admin check: a full token reaches every route below.
async fn seed() -> Seeded {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "owner".into(),
            password_hash: auth.hash_password(&SecretString::new("p")).unwrap(),
            policy: UserPolicy {
                admin: true,
                ..Default::default()
            },
        })
        .await
        .unwrap();
    let full = stores
        .issue(uid, "web")
        .await
        .unwrap()
        .0
        .expose()
        .to_string();
    let playback = stores
        .issue_with_ttl(uid, "cast:Den", 3600, TokenScope::Playback)
        .await
        .unwrap()
        .secret
        .0
        .expose()
        .to_string();
    Seeded {
        state: web::Data::new(AppState::new(stores, "srv".into())),
        uid,
        full,
        playback,
    }
}

async fn call(
    state: &web::Data<AppState>,
    method: &str,
    uri: &str,
    token: &str,
) -> (StatusCode, String) {
    let member_sinks = MemberSinks::new();
    let app = test::init_service(
        common::build_app(state.clone())
            .app_data(web::Data::new(GroupRegistry::spawn(std::sync::Arc::new(
                LocalDelivery::new(member_sinks.clone()),
            ))))
            .app_data(web::Data::new(SessionHub::new()))
            .app_data(web::Data::new(member_sinks)),
    )
    .await;
    let builder = match method {
        "GET" => test::TestRequest::get(),
        "POST" => test::TestRequest::post(),
        other => panic!("unsupported method {other}"),
    };
    let req = builder
        .uri(uri)
        .insert_header(("X-Emby-Token", token))
        .insert_header(("Content-Type", "application/json"))
        .set_payload(if uri.contains("SyncPlay/Join") {
            r#"{"GroupId":"00000000000000000000000000000000"}"#
        } else {
            "{}"
        })
        .to_request();
    let resp = test::call_service(&app, req).await;
    let status = resp.status();
    let body = test::read_body(resp).await;
    (status, String::from_utf8_lossy(&body).into_owned())
}

fn refused_for_scope(status: StatusCode, body: &str) -> bool {
    status == StatusCode::FORBIDDEN && body.contains("playback token may not call")
}

#[actix_web::test]
async fn a_playback_token_passes_the_extractor_on_the_receivers_routes() {
    let s = seed().await;
    let own_item = format!("/Users/{}/Items/1", s.uid.0.simple());
    let routes: Vec<(&str, String)> = vec![
        ("GET", "/Users/Me".into()),
        ("GET", "/Items/1".into()),
        ("GET", own_item),
        ("POST", "/Items/1/PlaybackInfo".into()),
        ("GET", "/Videos/1/stream".into()),
        ("GET", "/Videos/1/master.m3u8".into()),
        ("GET", "/Videos/1/Subtitles/2/Stream.vtt".into()),
        ("GET", "/Audio/1/stream".into()),
        ("GET", "/Items/1/Images/Primary".into()),
        ("POST", "/Sessions/Capabilities/Full".into()),
        ("POST", "/Sessions/Capabilities".into()),
        ("POST", "/Sessions/Playing".into()),
        ("POST", "/Sessions/Playing/Progress".into()),
        ("POST", "/Sessions/Playing/Stopped".into()),
        ("POST", "/Sessions/Playing/Ping".into()),
        ("GET", "/System/Info/Public".into()),
    ];
    for (method, uri) in &routes {
        let (status, body) = call(&s.state, method, uri, &s.playback).await;
        assert!(
            status != StatusCode::UNAUTHORIZED && !refused_for_scope(status, &body),
            "{method} {uri} must reach the handler for a playback token, got {status}: {body}"
        );
    }
}

#[actix_web::test]
async fn a_playback_token_is_refused_everywhere_else_and_a_full_token_is_not() {
    let s = seed().await;
    let uid = s.uid.0.simple();
    let refused: Vec<(&str, String)> = vec![
        ("POST", format!("/Users/{uid}/Policy")),
        ("GET", "/Users".into()),
        ("POST", "/Library/Refresh".into()),
        ("GET", "/Auth/Keys".into()),
        ("POST", "/Auth/Keys?app=x".into()),
        ("POST", "/QuickConnect/Authorize?code=123456".into()),
        ("POST", "/SyncPlay/New".into()),
        ("POST", "/SyncPlay/Join".into()),
        ("POST", "/Sessions/abc/Playing".into()),
        ("POST", "/Sessions/abc/Command/Stop".into()),
        ("GET", "/Sessions".into()),
        ("POST", "/Items/1/Refresh".into()),
        ("POST", "/Videos/MergeVersions".into()),
    ];
    for (method, uri) in &refused {
        let (status, body) = call(&s.state, method, uri, &s.playback).await;
        assert!(
            refused_for_scope(status, &body),
            "{method} {uri} must be refused to a playback token with the reason, got {status}: {body}"
        );
        let (full_status, full_body) = call(&s.state, method, uri, &s.full).await;
        assert!(
            full_status != StatusCode::UNAUTHORIZED && !refused_for_scope(full_status, &full_body),
            "{method} {uri} must be unchanged for a full token, got {full_status}: {full_body}"
        );
    }
}

#[actix_web::test]
async fn the_refusal_names_the_scope_the_method_and_the_route() {
    let s = seed().await;
    let (status, body) = call(&s.state, "POST", "/Library/Refresh", &s.playback).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        body.contains("playback") && body.contains("POST") && body.contains("/library/refresh"),
        "{body}"
    );
}

/// The refusal is a signal, not just a status: it is counted by scope, and an
/// allowed call counts nothing.
#[actix_web::test]
async fn a_scope_refusal_is_counted_and_an_allowed_call_is_not() {
    use metrics_util::debugging::DebuggingRecorder;

    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _guard = metrics::set_default_local_recorder(&recorder);
    let s = seed().await;
    let counted = || {
        snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(ck, _, _, _)| ck.key().name() == "pharos_token_scope_refused_total")
            .count()
    };

    call(&s.state, "GET", "/Users/Me", &s.playback).await;
    call(&s.state, "POST", "/Library/Refresh", &s.full).await;
    assert_eq!(counted(), 0, "allowed calls count nothing");

    call(&s.state, "POST", "/Library/Refresh", &s.playback).await;
    assert_eq!(counted(), 1, "the refusal must be counted");
}

#[actix_web::test]
async fn a_minted_tv_token_is_playback_scoped() {
    let s = seed().await;
    let tv = mint_tv_token(&s.state.stores, s.uid, "Den", DEFAULT_TV_TOKEN_LIFETIME)
        .await
        .unwrap();
    let token = tv.secret().expose();
    let (status, body) = call(&s.state, "GET", "/Users/Me", token).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = call(&s.state, "POST", "/Library/Refresh", token).await;
    assert!(refused_for_scope(status, &body), "{status}: {body}");
}

#[actix_web::test]
async fn a_legacy_token_with_no_scope_is_full() {
    let s = seed().await;
    let stores = &s.state.stores;
    let legacy = stores.issue(s.uid, "old").await.unwrap();
    let (status, body) = call(&s.state, "GET", "/Users", legacy.0.expose()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[actix_web::test]
async fn unmatched_and_unlisted_routes_are_denied_by_default() {
    assert!(!scope_permits(TokenScope::Playback, "GET", None));
    assert!(!scope_permits(
        TokenScope::Playback,
        "GET",
        Some("/brand/new/route")
    ));
    assert!(!scope_permits(
        TokenScope::Playback,
        "DELETE",
        Some("/items/{id}")
    ));
    assert!(!scope_permits(
        TokenScope::Playback,
        "POST",
        Some("/videos/mergeversions")
    ));
    assert!(scope_permits(
        TokenScope::Playback,
        "GET",
        Some("/items/{id}")
    ));
    assert!(scope_permits(
        TokenScope::Full,
        "DELETE",
        Some("/brand/new/route")
    ));
}

/// Every allowlist entry names a route that is registered, so a renamed route
/// cannot leave a dead entry behind (a dead entry would hide as a receiver
/// that silently stops working).
#[actix_web::test]
async fn every_allowlist_entry_names_a_registered_route() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/api/jellyfin");
    let mut source = String::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "rs") {
            source.push_str(&std::fs::read_to_string(path).unwrap());
        }
    }
    for r in PLAYBACK_ALLOWLIST {
        let needle = match r.pattern {
            PatternMatch::Exact(p) => format!("\"{p}\""),
            PatternMatch::Prefix(p) => format!("\"{p}"),
        };
        assert!(
            source.contains(&needle),
            "allowlist entry {:?} matches no registered route",
            r.pattern
        );
    }
}
