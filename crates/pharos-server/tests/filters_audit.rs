//! /Items?Filters=… contract — UserData-driven filters honour the
//! AND-combination across multiple tokens.
//!
//! Recognised: IsFavorite, IsNotFavorite, IsPlayed, IsUnplayed,
//! IsResumable. Unknown tokens are ignored (Jellyfin parity).
//!
//! Why audit beyond unit tests: the handler does an extra bulk
//! UserData lookup ONLY when Filters is active; a regression that
//! drops the lookup would silently return unfiltered items.

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

/// Items seeded:
///   id 1: untouched
///   id 2: favorite, not played, no resume
///   id 3: played, not favorite
///   id 4: not played, resume position set (resumable)
///   id 5: favorite + played
async fn seed_with_user_data() -> (web::Data<AppState>, String) {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("p")).unwrap();
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "u".into(),
            password_hash: hash,
            policy: UserPolicy::default(),
        })
        .await
        .unwrap();
    for i in 1..=5u64 {
        stores
            .put(MediaItem {
                id: i,
                path: format!("/m/it{i}.mkv").into(),
                title: format!("It{i}"),
                kind: MediaKind::Movie,
                ..Default::default()
            })
            .await
            .unwrap();
    }
    let set = |id: u64, ud: UserItemData| {
        let s = stores.clone();
        async move {
            s.set_user_data(uid, id, ud).await.unwrap();
        }
    };
    set(
        2,
        UserItemData {
            is_favorite: true,
            ..Default::default()
        },
    )
    .await;
    set(
        3,
        UserItemData {
            played: true,
            play_count: 1,
            ..Default::default()
        },
    )
    .await;
    set(
        4,
        UserItemData {
            last_played_position_ticks: 300_000_000,
            ..Default::default()
        },
    )
    .await;
    set(
        5,
        UserItemData {
            is_favorite: true,
            played: true,
            play_count: 1,
            ..Default::default()
        },
    )
    .await;
    let token = stores.issue(uid, "t").await.unwrap();
    let state = web::Data::new(AppState::new(stores, "srv".into()));
    (state, token.0.expose().to_string())
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

macro_rules! list_ids {
    ($app:expr, $token:expr, $filters:expr) => {{
        let body = test::call_and_read_body(
            $app,
            test::TestRequest::get()
                .uri(&format!("/Items?Filters={}&Limit=100", $filters))
                .insert_header(("X-Emby-Token", $token))
                .to_request(),
        )
        .await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        v["Items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["Name"].as_str().unwrap_or("").to_string())
            .collect::<Vec<String>>()
    }};
}

#[actix_web::test]
async fn is_favorite_filter_returns_only_favorites() {
    let (state, token) = seed_with_user_data().await;
    let app = test::init_service(build_app(state)).await;
    let names: Vec<String> = list_ids!(&app, token.as_str(), "IsFavorite");
    // It2 + It5 are favorited.
    assert_eq!(
        names.iter().collect::<std::collections::BTreeSet<_>>(),
        ["It2", "It5"]
            .iter()
            .map(|s| s.to_string())
            .collect::<std::collections::BTreeSet<_>>()
            .iter()
            .collect()
    );
}

#[actix_web::test]
async fn is_played_filter_returns_only_played() {
    let (state, token) = seed_with_user_data().await;
    let app = test::init_service(build_app(state)).await;
    let names: Vec<String> = list_ids!(&app, token.as_str(), "IsPlayed");
    assert_eq!(
        names.iter().collect::<std::collections::BTreeSet<_>>(),
        ["It3", "It5"]
            .iter()
            .map(|s| s.to_string())
            .collect::<std::collections::BTreeSet<_>>()
            .iter()
            .collect()
    );
}

#[actix_web::test]
async fn is_unplayed_returns_only_not_played() {
    let (state, token) = seed_with_user_data().await;
    let app = test::init_service(build_app(state)).await;
    let names: Vec<String> = list_ids!(&app, token.as_str(), "IsUnplayed");
    // It1, It2, It4 are unplayed.
    assert_eq!(
        names.iter().collect::<std::collections::BTreeSet<_>>(),
        ["It1", "It2", "It4"]
            .iter()
            .map(|s| s.to_string())
            .collect::<std::collections::BTreeSet<_>>()
            .iter()
            .collect()
    );
}

#[actix_web::test]
async fn is_resumable_returns_items_with_position_and_not_played() {
    let (state, token) = seed_with_user_data().await;
    let app = test::init_service(build_app(state)).await;
    let names: Vec<String> = list_ids!(&app, token.as_str(), "IsResumable");
    // Only It4 has resume position AND is not played.
    assert_eq!(names, vec!["It4"]);
}

#[actix_web::test]
async fn combined_filters_intersect() {
    let (state, token) = seed_with_user_data().await;
    let app = test::init_service(build_app(state)).await;
    let names: Vec<String> = list_ids!(&app, token.as_str(), "IsFavorite,IsPlayed");
    // Both flags ⇒ only It5.
    assert_eq!(names, vec!["It5"]);
}

#[actix_web::test]
async fn ids_query_filters_to_listed_ids_only() {
    let (state, token) = seed_with_user_data().await;
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Items?Ids=2,4&Limit=100")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let names: std::collections::BTreeSet<String> = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap_or("").to_string())
        .collect();
    let want: std::collections::BTreeSet<String> =
        ["It2", "It4"].iter().map(|s| s.to_string()).collect();
    assert_eq!(names, want, "Ids filter should restrict to the listed ids");
}

#[actix_web::test]
async fn ids_query_with_synth_ids_returns_empty() {
    let (state, token) = seed_with_user_data().await;
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            // TRUE synth id (non-zero high half — library/series namespace):
            // must never collide with numeric store ids. NB a zero-high-half
            // 32-hex string is NOT synth any more — since B15 it is the
            // canonical wire form of the numeric item id and must match.
            .uri("/Items?Ids=11112222333344441111222233334444&Limit=100")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        v["Items"].as_array().unwrap().is_empty(),
        "synth-only Ids must not collide with numeric store ids"
    );

    // And the canonical zero-padded form DOES resolve to item 1 (B22).
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Items?Ids=00000000000000000000000000000001&Limit=100")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        v["Items"].as_array().unwrap().len(),
        1,
        "canonical hex form of a real item id must match: {v}"
    );
}

#[actix_web::test]
async fn unknown_filter_tokens_ignored() {
    let (state, token) = seed_with_user_data().await;
    let app = test::init_service(build_app(state)).await;
    // Junk token → no filter applied, all items returned.
    let names: Vec<String> = list_ids!(&app, token.as_str(), "GibberishFilter");
    assert_eq!(names.len(), 5);
}
