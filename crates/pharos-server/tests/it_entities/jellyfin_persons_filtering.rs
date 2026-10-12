//! `/Persons` query handling: `PersonTypes`, `ParentId`, paging and the
//! name filters jellyfin-web's Books "Authors" tab and A-Z picker send.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{
    LibraryKind, LibraryStore, MediaItem, MediaKind, MediaStore, PersonKind, PersonRef,
    PersonStore, SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore,
};
use pharos_server::api::jellyfin::items::library_id_for_root;
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

const BOOKS_ROOT: &str = "/media/books";
const MOVIES_ROOT: &str = "/media/movies";

fn credit(name: &str, kind: PersonKind) -> PersonRef {
    PersonRef {
        name: name.into(),
        kind,
        ..Default::default()
    }
}

fn books_wire() -> String {
    library_id_for_root(std::path::Path::new(BOOKS_ROOT))
}

/// Books library: book 1 by Ann Author + Zed Writer, book 2 by Ann Author.
/// Movies library: one movie with actors Bob Actor + Carl Actor and writer
/// Mia Screenwriter.
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
    stores
        .upsert_library("Books", BOOKS_ROOT, LibraryKind::Books, &books_wire())
        .await
        .unwrap();
    let movies_wire = library_id_for_root(std::path::Path::new(MOVIES_ROOT));
    stores
        .upsert_library("Movies", MOVIES_ROOT, LibraryKind::Movies, &movies_wire)
        .await
        .unwrap();
    for (id, path, kind) in [
        (1u64, "/media/books/one.epub", MediaKind::Book),
        (2, "/media/books/two.epub", MediaKind::Book),
        (3, "/media/movies/m.mkv", MediaKind::Movie),
    ] {
        stores
            .put(MediaItem {
                id,
                path: path.into(),
                title: format!("item-{id}"),
                kind,
                ..Default::default()
            })
            .await
            .unwrap();
    }
    stores.backfill_library_ids().await.unwrap();
    stores
        .link_item_people(
            1,
            &[
                credit("Ann Author", PersonKind::Writer),
                credit("Zed Writer", PersonKind::Writer),
            ],
        )
        .await
        .unwrap();
    stores
        .link_item_people(2, &[credit("Ann Author", PersonKind::Writer)])
        .await
        .unwrap();
    stores
        .link_item_people(
            3,
            &[
                credit("Bob Actor", PersonKind::Actor),
                credit("Carl Actor", PersonKind::Actor),
                credit("Mia Screenwriter", PersonKind::Writer),
            ],
        )
        .await
        .unwrap();
    let libraries = stores.libraries().await.unwrap();
    let token = stores.issue(uid, "t").await.unwrap();
    let state = web::Data::new(
        AppState::new(stores, "srv".into())
            .with_media_roots(vec![BOOKS_ROOT.into(), MOVIES_ROOT.into()])
            .with_libraries(libraries),
    );
    (state, token.0.expose().to_string())
}

async fn get_json(state: &web::Data<AppState>, token: &str, uri: &str) -> serde_json::Value {
    let app = test::init_service(
        App::new()
            .app_data(state.clone())
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
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

fn names(v: &serde_json::Value) -> Vec<&str> {
    v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap())
        .collect()
}

#[actix_web::test]
async fn no_params_lists_everyone_name_ordered() {
    let (state, token) = seed().await;
    let v = get_json(&state, &token, "/Persons").await;
    assert_eq!(
        names(&v),
        vec![
            "Ann Author",
            "Bob Actor",
            "Carl Actor",
            "Mia Screenwriter",
            "Zed Writer"
        ]
    );
    assert_eq!(v["TotalRecordCount"], 5);
    assert_eq!(v["StartIndex"], 0);
}

#[actix_web::test]
async fn person_types_filters_by_credit_kind_case_insensitively() {
    let (state, token) = seed().await;
    let v = get_json(&state, &token, "/Persons?personTypes=actor").await;
    assert_eq!(names(&v), vec!["Bob Actor", "Carl Actor"]);
    assert_eq!(v["TotalRecordCount"], 2);
    let v = get_json(&state, &token, "/Persons?PersonTypes=Actor,Writer,Author").await;
    assert_eq!(v["TotalRecordCount"], 5);
}

#[actix_web::test]
async fn parent_id_scopes_to_people_credited_under_that_library() {
    let (state, token) = seed().await;
    let uri = format!("/Persons?parentId={}", books_wire());
    let v = get_json(&state, &token, &uri).await;
    assert_eq!(names(&v), vec!["Ann Author", "Zed Writer"]);
    assert_eq!(v["Items"][0]["ChildCount"], 2);
    assert_eq!(v["Items"][1]["ChildCount"], 1);
}

#[actix_web::test]
async fn unknown_parent_id_yields_no_people() {
    let (state, token) = seed().await;
    let v = get_json(
        &state,
        &token,
        "/Persons?parentId=ffffffffffffffffffffffffffffffff",
    )
    .await;
    assert_eq!(v["TotalRecordCount"], 0);
    assert!(v["Items"].as_array().unwrap().is_empty());
}

#[actix_web::test]
async fn paging_reports_total_and_start_index() {
    let (state, token) = seed().await;
    let v = get_json(&state, &token, "/Persons?startIndex=1&limit=2").await;
    assert_eq!(names(&v), vec!["Bob Actor", "Carl Actor"]);
    assert_eq!(v["TotalRecordCount"], 5);
    assert_eq!(v["StartIndex"], 1);
    let v = get_json(&state, &token, "/Persons?startIndex=4&limit=2").await;
    assert_eq!(names(&v), vec!["Zed Writer"]);
}

#[actix_web::test]
async fn name_filters_are_case_insensitive() {
    let (state, token) = seed().await;
    let v = get_json(&state, &token, "/Persons?NameStartsWith=b").await;
    assert_eq!(names(&v), vec!["Bob Actor"]);
    let v = get_json(&state, &token, "/Persons?nameStartsWithOrGreater=M").await;
    assert_eq!(names(&v), vec!["Mia Screenwriter", "Zed Writer"]);
    let v = get_json(&state, &token, "/Persons?nameLessThan=c").await;
    assert_eq!(names(&v), vec!["Ann Author", "Bob Actor"]);
    let v = get_json(&state, &token, "/Persons?searchTerm=SCREEN").await;
    assert_eq!(names(&v), vec!["Mia Screenwriter"]);
}

#[actix_web::test]
async fn person_types_author_matches_writer_credits_on_books() {
    let (state, token) = seed().await;
    let uri = format!("/Persons?personTypes=Author&parentId={}", books_wire());
    let v = get_json(&state, &token, &uri).await;
    assert_eq!(names(&v), vec!["Ann Author", "Zed Writer"]);
    let v = get_json(&state, &token, "/Persons?PersonTypes=Author").await;
    assert_eq!(
        names(&v),
        vec!["Ann Author", "Zed Writer"],
        "a movie's screenwriter is not an Author"
    );
    assert_eq!(v["TotalRecordCount"], 2);
}

#[actix_web::test]
async fn unrecognised_credit_kind_is_emitted_as_unknown() {
    let (state, token) = seed().await;
    {
        use pharos_core::PersonStore;
        state
            .stores
            .link_item_people(3, &[credit("Odd Role", PersonKind::Other)])
            .await
            .unwrap();
    }
    let movie = get_json(&state, &token, "/Items/3").await;
    assert_eq!(movie["People"][0]["Type"], "Unknown");
}

#[actix_web::test]
async fn person_types_writer_keeps_non_book_writers() {
    let (state, token) = seed().await;
    let v = get_json(&state, &token, "/Persons?personTypes=Writer").await;
    assert_eq!(names(&v), vec!["Mia Screenwriter"]);
}

#[actix_web::test]
async fn item_people_type_is_author_on_books_and_writer_elsewhere() {
    let (state, token) = seed().await;
    let book = get_json(&state, &token, "/Items/1").await;
    let book_types: Vec<&str> = book["People"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["Type"].as_str().unwrap())
        .collect();
    assert_eq!(book_types, vec!["Author", "Author"]);
    let movie = get_json(&state, &token, "/Items/3").await;
    let mia = movie["People"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["Name"] == "Mia Screenwriter")
        .unwrap();
    assert_eq!(mia["Type"], "Writer");
}
