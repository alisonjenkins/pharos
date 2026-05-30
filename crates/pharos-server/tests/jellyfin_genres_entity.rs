//! LIB-C4 — genres as real entities. /Genres lists genre rows (name +
//! 32-hex wire id Id) with counts, backfilled from the legacy
//! probe.genre strings; /Items?ParentId=<genre id> resolves through the
//! item_genres indexed join.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore, UserId, UserPolicy,
    UserRecord, UserStore,
};
use pharos_jellyfin_api::dto::genre_id_for;
use pharos_server::{api::jellyfin, auth::BuiltinAuth, middleware::LowercasePath, state::AppState};
use pharos_store_sqlx::sqlite::SqliteStore;

async fn seed() -> (web::Data<AppState>, String) {
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
    // Item 1 carries two genres; item 2 shares "Action".
    let rows: &[(u64, &str, Option<&str>)] = &[
        (1, "A", Some("Action, Sci-Fi")),
        (2, "B", Some("Action")),
        (3, "C", None),
    ];
    for (id, title, genre) in rows {
        stores
            .put(MediaItem {
                id: *id,
                path: format!("/m/{id}.mkv").into(),
                title: (*title).into(),
                kind: MediaKind::Movie,
                probe: MediaProbe {
                    genre: genre.map(str::to_string),
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

async fn get_json(state: web::Data<AppState>, token: &str, uri: &str) -> serde_json::Value {
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri(uri)
            .insert_header(("X-Emby-Token", token))
            .to_request(),
    )
    .await;
    serde_json::from_slice(&body).unwrap()
}

#[actix_web::test]
async fn genres_list_backfills_rows_with_wire_id_and_counts() {
    let (state, token) = seed().await;
    let v = get_json(state, &token, "/Genres").await;
    let items = v["Items"].as_array().unwrap();
    // Two distinct genres: Action (count 2) + Sci-Fi (count 1), name-ordered.
    let names: Vec<&str> = items.iter().map(|i| i["Name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["Action", "Sci-Fi"]);
    let action = &items[0];
    assert_eq!(action["Id"].as_str().unwrap(), genre_id_for("Action"));
    assert_eq!(action["Type"].as_str().unwrap(), "Genre");
    assert_eq!(action["ChildCount"].as_u64().unwrap(), 2);
    assert_eq!(items[1]["ChildCount"].as_u64().unwrap(), 1);
    assert_eq!(v["TotalRecordCount"].as_u64().unwrap(), 2);
}

#[actix_web::test]
async fn parent_id_genre_resolves_to_tagged_items() {
    let (state, token) = seed().await;
    // Backfill happens on /Genres; hit it first so the join is populated.
    let _ = get_json(state.clone(), &token, "/Genres").await;
    // ParentId = Action → items 1 and 2.
    let action = genre_id_for("Action");
    let v = get_json(
        state.clone(),
        &token,
        &format!("/Items?ParentId={action}&Limit=100"),
    )
    .await;
    let mut titles: Vec<&str> = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap())
        .collect();
    titles.sort_unstable();
    assert_eq!(titles, vec!["A", "B"], "both Action items");
    // ParentId = Sci-Fi → only item 1.
    let scifi = genre_id_for("Sci-Fi");
    let v = get_json(state, &token, &format!("/Items?ParentId={scifi}&Limit=100")).await;
    let titles: Vec<&str> = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap())
        .collect();
    assert_eq!(titles, vec!["A"], "only the Sci-Fi item");
}

#[actix_web::test]
async fn parent_id_genre_resolves_before_explicit_backfill_via_legacy_fallback() {
    // Without hitting /Genres first the item_genres join is empty, so the
    // ParentId pivot must still resolve via the legacy in-memory
    // probe.genre fallback.
    let (state, token) = seed().await;
    let action = genre_id_for("Action");
    let v = get_json(
        state,
        &token,
        &format!("/Items?ParentId={action}&Limit=100"),
    )
    .await;
    let mut titles: Vec<&str> = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap())
        .collect();
    titles.sort_unstable();
    assert_eq!(titles, vec!["A", "B"]);
}
