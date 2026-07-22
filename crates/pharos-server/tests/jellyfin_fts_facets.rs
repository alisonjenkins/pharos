//! LIB-B4 + LIB-B5 — `/Search/Hints` backed by the store FTS, and
//! `/Items/Filters` + `/Items/Filters2` backed by `MediaStore::facets`.
//!
//! Asserts: prefix + mid-word search hints (superset of the old substring
//! scan); the SearchHint wire shape is unchanged; and the Filters2 wire
//! shape carries NameGuidPair entity facets + the FacetCounts extension.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{
    GenreStore, MediaItem, MediaKind, MediaMetadata, MediaProbe, MediaStore, SecretString,
    StudioStore, TagStore, TokenStore, UserId, UserPolicy, UserRecord, UserStore,
};
use pharos_jellyfin_api::dto::genre_id_for;
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
    let mk =
        |id: u64, title: &str, overview: Option<&str>, kind: MediaKind, year: u32, rating: &str| {
            MediaItem {
                id,
                path: format!("/m/{id}.mkv").into(),
                title: title.into(),
                kind,
                probe: MediaProbe::default(),
                series: None,
                created_at: Some(1_700_000_000 + id as i64),
                metadata: MediaMetadata {
                    overview: overview.map(str::to_string),
                    production_year: Some(year),
                    official_rating: Some(rating.into()),
                    ..Default::default()
                },
                has_primary_art: false,
            }
        };
    let items = [
        mk(
            1,
            "Pokemon Detective",
            Some("A sleuth and his partner."),
            MediaKind::Movie,
            2019,
            "PG",
        ),
        mk(
            2,
            "The Matrix",
            Some("Hacker learns the truth."),
            MediaKind::Movie,
            1999,
            "R",
        ),
        mk(3, "Common People", None, MediaKind::Episode, 2019, "TV-14"),
    ];
    for i in &items {
        stores.put(i.clone()).await.unwrap();
    }
    stores
        .link_item_genres(1, &["Action".into(), "Mystery".into()])
        .await
        .unwrap();
    stores
        .link_item_genres(2, &["Action".into(), "Sci-Fi".into()])
        .await
        .unwrap();
    stores
        .link_item_genres(3, &["Comedy".into()])
        .await
        .unwrap();
    stores
        .link_item_studios(1, &["Legendary".into()])
        .await
        .unwrap();
    stores
        .link_item_studios(2, &["Warner".into()])
        .await
        .unwrap();
    stores.link_item_tags(1, &["hd".into()]).await.unwrap();
    stores.link_item_tags(2, &["hd".into()]).await.unwrap();

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
async fn search_hints_prefix_token() {
    let (state, token) = seed().await;
    let v = get_json(state, &token, "/Search/Hints?searchTerm=pok").await;
    let hints = v["SearchHints"].as_array().unwrap();
    // The "Pokemon Detective" item hint is present (prefix on "Pokemon").
    let names: Vec<&str> = hints.iter().map(|h| h["Name"].as_str().unwrap()).collect();
    assert!(names.contains(&"Pokemon Detective"), "got {names:?}");
    // SearchHint wire shape unchanged: ItemId + Id + Type + IsFolder.
    let item = hints
        .iter()
        .find(|h| h["Name"] == "Pokemon Detective")
        .unwrap();
    assert_eq!(item["Type"], "Movie");
    assert_eq!(item["IsFolder"], false);
    assert!(item["ItemId"].is_string());
    assert_eq!(item["ItemId"], item["Id"]);
}

#[actix_web::test]
async fn search_hints_mid_word_substring() {
    let (state, token) = seed().await;
    // "kemon" is mid-word inside "Pokemon" — the FTS substring arm must
    // still surface it (superset of the old substring scan).
    let v = get_json(state, &token, "/Search/Hints?searchTerm=kemon").await;
    let names: Vec<&str> = v["SearchHints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["Name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"Pokemon Detective"), "got {names:?}");
}

#[actix_web::test]
async fn search_hints_searches_overview() {
    let (state, token) = seed().await;
    // "hacker" only appears in The Matrix overview, not its title.
    let v = get_json(state, &token, "/Search/Hints?searchTerm=hacker").await;
    let names: Vec<&str> = v["SearchHints"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|h| h["IsFolder"] == false)
        .map(|h| h["Name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"The Matrix"), "got {names:?}");
}

#[actix_web::test]
async fn filters2_emits_nameguid_genres_and_counts() {
    let (state, token) = seed().await;
    let v = get_json(state, &token, "/Items/Filters2").await;
    // Genres are NameGuidPair with the genre wire id.
    let genres = v["Genres"].as_array().unwrap();
    let action = genres
        .iter()
        .find(|g| g["Name"] == "Action")
        .expect("Action genre present");
    assert_eq!(action["Id"], genre_id_for("Action"));

    // Studios are NameGuidPair too.
    let studios = v["Studios"].as_array().unwrap();
    assert!(studios.iter().any(|s| s["Name"] == "Legendary"));

    // FacetCounts extension: Action appears in items 1 + 2 → count 2.
    let gc = v["FacetCounts"]["Genres"].as_array().unwrap();
    let action_c = gc.iter().find(|g| g["Name"] == "Action").unwrap();
    assert_eq!(action_c["Count"], 2);

    // Years descending; 2019 has items 1 + 3.
    let years: Vec<i64> = v["Years"]
        .as_array()
        .unwrap()
        .iter()
        .map(|y| y.as_i64().unwrap())
        .collect();
    assert!(years.contains(&2019) && years.contains(&1999));
    let yc = v["FacetCounts"]["Years"].as_array().unwrap();
    let y2019 = yc.iter().find(|y| y["Name"] == "2019").unwrap();
    assert_eq!(y2019["Count"], 2);
}

#[actix_web::test]
async fn filters2_respects_include_item_types_scope() {
    let (state, token) = seed().await;
    // Movie-only scope drops the Comedy episode (id 3).
    let v = get_json(state, &token, "/Items/Filters2?IncludeItemTypes=Movie").await;
    let genres: Vec<&str> = v["Genres"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["Name"].as_str().unwrap())
        .collect();
    assert!(!genres.contains(&"Comedy"), "got {genres:?}");
    assert!(genres.contains(&"Action"));
}

#[actix_web::test]
async fn filters_legacy_flat_arrays() {
    let (state, token) = seed().await;
    let v = get_json(state, &token, "/Items/Filters").await;
    // Legacy shape: flat string / int arrays, no NameGuidPair, no counts.
    let genres: Vec<&str> = v["Genres"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g.as_str().unwrap())
        .collect();
    assert!(genres.contains(&"Action"));
    let ratings: Vec<&str> = v["OfficialRatings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r.as_str().unwrap())
        .collect();
    assert!(ratings.contains(&"R") && ratings.contains(&"PG"));
}
