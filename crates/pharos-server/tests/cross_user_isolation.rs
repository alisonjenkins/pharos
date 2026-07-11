//! V9 spirit: per-user UserData (favorite, played, resume position)
//! is strictly per-bearer. A handler that read UserData with the
//! wrong user id silently shows user A's state to user B.
//!
//! Why this is a separate audit: every other test layer uses one
//! user. A single-user suite passes even if the handler is
//! hard-coded to read `UserId::nil()`.
//!
//! Strategy:
//!   1. Seed two distinct users (alice, bob) + one media item.
//!   2. Alice marks the item as favorite + played + resumes at 50%.
//!   3. Bob fetches the item via /Items/{id} with HIS bearer.
//!   4. Assert Bob's UserData on the item is all-default — NOT
//!      Alice's state.
//!
//! Also checks the reverse — Alice's bearer still sees Alice's state
//! (positive control so a totally-broken read doesn't pass silently).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaStore, SecretString, TokenStore, UserDataStore, UserId,
    UserItemData, UserPolicy, UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

struct Fixture {
    state: web::Data<AppState>,
    alice_token: String,
    bob_token: String,
    alice_id: UserId,
}

async fn seed_two_users() -> Fixture {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("p")).unwrap();

    let alice = UserId::new();
    let bob = UserId::new();
    stores
        .create(UserRecord {
            id: alice,
            name: "alice".into(),
            password_hash: hash.clone(),
            policy: UserPolicy::default(),
        })
        .await
        .unwrap();
    stores
        .create(UserRecord {
            id: bob,
            name: "bob".into(),
            password_hash: hash,
            policy: UserPolicy::default(),
        })
        .await
        .unwrap();
    stores
        .put(MediaItem {
            id: 1,
            path: "/m/shared.mkv".into(),
            title: "Shared Movie".into(),
            kind: MediaKind::Movie,
            ..Default::default()
        })
        .await
        .unwrap();

    // Alice marks the item: favorite + mid-playback. `played=false`
    // because the Resume invariant is "non-zero position AND NOT
    // played" — fully-played items are not resumable.
    stores
        .set_user_data(
            alice,
            1,
            UserItemData {
                played: false,
                play_count: 1,
                last_played_position_ticks: 300_000_000,
                is_favorite: true,
                last_played_at: 1_700_000_000,
            },
        )
        .await
        .unwrap();

    let alice_token = stores.issue(alice, "alice-device").await.unwrap();
    let bob_token = stores.issue(bob, "bob-device").await.unwrap();
    let state = web::Data::new(AppState::new(stores, "srv".into()));

    Fixture {
        state,
        alice_token: alice_token.0.expose().to_string(),
        bob_token: bob_token.0.expose().to_string(),
        alice_id: alice,
    }
}

fn build_app(
    state: web::Data<AppState>,
) -> App<
    impl actix_web::dev::ServiceFactory<
        actix_web::dev::ServiceRequest,
        Config = (),
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
        InitError = (),
    >,
> {
    App::new()
        .app_data(state)
        .wrap(LowercasePath)
        .configure(jellyfin::configure)
}

/// Pull `UserData` block off a BaseItemDto envelope; returns
/// JSON-`Null` if the field is absent so callers can assert exact shape.
fn user_data_field(v: &serde_json::Value) -> &serde_json::Value {
    v.get("UserData").unwrap_or(&serde_json::Value::Null)
}

#[actix_web::test]
async fn item_endpoint_returns_per_bearer_user_data_not_other_users() {
    let f = seed_two_users().await;
    let app = test::init_service(build_app(f.state.clone())).await;

    // Bob fetches /Items/1 with HIS bearer — must NOT see Alice's
    // favorite/played state.
    let body_bob = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Items/1")
            .insert_header(("X-Emby-Token", f.bob_token.as_str()))
            .to_request(),
    )
    .await;
    let v_bob: serde_json::Value = serde_json::from_slice(&body_bob).unwrap();
    let ud_bob = user_data_field(&v_bob);
    assert_eq!(
        ud_bob.get("IsFavorite").and_then(|v| v.as_bool()),
        Some(false),
        "Bob must not see Alice's favorite flag; got {ud_bob}"
    );
    assert_eq!(
        ud_bob.get("Played").and_then(|v| v.as_bool()),
        Some(false),
        "Bob must not see Alice's played flag; got {ud_bob}"
    );
    assert_eq!(
        ud_bob.get("PlaybackPositionTicks").and_then(|v| v.as_u64()),
        Some(0),
        "Bob must not see Alice's resume position; got {ud_bob}"
    );

    // Positive control — Alice's bearer DOES see Alice's data.
    let body_alice = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Items/1")
            .insert_header(("X-Emby-Token", f.alice_token.as_str()))
            .to_request(),
    )
    .await;
    let v_alice: serde_json::Value = serde_json::from_slice(&body_alice).unwrap();
    let ud_alice = user_data_field(&v_alice);
    assert_eq!(
        ud_alice.get("IsFavorite").and_then(|v| v.as_bool()),
        Some(true),
        "Alice must still see Alice's favorite flag; got {ud_alice}"
    );
    assert_eq!(
        ud_alice
            .get("PlaybackPositionTicks")
            .and_then(|v| v.as_u64()),
        Some(300_000_000),
        "Alice must still see Alice's resume position; got {ud_alice}"
    );
}

#[actix_web::test]
async fn user_items_endpoint_rejects_mismatched_bearer() {
    let f = seed_two_users().await;
    let app = test::init_service(build_app(f.state.clone())).await;

    // Bob's bearer hitting /Users/{alice}/Items/{id} must 403 —
    // the path user must match the bearer (V9 spirit).
    let alice_path_id = f.alice_id.0.simple().to_string();
    let req = test::TestRequest::get()
        .uri(&format!("/Users/{alice_path_id}/Items/1"))
        .insert_header(("X-Emby-Token", f.bob_token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status().as_u16(),
        403,
        "mismatched user_id+bearer combination must be rejected"
    );
}

#[actix_web::test]
async fn resume_endpoint_returns_only_callers_in_progress_items() {
    let f = seed_two_users().await;
    let app = test::init_service(build_app(f.state.clone())).await;

    // Bob's /Users/{bob}/Items/Resume must be empty — only Alice
    // has a resume position. Walk Bob's bearer.
    let bob_path_id = {
        // We don't have bob's UserId outside the fixture; resolve
        // via /Users/Me on Bob's bearer.
        let body = test::call_and_read_body(
            &app,
            test::TestRequest::get()
                .uri("/Users/Me")
                .insert_header(("X-Emby-Token", f.bob_token.as_str()))
                .to_request(),
        )
        .await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        v.get("Id").and_then(|i| i.as_str()).unwrap().to_string()
    };
    let body_bob = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri(&format!("/Users/{bob_path_id}/Items/Resume"))
            .insert_header(("X-Emby-Token", f.bob_token.as_str()))
            .to_request(),
    )
    .await;
    let v_bob: serde_json::Value = serde_json::from_slice(&body_bob).unwrap();
    let items_bob = v_bob["Items"].as_array().expect("Items array");
    assert!(
        items_bob.is_empty(),
        "Bob's Resume list should be empty (only Alice has a resume position); got {v_bob}"
    );

    // Positive control: Alice's Resume DOES list the item.
    let alice_path_id = f.alice_id.0.simple().to_string();
    let body_alice = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri(&format!("/Users/{alice_path_id}/Items/Resume"))
            .insert_header(("X-Emby-Token", f.alice_token.as_str()))
            .to_request(),
    )
    .await;
    let v_alice: serde_json::Value = serde_json::from_slice(&body_alice).unwrap();
    let items_alice = v_alice["Items"].as_array().expect("Items array");
    assert_eq!(
        items_alice.len(),
        1,
        "Alice's Resume list should contain the in-progress item; got {v_alice}"
    );
}
