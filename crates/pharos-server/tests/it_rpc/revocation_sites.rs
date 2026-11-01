//! T025: every path that revokes a token, deletes a user or changes a policy
//! announces the user on `AppState::revocations`, so that user's open native
//! connections re-check at once instead of at the next `revalidate_secs`
//! tick. One test per Jellyfin site; the native sites have their own.
//! Disarm check: with `announce` a no-op, each test's receiver stays empty.

use actix_web::{test, web};
use pharos_core::{TokenStore, UserId};
use pharos_server::state::AppState;
use tokio::sync::broadcast;

use crate::common::{build_app, seed_rich};

/// Every user announced so far, without waiting.
fn announced(rx: &mut broadcast::Receiver<UserId>) -> Vec<UserId> {
    let mut users = Vec::new();
    while let Ok(user) = rx.try_recv() {
        users.push(user);
    }
    users
}

async fn send(
    state: &web::Data<AppState>,
    req: test::TestRequest,
    token: &str,
) -> actix_web::http::StatusCode {
    let app = test::init_service(build_app(state.clone())).await;
    let req = req.insert_header(("X-Emby-Token", token)).to_request();
    test::call_service(&app, req).await.status()
}

#[actix_web::test]
async fn jellyfin_logout_announces_the_user() {
    let fx = seed_rich().await;
    let mut rx = fx.state.revocations.subscribe();
    let status = send(
        &fx.state,
        test::TestRequest::post().uri("/Sessions/Logout"),
        &fx.user_token,
    )
    .await;
    assert!(status.is_success(), "{status}");
    assert_eq!(announced(&mut rx), [fx.user_id]);
}

#[actix_web::test]
async fn deleting_a_device_announces_its_users() {
    let fx = seed_rich().await;
    let mut rx = fx.state.revocations.subscribe();
    // Both seeded users hold a token for device "test".
    let status = send(
        &fx.state,
        test::TestRequest::delete().uri("/Devices?id=test"),
        &fx.admin_token,
    )
    .await;
    assert!(status.is_success(), "{status}");
    let mut users = announced(&mut rx);
    users.sort_by_key(|u| u.0);
    let mut want = vec![fx.admin_id, fx.user_id];
    want.sort_by_key(|u| u.0);
    assert_eq!(users, want);
}

#[actix_web::test]
async fn revoking_an_api_key_announces_its_owner() {
    let fx = seed_rich().await;
    fx.state
        .stores
        .issue(fx.admin_id, "apikey:cli")
        .await
        .unwrap();
    let mut rx = fx.state.revocations.subscribe();
    let status = send(
        &fx.state,
        test::TestRequest::delete().uri("/Auth/Keys/apikey%3Acli"),
        &fx.admin_token,
    )
    .await;
    assert!(status.is_success(), "{status}");
    assert_eq!(announced(&mut rx), [fx.admin_id]);
}

#[actix_web::test]
async fn deleting_a_user_announces_them() {
    let fx = seed_rich().await;
    let mut rx = fx.state.revocations.subscribe();
    let status = send(
        &fx.state,
        test::TestRequest::delete().uri(&format!("/Users/{}", fx.user_id.0.simple())),
        &fx.admin_token,
    )
    .await;
    assert!(status.is_success(), "{status}");
    assert_eq!(announced(&mut rx), [fx.user_id]);
}

#[actix_web::test]
async fn changing_a_policy_announces_the_user() {
    let fx = seed_rich().await;
    let mut rx = fx.state.revocations.subscribe();
    let status = send(
        &fx.state,
        test::TestRequest::post()
            .uri(&format!("/Users/{}/Policy", fx.user_id.0.simple()))
            .set_json(serde_json::json!({"IsDisabled": true})),
        &fx.admin_token,
    )
    .await;
    assert!(status.is_success(), "{status}");
    assert_eq!(announced(&mut rx), [fx.user_id]);
}
