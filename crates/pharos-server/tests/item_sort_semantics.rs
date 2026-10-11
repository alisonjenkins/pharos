//! `/Items` SortBy / SortOrder semantics against jellyfin-web 12.2: metadata
//! sort keys, per-key SortOrder, and Jellyfin-style SortName.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaMetadata, MediaStore, SecretString, TokenStore, UserId, UserPolicy,
    UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

struct Row {
    id: u64,
    title: &'static str,
    year: Option<u32>,
    premiere: Option<i64>,
    community: Option<f32>,
    critic: Option<f32>,
}

impl Row {
    fn new(id: u64, title: &'static str) -> Self {
        Self {
            id,
            title,
            year: None,
            premiere: None,
            community: None,
            critic: None,
        }
    }
}

async fn seed(rows: Vec<Row>) -> (web::Data<AppState>, String) {
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
    for r in rows {
        stores
            .put(MediaItem {
                id: r.id,
                path: format!("/m/{}.mkv", r.id).into(),
                title: r.title.into(),
                kind: MediaKind::Movie,
                created_at: Some(1_000),
                metadata: MediaMetadata {
                    production_year: r.year,
                    premiere_date: r.premiere,
                    community_rating: r.community,
                    critic_rating: r.critic,
                    ..Default::default()
                },
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

/// Item names in response order for `/Items?{query}`.
macro_rules! names {
    ($app:expr, $token:expr, $query:expr) => {{
        let body = test::call_and_read_body(
            $app,
            test::TestRequest::get()
                .uri(&format!("/Items?{}", $query))
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

fn metadata_rows() -> Vec<Row> {
    vec![
        Row {
            year: Some(2001),
            premiere: Some(300),
            community: Some(7.5),
            critic: Some(40.0),
            ..Row::new(1, "Bravo")
        },
        Row {
            year: Some(1999),
            premiere: Some(100),
            community: Some(9.0),
            critic: Some(90.0),
            ..Row::new(2, "Alpha")
        },
        Row {
            year: Some(2010),
            premiere: Some(200),
            community: Some(5.0),
            critic: Some(70.0),
            ..Row::new(3, "Charlie")
        },
        Row::new(4, "Delta"),
    ]
}

#[actix_web::test]
async fn production_year_sorts_by_year_not_name() {
    let (state, token) = seed(metadata_rows()).await;
    let app = test::init_service(build_app(state)).await;
    let asc = names!(
        &app,
        token.as_str(),
        "SortBy=ProductionYear&SortOrder=Ascending"
    );
    assert_eq!(asc, vec!["Delta", "Alpha", "Bravo", "Charlie"]);
    let desc = names!(
        &app,
        token.as_str(),
        "SortBy=ProductionYear&SortOrder=Descending"
    );
    assert_eq!(desc, vec!["Charlie", "Bravo", "Alpha", "Delta"]);
}

#[actix_web::test]
async fn premiere_date_sorts_by_date() {
    let (state, token) = seed(metadata_rows()).await;
    let app = test::init_service(build_app(state)).await;
    let desc = names!(
        &app,
        token.as_str(),
        "SortBy=PremiereDate&SortOrder=Descending"
    );
    assert_eq!(desc, vec!["Bravo", "Charlie", "Alpha", "Delta"]);
}

#[actix_web::test]
async fn community_and_critic_rating_sort_by_score() {
    let (state, token) = seed(metadata_rows()).await;
    let app = test::init_service(build_app(state)).await;
    let community = names!(
        &app,
        token.as_str(),
        "SortBy=CommunityRating&SortOrder=Descending"
    );
    assert_eq!(community, vec!["Alpha", "Bravo", "Charlie", "Delta"]);
    let critic = names!(
        &app,
        token.as_str(),
        "SortBy=CriticRating&SortOrder=Descending"
    );
    assert_eq!(critic, vec!["Alpha", "Charlie", "Bravo", "Delta"]);
}
