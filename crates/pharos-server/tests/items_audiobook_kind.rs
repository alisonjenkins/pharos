//! `IncludeItemTypes=Audiobook` / `AudioBook`: pharos has no audiobook kind.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaStore, SecretString, TokenStore, UserId, UserPolicy, UserRecord,
    UserStore,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

async fn seed() -> (web::Data<AppState>, String) {
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
    for (id, title, kind) in [
        (1u64, "TheBook", MediaKind::Book),
        (2, "TheTrack", MediaKind::Audio),
        (3, "TheMovie", MediaKind::Movie),
    ] {
        stores
            .put(MediaItem {
                id,
                path: format!("/m/{id}").into(),
                title: title.into(),
                kind,
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

async fn names(state: &web::Data<AppState>, token: &str, qs: &str) -> Vec<String> {
    let app = test::init_service(build_app(state.clone())).await;
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
async fn audiobook_alone_matches_nothing_in_either_spelling() {
    let (state, token) = seed().await;
    for t in ["Audiobook", "AudioBook", "audiobook"] {
        let n = names(&state, &token, &format!("IncludeItemTypes={t}")).await;
        assert!(n.is_empty(), "{t} matched {n:?}");
    }
}

#[actix_web::test]
async fn audiobook_next_to_a_real_kind_leaves_that_kind() {
    let (state, token) = seed().await;
    assert_eq!(
        names(&state, &token, "IncludeItemTypes=Book,AudioBook").await,
        ["TheBook"]
    );
}
