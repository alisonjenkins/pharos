//! `/Items` SortBy / SortOrder semantics against jellyfin-web 12.2: metadata
//! sort keys, per-key SortOrder, and Jellyfin-style SortName.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaMetadata, MediaStore, SecretString, TokenStore, UserDataStore,
    UserId, UserPolicy, UserRecord, UserStore,
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
    /// `(play_count, last_played_at)` for the test user.
    plays: Option<(u32, i64)>,
    /// `(album, year)` makes the row an audio track on that album.
    album: Option<(&'static str, u32)>,
    rating: Option<&'static str>,
    /// `(series name, series year)` makes the row an episode of that show.
    series: Option<(&'static str, u32)>,
    created: Option<i64>,
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
            plays: None,
            album: None,
            rating: None,
            series: None,
            created: None,
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
                kind: if r.album.is_some() {
                    MediaKind::Audio
                } else if r.series.is_some() {
                    MediaKind::Episode
                } else {
                    MediaKind::Movie
                },
                series: r.series.map(|(name, year)| pharos_core::SeriesInfo {
                    series_name: name.to_string(),
                    season_number: Some(1),
                    episode_number: Some(1),
                    series_folder: Some(format!("/tv/{name}")),
                    series_year: Some(year),
                }),
                probe: pharos_core::MediaProbe {
                    album: r.album.map(|(a, _)| a.to_string()),
                    year: r.album.map(|(_, y)| y),
                    ..Default::default()
                },
                created_at: Some(r.created.unwrap_or(1_000)),
                metadata: MediaMetadata {
                    production_year: r.year,
                    premiere_date: r.premiere,
                    community_rating: r.community,
                    critic_rating: r.critic,
                    official_rating: r.rating.map(str::to_string),
                    ..Default::default()
                },
                ..Default::default()
            })
            .await
            .unwrap();
        if let Some((count, at)) = r.plays {
            stores
                .set_user_data(
                    uid,
                    r.id,
                    pharos_core::UserItemData {
                        played: true,
                        play_count: count,
                        last_played_at: at,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
        }
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

#[actix_web::test]
async fn parental_rating_sorts_by_maturity_with_unknown_lowest() {
    let rated = |id: u64, title: &'static str, rating: &'static str| Row {
        rating: Some(rating),
        ..Row::new(id, title)
    };
    let rows = vec![
        rated(1, "NC17", "NC-17"),
        rated(2, "Pg13", " pg-13 "),
        rated(3, "TvMa", "tv-ma"),
        rated(4, "G", "G"),
        rated(5, "Fifteen", "15"),
        rated(6, "Unrated", "Not Rated"),
        rated(7, "Blank", ""),
        Row::new(8, "None"),
        rated(9, "TvY", "TV-Y"),
        rated(10, "R", "R"),
        // SQL TRIM strips spaces only, so a trailing newline is unknown.
        rated(11, "Newline", "PG-13\n"),
    ];
    let (state, token) = seed(rows).await;
    let app = test::init_service(build_app(state)).await;
    // Ties (unknown/blank/none/newline; G/TV-Y) fall to the id tiebreak.
    let asc = names!(
        &app,
        token.as_str(),
        "SortBy=ParentalRating&SortOrder=Ascending"
    );
    assert_eq!(
        asc,
        vec![
            "Unrated", "Blank", "None", "Newline", "G", "TvY", "Pg13", "Fifteen", "R", "TvMa",
            "NC17"
        ]
    );
    let desc = names!(
        &app,
        token.as_str(),
        "SortBy=ParentalRating&SortOrder=Descending"
    );
    assert_eq!(
        desc,
        vec![
            "NC17", "TvMa", "R", "Fifteen", "Pg13", "G", "TvY", "Unrated", "Blank", "None",
            "Newline"
        ]
    );
}

/// One episode per show; the episode title is irrelevant to series order.
fn show_rows() -> Vec<Row> {
    let ep = |id: u64, show: &'static str, year: u32, created: i64| Row {
        series: Some((show, year)),
        created: Some(created),
        ..Row::new(id, "Pilot")
    };
    vec![
        ep(1, "Bravo", 2001, 10),
        ep(2, "The Delta", 2005, 5),
        ep(3, "Alpha", 1999, 30),
        ep(4, "Charlie", 2001, 20),
        // A later episode of Bravo makes it the most recently added show.
        ep(5, "Bravo", 2001, 40),
    ]
}

#[actix_web::test]
async fn series_tiles_pair_each_sort_key_with_its_own_sort_order() {
    let (state, token) = seed(show_rows()).await;
    let app = test::init_service(build_app(state)).await;
    let base = "IncludeItemTypes=Series&Recursive=true";
    let name_asc = names!(
        &app,
        token.as_str(),
        format!("{base}&SortBy=ProductionYear,SortName&SortOrder=Descending,Ascending")
    );
    assert_eq!(name_asc, vec!["The Delta", "Bravo", "Charlie", "Alpha"]);
    let name_desc = names!(
        &app,
        token.as_str(),
        format!("{base}&SortBy=ProductionYear,SortName&SortOrder=Descending,Descending")
    );
    assert_eq!(name_desc, vec!["The Delta", "Charlie", "Bravo", "Alpha"]);
    let premiere = names!(
        &app,
        token.as_str(),
        format!("{base}&SortBy=PremiereDate,SortName&SortOrder=Ascending")
    );
    assert_eq!(premiere, vec!["Alpha", "Bravo", "Charlie", "The Delta"]);
    // SortName drops the leading article, so "The Delta" sorts under D.
    let by_name = names!(
        &app,
        token.as_str(),
        format!("{base}&SortBy=SortName&SortOrder=Descending")
    );
    assert_eq!(by_name, vec!["The Delta", "Charlie", "Bravo", "Alpha"]);
}

#[actix_web::test]
async fn series_tiles_sort_by_newest_episode_added() {
    let (state, token) = seed(show_rows()).await;
    let app = test::init_service(build_app(state)).await;
    let got = names!(
        &app,
        token.as_str(),
        "IncludeItemTypes=Series&Recursive=true&SortBy=DateCreated&SortOrder=Descending"
    );
    assert_eq!(got, vec!["Bravo", "Alpha", "Charlie", "The Delta"]);
}

fn titled(titles: &[&'static str]) -> Vec<Row> {
    titles
        .iter()
        .enumerate()
        .map(|(i, t)| Row::new(i as u64 + 1, t))
        .collect()
}

#[actix_web::test]
async fn sort_name_ignores_leading_articles_but_name_does_not() {
    let (state, token) = seed(titled(&["Zeta", "The Matrix", "Nemo", "Alpha", "A Zoo"])).await;
    let app = test::init_service(build_app(state)).await;
    let expected = vec!["Alpha", "The Matrix", "Nemo", "Zeta", "A Zoo"];
    let default = names!(&app, token.as_str(), "SortOrder=Ascending");
    assert_eq!(default, expected);
    let sort_name = names!(&app, token.as_str(), "SortBy=SortName");
    assert_eq!(sort_name, expected);
    let name = names!(&app, token.as_str(), "SortBy=Name");
    assert_eq!(name, vec!["A Zoo", "Alpha", "Nemo", "The Matrix", "Zeta"]);
    let desc = names!(&app, token.as_str(), "SortBy=SortName&SortOrder=Descending");
    assert_eq!(desc, vec!["A Zoo", "Zeta", "Nemo", "The Matrix", "Alpha"]);
}

#[actix_web::test]
async fn item_dto_carries_the_sort_name_as_a_plain_string() {
    let (state, token) = seed(titled(&["The Matrix", "Ep 10", ""])).await;
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Items?SortBy=Name")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let sort_name = |name: &str| {
        v["Items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["Name"] == name)
            .unwrap()
            .get("SortName")
            .cloned()
    };
    assert_eq!(
        sort_name("The Matrix"),
        Some(serde_json::json!("matrix")),
        "article dropped, lowercased"
    );
    assert_eq!(
        sort_name("Ep 10"),
        Some(serde_json::json!(pharos_core::sort_name("Ep 10")))
    );
    // jellyfin-web reads SortName[0] and throws on "", so an empty one is absent.
    assert_eq!(sort_name(""), None);
}

#[actix_web::test]
async fn sort_name_orders_numbers_by_value() {
    let (state, token) = seed(titled(&["Ep 10", "Ep 2", "Ep 1", "Ep 100"])).await;
    let app = test::init_service(build_app(state)).await;
    let got = names!(&app, token.as_str(), "SortBy=SortName");
    assert_eq!(got, vec!["Ep 1", "Ep 2", "Ep 10", "Ep 100"]);
}

#[actix_web::test]
async fn sort_name_pages_without_gaps_or_repeats() {
    let (state, token) = seed(titled(&["The B", "A C", "An A", "D", "The E"])).await;
    let app = test::init_service(build_app(state)).await;
    let mut seen = Vec::new();
    for start in [0, 2, 4] {
        seen.extend(names!(
            &app,
            token.as_str(),
            format!("SortBy=SortName&StartIndex={start}&Limit=2")
        ));
    }
    assert_eq!(seen, vec!["An A", "The B", "A C", "D", "The E"]);
}

fn year_tie_rows() -> Vec<Row> {
    vec![
        Row {
            year: Some(2001),
            premiere: Some(10),
            ..Row::new(1, "Bravo")
        },
        Row {
            year: Some(2001),
            premiere: Some(10),
            ..Row::new(2, "Alpha")
        },
        Row {
            year: Some(2010),
            premiere: Some(10),
            ..Row::new(3, "Charlie")
        },
        Row {
            year: Some(1999),
            premiere: Some(5),
            ..Row::new(4, "Delta")
        },
    ]
}

#[actix_web::test]
async fn book_rows_sort_year_descending_then_name_ascending() {
    let (state, token) = seed(year_tie_rows()).await;
    let app = test::init_service(build_app(state)).await;
    let got = names!(
        &app,
        token.as_str(),
        "SortBy=ProductionYear,SortName&SortOrder=Descending,Ascending"
    );
    assert_eq!(got, vec!["Charlie", "Alpha", "Bravo", "Delta"]);
}

#[actix_web::test]
async fn short_sort_order_repeats_its_last_entry() {
    let (state, token) = seed(year_tie_rows()).await;
    let app = test::init_service(build_app(state)).await;
    let got = names!(
        &app,
        token.as_str(),
        "SortBy=ProductionYear,SortName&SortOrder=Descending"
    );
    assert_eq!(got, vec!["Charlie", "Bravo", "Alpha", "Delta"]);
    let none = names!(&app, token.as_str(), "SortBy=ProductionYear,SortName");
    assert_eq!(none, vec!["Delta", "Alpha", "Bravo", "Charlie"]);
}

#[actix_web::test]
async fn three_key_sort_order_pairs_each_key() {
    let (state, token) = seed(year_tie_rows()).await;
    let app = test::init_service(build_app(state)).await;
    let got = names!(
        &app,
        token.as_str(),
        "SortBy=PremiereDate,ProductionYear,SortName&SortOrder=Descending,Descending,Ascending"
    );
    assert_eq!(got, vec!["Charlie", "Alpha", "Bravo", "Delta"]);
}

#[actix_web::test]
async fn album_grid_pairs_year_and_name_with_their_own_sort_order() {
    let track = |id: u64, album: &'static str, year: u32| Row {
        album: Some((album, year)),
        ..Row::new(id, album)
    };
    let rows = vec![
        track(1, "Bravo", 2001),
        track(2, "Alpha", 2001),
        track(3, "Charlie", 2010),
        track(4, "Delta", 1999),
    ];
    let (state, token) = seed(rows).await;
    let app = test::init_service(build_app(state)).await;
    let got = names!(
        &app,
        token.as_str(),
        "IncludeItemTypes=MusicAlbum&Recursive=true&SortBy=ProductionYear,SortName&SortOrder=Descending,Ascending"
    );
    assert_eq!(got, vec!["Charlie", "Alpha", "Bravo", "Delta"]);
}

#[actix_web::test]
async fn album_grid_sorts_by_newest_track_added() {
    let track = |id: u64, album: &'static str, created: i64| Row {
        album: Some((album, 2000)),
        created: Some(created),
        ..Row::new(id, album)
    };
    let rows = vec![
        track(1, "Bravo", 10),
        track(2, "Alpha", 30),
        track(3, "Charlie", 20),
        // A later track makes Bravo the most recently added album.
        track(4, "Bravo", 40),
    ];
    let (state, token) = seed(rows).await;
    let app = test::init_service(build_app(state)).await;
    let base = "IncludeItemTypes=MusicAlbum&Recursive=true";
    let recent = names!(
        &app,
        token.as_str(),
        format!("{base}&SortBy=DateCreated&SortOrder=Descending")
    );
    assert_eq!(recent, vec!["Bravo", "Alpha", "Charlie"]);
    let oldest = names!(
        &app,
        token.as_str(),
        format!("{base}&SortBy=DateCreated&SortOrder=Ascending")
    );
    assert_eq!(oldest, vec!["Charlie", "Alpha", "Bravo"]);
}

#[actix_web::test]
async fn multi_key_sort_pages_consistently() {
    let (state, token) = seed(year_tie_rows()).await;
    let app = test::init_service(build_app(state)).await;
    let q = "SortBy=ProductionYear,SortName&SortOrder=Descending,Ascending";
    let page1 = names!(&app, token.as_str(), format!("{q}&StartIndex=0&Limit=2"));
    let page2 = names!(&app, token.as_str(), format!("{q}&StartIndex=2&Limit=2"));
    assert_eq!(page1, vec!["Charlie", "Alpha"]);
    assert_eq!(page2, vec!["Bravo", "Delta"]);
}

#[actix_web::test]
async fn play_count_and_date_played_sort_by_the_requesting_users_data() {
    let rows = vec![
        Row {
            plays: Some((2, 500)),
            ..Row::new(1, "Bravo")
        },
        Row {
            plays: Some((9, 100)),
            ..Row::new(2, "Alpha")
        },
        Row {
            plays: Some((5, 900)),
            ..Row::new(3, "Charlie")
        },
        Row::new(4, "Delta"),
    ];
    let (state, token) = seed(rows).await;
    let app = test::init_service(build_app(state)).await;
    let by_count = names!(
        &app,
        token.as_str(),
        "SortBy=PlayCount&SortOrder=Descending"
    );
    assert_eq!(by_count, vec!["Alpha", "Charlie", "Bravo", "Delta"]);
    let by_date = names!(
        &app,
        token.as_str(),
        "SortBy=DatePlayed&SortOrder=Descending"
    );
    assert_eq!(by_date, vec!["Charlie", "Bravo", "Alpha", "Delta"]);
}
