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
    // Two items. Item 1 stars Keanu (Neo) + directed by Lana; item 2 also
    // stars Keanu (John Wick), so Keanu's count is 2.
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
    let mut lana = PersonRef {
        name: "Lana Wachowski".into(),
        kind: PersonKind::Director,
        ..Default::default()
    };
    lana.thumb = Some("http://img/lana.jpg".into());
    stores
        .link_item_people(1, &[actor("Keanu Reeves", "Neo", 0), lana])
        .await
        .unwrap();
    stores
        .link_item_people(2, &[actor("Keanu Reeves", "John Wick", 0)])
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
    // Name-ordered: Keanu Reeves, Lana Wachowski.
    let names: Vec<&str> = items.iter().map(|i| i["Name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["Keanu Reeves", "Lana Wachowski"]);
    let keanu = &items[0];
    assert_eq!(keanu["Id"].as_str().unwrap(), person_id_for("Keanu Reeves"));
    assert_eq!(keanu["Type"].as_str().unwrap(), "Person");
    assert!(keanu["IsFolder"].as_bool().unwrap());
    assert_eq!(
        keanu["ChildCount"].as_u64().unwrap(),
        2,
        "Keanu in both items"
    );
    assert_eq!(items[1]["ChildCount"].as_u64().unwrap(), 1);
    assert_eq!(v["TotalRecordCount"].as_u64().unwrap(), 2);
    // Lana's recorded headshot advertises a Primary image tag.
    assert!(items[1]["ImageTags"].get("Primary").is_some());
}

#[actix_web::test]
async fn get_person_resolves_by_wire_id_and_404s_unknown() {
    let (state, token) = seed().await;
    let v = get_json(
        state.clone(),
        &token,
        &format!("/Persons/{}", person_id_for("Keanu Reeves")),
    )
    .await;
    assert_eq!(v["Name"].as_str().unwrap(), "Keanu Reeves");
    assert_eq!(v["Type"].as_str().unwrap(), "Person");
    assert_eq!(v["ChildCount"].as_u64().unwrap(), 2);
    // B92 — the Android TV kotlin SDK re-serialises the person wire id DASHED;
    // the store matches wire_id exactly, so a dashed id 404'd. It must resolve
    // identically to the dashless form.
    let dashless = person_id_for("Keanu Reeves");
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
        "Keanu Reeves",
        "dashed person id must resolve"
    );
    assert_eq!(v["ChildCount"].as_u64().unwrap(), 2);
    let status = get_status(state, &token, "/Persons/ffffffffffffffffffffffffffffffff").await;
    assert_eq!(status, 404, "unknown person id is not found");
}

#[actix_web::test]
async fn parent_id_person_resolves_to_credited_items() {
    let (state, token) = seed().await;
    // ParentId = Keanu → items 1 and 2.
    let keanu = person_id_for("Keanu Reeves");
    let v = get_json(
        state.clone(),
        &token,
        &format!("/Items?ParentId={keanu}&Limit=100"),
    )
    .await;
    let mut titles: Vec<&str> = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap())
        .collect();
    titles.sort_unstable();
    assert_eq!(titles, vec!["A", "B"], "both Keanu items");
    // ParentId = Lana → only item 1.
    let lana = person_id_for("Lana Wachowski");
    let v = get_json(state, &token, &format!("/Items?ParentId={lana}&Limit=100")).await;
    let titles: Vec<&str> = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap())
        .collect();
    assert_eq!(titles, vec!["A"], "only the Lana-directed item");
}

#[actix_web::test]
async fn item_dto_carries_its_people() {
    let (state, token) = seed().await;
    let v = get_json(state, &token, "/Items/1").await;
    let people = v["People"].as_array().unwrap();
    let names: Vec<&str> = people.iter().map(|p| p["Name"].as_str().unwrap()).collect();
    // NFO order: Keanu (order 0) then Lana (crew, no order).
    assert_eq!(names, vec!["Keanu Reeves", "Lana Wachowski"]);
    let keanu = &people[0];
    assert_eq!(keanu["Id"].as_str().unwrap(), person_id_for("Keanu Reeves"));
    // Role on a cast credit = the character.
    assert_eq!(keanu["Role"].as_str().unwrap(), "Neo");
    assert_eq!(keanu["Type"].as_str().unwrap(), "Actor");
    assert_eq!(people[1]["Type"].as_str().unwrap(), "Director");
}

#[actix_web::test]
async fn wire_id_matches_core_helper() {
    // The DTO helper, the store wire_id, and the ParentId pivot all agree.
    assert_eq!(
        person_id_for("Keanu Reeves"),
        person_wire_id("Keanu Reeves")
    );
}
