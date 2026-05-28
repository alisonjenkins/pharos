//! `/Items?SortBy=Random` seed stability.
//!
//! Two invariants:
//!   1. Two requests with the same bearer + same query return the
//!      same shuffled order (server derives a per-user seed when
//!      `SortSeed` is absent). Pagination would otherwise mix
//!      duplicates + holes.
//!   2. A client-supplied `SortSeed=N` query param produces a
//!      deterministic order — two requests with the same seed match.
//!   3. Different seeds DO change the order (high-prob — sanity
//!      check the shuffle actually shuffles).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaStore, SecretString, TokenStore, UserId, UserPolicy, UserRecord,
    UserStore,
};
use pharos_server::{api::jellyfin, auth::BuiltinAuth, middleware::LowercasePath, state::AppState};
use pharos_store_sqlx::sqlite::SqliteStore;

async fn seed_n(n: usize) -> (web::Data<AppState>, String) {
    let stores = SqliteStore::connect("sqlite::memory:").await.unwrap();
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
    for i in 1..=n as u64 {
        stores
            .put(MediaItem {
                id: i,
                path: format!("/m/{i:02}.mkv").into(),
                title: format!("It{i:02}"),
                kind: MediaKind::Movie,
                ..Default::default()
            })
            .await
            .unwrap();
    }
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

macro_rules! names_random {
    ($app:expr, $token:expr, $seed:expr) => {{
        let uri: String = match $seed {
            Some(s) => format!("/Items?SortBy=Random&SortSeed={}&Limit=100", s),
            None => "/Items?SortBy=Random&Limit=100".to_string(),
        };
        let body = test::call_and_read_body(
            $app,
            test::TestRequest::get()
                .uri(&uri)
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
async fn random_sort_without_seed_is_stable_per_bearer() {
    let (state, token) = seed_n(12).await;
    let app = test::init_service(build_app(state)).await;
    let a: Vec<String> = names_random!(&app, token.as_str(), Option::<u64>::None);
    let b: Vec<String> = names_random!(&app, token.as_str(), Option::<u64>::None);
    assert_eq!(
        a, b,
        "same bearer must see the same Random order across requests"
    );
    // And not just because shuffle didn't move things — confirm some
    // permutation happened (sorted-by-id would be It01,It02,...).
    let sorted: Vec<String> = (1..=12).map(|i| format!("It{i:02}")).collect();
    assert_ne!(
        a, sorted,
        "shuffle must actually shuffle (high probability)"
    );
}

#[actix_web::test]
async fn explicit_sort_seed_is_deterministic() {
    let (state, token) = seed_n(12).await;
    let app = test::init_service(build_app(state)).await;
    let a: Vec<String> = names_random!(&app, token.as_str(), Some(424242));
    let b: Vec<String> = names_random!(&app, token.as_str(), Some(424242));
    assert_eq!(a, b, "same SortSeed must produce the same order");
}

#[actix_web::test]
async fn different_seeds_produce_different_orders() {
    let (state, token) = seed_n(12).await;
    let app = test::init_service(build_app(state)).await;
    let a: Vec<String> = names_random!(&app, token.as_str(), Some(1));
    let b: Vec<String> = names_random!(&app, token.as_str(), Some(987654321));
    assert_ne!(
        a, b,
        "different SortSeeds should produce different permutations (high probability)"
    );
}

#[actix_web::test]
async fn random_pagination_sweep_with_explicit_seed_has_no_dupes_or_holes() {
    let (state, token) = seed_n(15).await;
    let app = test::init_service(build_app(state)).await;
    // Sweep two 8-item pages with the same SortSeed; reconstruct
    // the full set. No dupes, no holes — that is the practical
    // invariant clients rely on for infinite scroll.
    let page_size = 8u32;
    let seed = 0xdeadbeefu64;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut start = 0u32;
    loop {
        let body = test::call_and_read_body(
            &app,
            test::TestRequest::get()
                .uri(&format!(
                    "/Items?SortBy=Random&SortSeed={seed}&StartIndex={start}&Limit={page_size}"
                ))
                .insert_header(("X-Emby-Token", token.as_str()))
                .to_request(),
        )
        .await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let items = v["Items"].as_array().unwrap();
        if items.is_empty() {
            break;
        }
        for it in items {
            let n = it["Name"].as_str().unwrap().to_string();
            assert!(seen.insert(n.clone()), "duplicate {n} across pages");
        }
        start += page_size;
        assert!(start < 200, "pagination did not terminate");
    }
    assert_eq!(seen.len(), 15, "missed items: got {} of 15", seen.len());
}
