//! LIB-C2 — people (cast & crew) as real entities. /Persons lists people
//! rows (name + 32-hex wire id Id) with counts; /Persons/{id} resolves a
//! single person; /Items?ParentId=<person id> resolves through the
//! item_people indexed join; and an item's /Items/{id} DTO carries its
//! cast/crew under `People`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{
    person_wire_id, MediaItem, MediaKind, MediaProbe, MediaStore, PersonKind, PersonRef,
    PersonStore, SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore,
};
use pharos_jellyfin_api::dto::person_id_for;
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

fn actor(name: &str, character: &str, order: u32) -> PersonRef {
    PersonRef {
        name: name.into(),
        kind: PersonKind::Actor,
        character: Some(character.into()),
        sort_order: Some(order),
        ..Default::default()
    }
}

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
    // Two items. Item 1 stars Actor Q (Hero A) + directed by Director Q; item 2 also
    // stars Actor Q (Hero B), so Actor Q's count is 2.
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
    let mut director_q = PersonRef {
        name: "Director Q".into(),
        kind: PersonKind::Director,
        ..Default::default()
    };
    director_q.thumb = Some("http://img/director_q.jpg".into());
    stores
        .link_item_people(1, &[actor("Actor Q", "Hero A", 0), director_q])
        .await
        .unwrap();
    stores
        .link_item_people(2, &[actor("Actor Q", "Hero B", 0)])
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

async fn get_status(state: web::Data<AppState>, token: &str, uri: &str) -> u16 {
    let app = test::init_service(build_app(state)).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(uri)
            .insert_header(("X-Emby-Token", token))
            .to_request(),
    )
    .await;
    resp.status().as_u16()
}

#[actix_web::test]
async fn persons_list_emits_rows_with_wire_id_and_counts() {
    let (state, token) = seed().await;
    let v = get_json(state, &token, "/Persons").await;
    let items = v["Items"].as_array().unwrap();
    // Name-ordered: Actor Q, Director Q.
    let names: Vec<&str> = items.iter().map(|i| i["Name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["Actor Q", "Director Q"]);
    let star = &items[0];
    assert_eq!(star["Id"].as_str().unwrap(), person_id_for("Actor Q"));
    assert_eq!(star["Type"].as_str().unwrap(), "Person");
    assert!(star["IsFolder"].as_bool().unwrap());
    assert_eq!(
        star["ChildCount"].as_u64().unwrap(),
        2,
        "Actor Q in both items"
    );
    assert_eq!(items[1]["ChildCount"].as_u64().unwrap(), 1);
    assert_eq!(v["TotalRecordCount"].as_u64().unwrap(), 2);
    // Director Q's recorded headshot advertises a Primary image tag.
    assert!(items[1]["ImageTags"].get("Primary").is_some());
}

#[actix_web::test]
async fn get_person_resolves_by_wire_id_and_404s_unknown() {
    let (state, token) = seed().await;
    let v = get_json(
        state.clone(),
        &token,
        &format!("/Persons/{}", person_id_for("Actor Q")),
    )
    .await;
    assert_eq!(v["Name"].as_str().unwrap(), "Actor Q");
    assert_eq!(v["Type"].as_str().unwrap(), "Person");
    assert_eq!(v["ChildCount"].as_u64().unwrap(), 2);
    // B92 — the Android TV kotlin SDK re-serialises the person wire id DASHED;
    // the store matches wire_id exactly, so a dashed id 404'd. It must resolve
    // identically to the dashless form.
    let dashless = person_id_for("Actor Q");
    let dashed = format!(
        "{}-{}-{}-{}-{}",
        &dashless[0..8],
        &dashless[8..12],
        &dashless[12..16],
        &dashless[16..20],
        &dashless[20..32],
    );
    let v = get_json(state.clone(), &token, &format!("/Persons/{dashed}")).await;
    assert_eq!(
        v["Name"].as_str().unwrap(),
        "Actor Q",
        "dashed person id must resolve"
    );
    assert_eq!(v["ChildCount"].as_u64().unwrap(), 2);
    let status = get_status(state, &token, "/Persons/ffffffffffffffffffffffffffffffff").await;
    assert_eq!(status, 404, "unknown person id is not found");
}

#[actix_web::test]
async fn parent_id_person_resolves_to_credited_items() {
    let (state, token) = seed().await;
    // ParentId = Actor Q → items 1 and 2.
    let star = person_id_for("Actor Q");
    let v = get_json(
        state.clone(),
        &token,
        &format!("/Items?ParentId={star}&Limit=100"),
    )
    .await;
    let mut titles: Vec<&str> = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap())
        .collect();
    titles.sort_unstable();
    assert_eq!(titles, vec!["A", "B"], "both Actor Q items");
    // ParentId = Director Q → only item 1.
    let director_q = person_id_for("Director Q");
    let v = get_json(
        state,
        &token,
        &format!("/Items?ParentId={director_q}&Limit=100"),
    )
    .await;
    let titles: Vec<&str> = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap())
        .collect();
    assert_eq!(titles, vec!["A"], "only the Director-Q-directed item");
}

#[actix_web::test]
async fn item_dto_carries_its_people() {
    let (state, token) = seed().await;
    let v = get_json(state, &token, "/Items/1").await;
    let people = v["People"].as_array().unwrap();
    let names: Vec<&str> = people.iter().map(|p| p["Name"].as_str().unwrap()).collect();
    // NFO order: Actor Q (order 0) then Director Q (crew, no order).
    assert_eq!(names, vec!["Actor Q", "Director Q"]);
    let star = &people[0];
    assert_eq!(star["Id"].as_str().unwrap(), person_id_for("Actor Q"));
    // Role on a cast credit = the character.
    assert_eq!(star["Role"].as_str().unwrap(), "Hero A");
    assert_eq!(star["Type"].as_str().unwrap(), "Actor");
    assert_eq!(people[1]["Type"].as_str().unwrap(), "Director");
}

#[actix_web::test]
async fn wire_id_matches_core_helper() {
    // The DTO helper, the store wire_id, and the ParentId pivot all agree.
    assert_eq!(person_id_for("Actor Q"), person_wire_id("Actor Q"));
}
