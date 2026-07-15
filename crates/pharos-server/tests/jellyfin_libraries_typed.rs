//! LIB-C1 — typed libraries. /Library/VirtualFolders + /Library/MediaFolders
//! return one entry per configured root with a per-kind CollectionType and a
//! stable wire id; /Items?ParentId=<library wire id> resolves to that
//! library's items via the path-prefix-backfilled library_id; the
//! path-boundary case (/media/movies vs /media/movies-4k) is assigned
//! correctly; and the legacy single-root config path still works.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{
    LibraryKind, LibraryStore, MediaItem, MediaKind, MediaStore, SecretString, TokenStore, UserId,
    UserPolicy, UserRecord, UserStore,
};
use pharos_server::api::jellyfin::items::library_id_for_root;
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

/// Seed two typed libraries (Movies + a Mixed root), an item under each,
/// plus a path-boundary sibling under /media/movies-4k that must not leak
/// into the Movies library.
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

    let movies_root = "/media/movies";
    let other_root = "/media/other";
    let movies_wire = library_id_for_root(std::path::Path::new(movies_root));
    let other_wire = library_id_for_root(std::path::Path::new(other_root));
    stores
        .upsert_library("Movies", movies_root, LibraryKind::Movies, &movies_wire)
        .await
        .unwrap();
    stores
        .upsert_library("Other", other_root, LibraryKind::Mixed, &other_wire)
        .await
        .unwrap();

    let rows: &[(u64, &str)] = &[
        (1, "/media/movies/a.mkv"),
        (2, "/media/other/b.mkv"),
        // path-boundary sibling: string-prefixed by /media/movies but a
        // different directory — must NOT be claimed by the Movies library.
        (3, "/media/movies-4k/c.mkv"),
    ];
    for (id, path) in rows {
        stores
            .put(MediaItem {
                id: *id,
                path: (*path).into(),
                title: format!("item-{id}"),
                kind: MediaKind::Movie,
                ..Default::default()
            })
            .await
            .unwrap();
    }
    stores.backfill_library_ids().await.unwrap();
    let libraries = stores.libraries().await.unwrap();

    let token = stores.issue(uid, "t").await.unwrap();
    let state = web::Data::new(
        AppState::new(stores, "srv".into())
            .with_media_roots(vec![movies_root.into(), other_root.into()])
            .with_libraries(libraries),
    );
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
async fn virtual_folders_returns_two_typed_libraries_with_stable_wire_ids() {
    let (state, token) = seed().await;
    let v = get_json(state, &token, "/Library/VirtualFolders").await;
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    // Name-ordered by the store: Movies, Other.
    let movies = &arr[0];
    assert_eq!(movies["Name"], "Movies");
    assert_eq!(movies["CollectionType"], "movies");
    assert_eq!(
        movies["ItemId"].as_str().unwrap(),
        library_id_for_root(std::path::Path::new("/media/movies"))
    );
    assert_eq!(movies["Locations"][0], "/media/movies");
    let other = &arr[1];
    assert_eq!(other["Name"], "Other");
    assert_eq!(other["CollectionType"], "mixed");
    assert_eq!(other["Locations"][0], "/media/other");
}

#[actix_web::test]
async fn media_folders_emits_typed_collection_type() {
    let (state, token) = seed().await;
    let v = get_json(state, &token, "/Library/MediaFolders").await;
    let items = v["Items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["CollectionType"], "movies");
    // B69 — a mixed library carries a NULL CollectionType ("mixed" is not a
    // valid kotlin BaseItemDto CollectionType enum value).
    assert_eq!(items[1]["CollectionType"], serde_json::Value::Null);
    // The library Id matches the stable wire id.
    assert_eq!(
        items[0]["Id"].as_str().unwrap(),
        library_id_for_root(std::path::Path::new("/media/movies"))
    );
}

#[actix_web::test]
async fn parent_id_library_resolves_only_that_librarys_items() {
    let (state, token) = seed().await;
    let movies = library_id_for_root(std::path::Path::new("/media/movies"));
    let v = get_json(
        state.clone(),
        &token,
        &format!("/Items?ParentId={movies}&Limit=100"),
    )
    .await;
    let titles: Vec<&str> = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap())
        .collect();
    // Only item-1 (under /media/movies). item-3 (/media/movies-4k) must
    // NOT leak in despite the shared string prefix.
    assert_eq!(titles, vec!["item-1"], "path-boundary safe");

    let other = library_id_for_root(std::path::Path::new("/media/other"));
    let v = get_json(state, &token, &format!("/Items?ParentId={other}&Limit=100")).await;
    let titles: Vec<&str> = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap())
        .collect();
    assert_eq!(titles, vec!["item-2"]);
}

#[actix_web::test]
async fn single_root_legacy_config_still_works_without_libraries() {
    // The legacy path: no typed libraries wired, only media_roots → one
    // synthesised mixed library per root (back-compat).
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
    let token = stores.issue(uid, "t").await.unwrap();
    let state = web::Data::new(
        AppState::new(stores, "srv".into()).with_media_roots(vec!["/media/single".into()]),
    );
    let v = get_json(state, token.0.expose(), "/Library/MediaFolders").await;
    let items = v["Items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    // B69 — mixed → null (not the invalid "mixed" enum string).
    assert_eq!(items[0]["CollectionType"], serde_json::Value::Null);
    assert_eq!(
        items[0]["Id"].as_str().unwrap(),
        library_id_for_root(std::path::Path::new("/media/single"))
    );
}
