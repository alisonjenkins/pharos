//! /Items resolution-class filters: Is4K, IsHd, MinWidth, MaxWidth,
//! Is3D. Items without probe.width drop when any filter is active.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore, UserId, UserPolicy,
    UserRecord, UserStore, Video3DFormat,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

async fn seed_widths() -> (web::Data<AppState>, String) {
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
    let rows: &[(u64, &str, Option<u32>)] = &[
        (1, "SD", Some(640)),
        (2, "HD720", Some(1280)),
        (3, "HD1080", Some(1920)),
        (4, "UHD", Some(3840)),
        (5, "NoData", None),
    ];
    for (id, title, width) in rows {
        stores
            .put(MediaItem {
                id: *id,
                path: format!("/m/{id}.mkv").into(),
                title: (*title).into(),
                kind: MediaKind::Movie,
                probe: MediaProbe {
                    width: *width,
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

async fn names(state: web::Data<AppState>, token: &str, qs: &str) -> Vec<String> {
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri(&format!("/Items?{qs}&Limit=100&SortBy=SortName"))
            .insert_header(("X-Emby-Token", token))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap_or("").to_string())
        .collect()
}

#[actix_web::test]
async fn is_4k_true_returns_uhd_only() {
    let (state, token) = seed_widths().await;
    assert_eq!(names(state, &token, "Is4K=true").await, vec!["UHD"]);
}

#[actix_web::test]
async fn is_hd_true_returns_720_and_1080() {
    let (state, token) = seed_widths().await;
    let n = names(state, &token, "IsHd=true").await;
    assert_eq!(n, vec!["HD720", "HD1080"]);
}

#[actix_web::test]
async fn is_4k_drops_items_without_width() {
    let (state, token) = seed_widths().await;
    let n = names(state, &token, "Is4K=true").await;
    assert!(
        !n.contains(&"NoData".to_string()),
        "NoData must drop; got {n:?}"
    );
}

#[actix_web::test]
async fn min_max_width_bounds_pick_specific_class() {
    let (state, token) = seed_widths().await;
    // 1280..=1920 → HD720 + HD1080 only.
    let n = names(state, &token, "MinWidth=1280&MaxWidth=1920").await;
    assert_eq!(n, vec!["HD720", "HD1080"]);
}

#[actix_web::test]
async fn is_3d_false_returns_everyone_true_returns_nothing() {
    let (state, token) = seed_widths().await;
    let none_3d = names(state.clone(), &token, "Is3D=true").await;
    assert!(none_3d.is_empty(), "expected 0 3D items; got {none_3d:?}");
    let all = names(state, &token, "Is3D=false").await;
    assert_eq!(all.len(), 5);
}

#[actix_web::test]
async fn is_3d_and_packed_frame_resolution_filters_respect_per_eye_dimensions() {
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

    let rows: &[(u64, &str, u32, u32, Option<Video3DFormat>)] = &[
        (10, "Movie 2D 1080p", 1920, 1080, None),
        (
            20,
            "Movie 3D HSBS",
            1920,
            1080,
            Some(Video3DFormat::HalfSideBySide),
        ),
        (
            30,
            "Movie 3D FSBS",
            3840,
            1080,
            Some(Video3DFormat::FullSideBySide),
        ),
        (40, "Movie 2D 4K", 3840, 2160, None),
    ];
    for (id, title, width, height, fmt) in rows {
        stores
            .put(MediaItem {
                id: *id,
                path: format!("/m/{id}.mkv").into(),
                title: (*title).into(),
                kind: MediaKind::Movie,
                probe: MediaProbe {
                    width: Some(*width),
                    height: Some(*height),
                    video_3d_format: *fmt,
                    ..Default::default()
                },
                ..Default::default()
            })
            .await
            .unwrap();
    }
    let token = stores.issue(uid, "t").await.unwrap();
    let state = web::Data::new(AppState::new(stores, "srv".into()));
    let token_str = token.0.expose().to_string();

    let only_3d = names(state.clone(), &token_str, "Is3D=true").await;
    assert_eq!(only_3d, vec!["Movie 3D FSBS", "Movie 3D HSBS"]);

    let only_2d = names(state.clone(), &token_str, "Is3D=false").await;
    assert_eq!(only_2d, vec!["Movie 2D 4K", "Movie 2D 1080p"]);

    let only_4k = names(state.clone(), &token_str, "Is4K=true").await;
    assert_eq!(only_4k, vec!["Movie 2D 4K"]);

    let only_hd = names(state, &token_str, "IsHd=true").await;
    assert_eq!(
        only_hd,
        vec!["Movie 2D 1080p", "Movie 3D FSBS", "Movie 3D HSBS"]
    );
}
