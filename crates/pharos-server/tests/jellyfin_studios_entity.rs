//! LIB-C3 — studios as real entities. /Studios lists studio rows (name +
//! 32-hex wire id Id) with counts; /Items?ParentId=<studio id> resolves
//! through the item_studios indexed join; and an item's /Items/{id} DTO
//! carries its studios under `Studios`. Replaces the old /Studios stub
//! that aggregated album_artist.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{
    studio_wire_id, MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, StudioStore,
    TokenStore, UserId, UserPolicy, UserRecord, UserStore,
};
use pharos_jellyfin_api::dto::studio_id_for;
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
    // Two items. Item 1 is a Warner Bros. + Village Roadshow co-production;
    // item 2 is Warner Bros. only, so Warner Bros.'s count is 2.
    for (id, title) in [(1u64, "A"), (2, "B")] {
        stores
            .put(MediaItem {
                id,
                path: format!("/m/{id}.mkv").into(),
                title: title.into(),
                kind: MediaKind::Movie,
                probe: MediaProbe::default(),
                ..Default::default()
            })
            .await
            .unwrap();
    }
    stores
        .link_item_studios(1, &["Warner Bros.".into(), "Village Roadshow".into()])
        .await
        .unwrap();
    stores
        .link_item_studios(2, &["Warner Bros.".into()])
        .await
        .unwrap();
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
async fn studios_list_emits_rows_with_wire_id_and_counts() {
    let (state, token) = seed().await;
    let v = get_json(state, &token, "/Studios").await;
    let items = v["Items"].as_array().unwrap();
    // Name-ordered: Village Roadshow (count 1), Warner Bros. (count 2).
    let names: Vec<&str> = items.iter().map(|i| i["Name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["Village Roadshow", "Warner Bros."]);
    let vr = &items[0];
    assert_eq!(
        vr["Id"].as_str().unwrap(),
        studio_id_for("Village Roadshow")
    );
    assert_eq!(vr["Type"].as_str().unwrap(), "Studio");
    assert!(vr["IsFolder"].as_bool().unwrap());
    assert_eq!(vr["ChildCount"].as_u64().unwrap(), 1);
    let wb = &items[1];
    assert_eq!(wb["Id"].as_str().unwrap(), studio_id_for("Warner Bros."));
    assert_eq!(
        wb["ChildCount"].as_u64().unwrap(),
        2,
        "Warner Bros. in both items"
    );
    assert_eq!(v["TotalRecordCount"].as_u64().unwrap(), 2);
}

#[actix_web::test]
async fn parent_id_studio_resolves_to_tagged_items() {
    let (state, token) = seed().await;
    // ParentId = Warner Bros. → items 1 and 2.
    let wb = studio_id_for("Warner Bros.");
    let v = get_json(
        state.clone(),
        &token,
        &format!("/Items?ParentId={wb}&Limit=100"),
    )
    .await;
    let mut titles: Vec<&str> = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap())
        .collect();
    titles.sort_unstable();
    assert_eq!(titles, vec!["A", "B"], "both Warner Bros. items");
    // ParentId = Village Roadshow → only item 1.
    let vr = studio_id_for("Village Roadshow");
    let v = get_json(state, &token, &format!("/Items?ParentId={vr}&Limit=100")).await;
    let titles: Vec<&str> = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap())
        .collect();
    assert_eq!(titles, vec!["A"], "only the Village Roadshow item");
}

#[actix_web::test]
async fn item_dto_carries_its_studios() {
    let (state, token) = seed().await;
    let v = get_json(state, &token, "/Items/1").await;
    let studios = v["Studios"].as_array().unwrap();
    // Name-ordered NameGuidPair entries.
    let names: Vec<&str> = studios
        .iter()
        .map(|s| s["Name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["Village Roadshow", "Warner Bros."]);
    assert_eq!(
        studios[1]["Id"].as_str().unwrap(),
        studio_id_for("Warner Bros.")
    );
}

#[actix_web::test]
async fn wire_id_matches_core_helper() {
    // The DTO helper, the store wire_id, and the ParentId pivot all agree.
    assert_eq!(
        studio_id_for("Warner Bros."),
        studio_wire_id("Warner Bros.")
    );
}
