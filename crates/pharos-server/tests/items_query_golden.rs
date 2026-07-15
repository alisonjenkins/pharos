#![allow(clippy::unwrap_used, clippy::expect_used)]
//! LIB-B2 — byte-identical golden parity gate for `/Items` + `/Users/{u}/Items`.
//!
//! A fixed corpus exercises every query shape the in-memory pipeline served
//! (no filter, IncludeItemTypes, each ParentId class, every SortBy,
//! IsFavorite/IsPlayed/IsResumable, pagination, SearchTerm, residual chip
//! filters). Each shape's full JSON body is canonicalised (pretty-printed,
//! key-sorted) and compared against a checked-in golden file.
//!
//! The golden files were captured against the legacy in-memory
//! `list()` + `restrict_to_parent` + `filter_and_sort` path. The B2 rewrite
//! routes these through `MediaStore::query`; the bytes MUST match.
//!
//! Regenerate (only when a wire change is intended + reviewed) with:
//!   PHAROS_GOLDEN_REGEN=1 cargo nextest run -p pharos-server items_query_golden
//!
//! The corpus uses ASCII-only titles / names so the SQL `LOWER` / `LIKE`
//! case-fold matches Rust's `to_lowercase`, keeping the golden bytes stable
//! across the in-memory and SQL engines.

use actix_web::{test, web, App};
use pharos_core::{
    CollectionStore, GenreStore, LibraryKind, LibraryStore, MediaItem, MediaKind, MediaProbe,
    MediaStore, PersonKind, PersonRef, PersonStore, SecretString, SeriesInfo, StudioStore,
    TagStore, TokenStore, UserDataStore, UserId, UserItemData, UserPolicy, UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

const ROOT: &str = "/media/Movies";

/// Build the fixed corpus + entity links. Returns (state, token, user id).
async fn seed() -> (web::Data<AppState>, String, UserId) {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("pw")).unwrap();
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "ali".into(),
            password_hash: hash,
            policy: UserPolicy {
                admin: true,
                ..Default::default()
            },
        })
        .await
        .unwrap();
    let token = stores.issue(uid, "test").await.unwrap();

    // A spread of kinds, titles, durations, widths, subtitles, created_at,
    // genres (probe), series, and music probe fields. ASCII-only.
    struct Row {
        id: u64,
        path: &'static str,
        title: &'static str,
        kind: MediaKind,
        probe: MediaProbe,
        series: Option<SeriesInfo>,
        created_at: Option<i64>,
    }
    fn probe(
        dur: Option<u64>,
        width: Option<u32>,
        subs: usize,
        genre: Option<&str>,
        artist: Option<&str>,
        album: Option<&str>,
    ) -> MediaProbe {
        MediaProbe {
            duration_ms: dur,
            width,
            height: width.map(|w| w * 9 / 16),
            subtitle_tracks: (0..subs).map(|_| Default::default()).collect(),
            genre: genre.map(str::to_string),
            artist: artist.map(str::to_string),
            album: album.map(str::to_string),
            album_artist: artist.map(str::to_string),
            ..Default::default()
        }
    }
    let rows = vec![
        Row {
            id: 10,
            path: "/media/Movies/alpha.mkv",
            title: "Alpha",
            kind: MediaKind::Movie,
            probe: probe(Some(7_200_000), Some(3840), 1, Some("Action"), None, None),
            series: None,
            created_at: Some(1_000),
        },
        Row {
            id: 11,
            path: "/media/Movies/bravo.mkv",
            title: "Bravo",
            kind: MediaKind::Movie,
            probe: probe(Some(5_400_000), Some(1920), 0, Some("Drama"), None, None),
            series: None,
            created_at: Some(3_000),
        },
        Row {
            id: 12,
            path: "/media/Movies/charlie.mkv",
            title: "Charlie",
            kind: MediaKind::Movie,
            probe: probe(Some(6_000_000), Some(1280), 2, Some("Action"), None, None),
            series: None,
            created_at: Some(2_000),
        },
        Row {
            id: 20,
            path: "/media/TV/Delta Show/Season 1/d.s01e01.mkv",
            title: "Delta E1",
            kind: MediaKind::Episode,
            probe: probe(Some(1_500_000), Some(1920), 1, Some("Comedy"), None, None),
            series: Some(SeriesInfo {
                series_name: "Delta Show".into(),
                season_number: Some(1),
                episode_number: Some(1),
                series_folder: Some("/media/TV/Delta Show".into()),
                series_year: Some(2020),
            }),
            created_at: Some(4_000),
        },
        Row {
            id: 21,
            path: "/media/TV/Delta Show/Season 1/d.s01e02.mkv",
            title: "Delta E2",
            kind: MediaKind::Episode,
            probe: probe(Some(1_500_000), Some(1920), 0, Some("Comedy"), None, None),
            series: Some(SeriesInfo {
                series_name: "Delta Show".into(),
                season_number: Some(1),
                episode_number: Some(2),
                series_folder: Some("/media/TV/Delta Show".into()),
                series_year: Some(2020),
            }),
            created_at: Some(5_000),
        },
        Row {
            id: 22,
            path: "/media/TV/Delta Show/Season 2/d.s02e01.mkv",
            title: "Delta S2E1",
            kind: MediaKind::Episode,
            probe: probe(Some(1_500_000), Some(1920), 0, Some("Comedy"), None, None),
            series: Some(SeriesInfo {
                series_name: "Delta Show".into(),
                season_number: Some(2),
                episode_number: Some(1),
                series_folder: Some("/media/TV/Delta Show".into()),
                series_year: Some(2020),
            }),
            created_at: Some(6_000),
        },
        Row {
            id: 30,
            path: "/media/Music/echo.mp3",
            title: "Echo Track",
            kind: MediaKind::Audio,
            probe: probe(
                Some(200_000),
                None,
                0,
                Some("Jazz"),
                Some("Echo Artist"),
                Some("Echo Album"),
            ),
            series: None,
            created_at: Some(7_000),
        },
        Row {
            id: 31,
            path: "/media/Music/foxtrot.mp3",
            title: "Foxtrot Track",
            kind: MediaKind::Audio,
            probe: probe(
                Some(300_000),
                None,
                0,
                Some("Jazz"),
                Some("Echo Artist"),
                Some("Echo Album"),
            ),
            series: None,
            created_at: Some(8_000),
        },
    ];

    for r in &rows {
        stores
            .put(MediaItem {
                id: r.id,
                path: r.path.into(),
                title: r.title.into(),
                kind: r.kind,
                probe: r.probe.clone(),
                series: r.series.clone(),
                created_at: r.created_at,
                metadata: Default::default(),
            })
            .await
            .unwrap();
    }

    // Entity links so the entity-backed ParentId pivots resolve.
    // Genres (entity join) — mirror probe.genre on the movies/episodes.
    stores
        .link_item_genres(10, &["Action".into()])
        .await
        .unwrap();
    stores
        .link_item_genres(12, &["Action".into()])
        .await
        .unwrap();
    stores
        .link_item_genres(11, &["Drama".into()])
        .await
        .unwrap();
    // People.
    stores
        .link_item_people(
            10,
            &[
                PersonRef {
                    name: "Jane Director".into(),
                    role: None,
                    kind: PersonKind::Director,
                    character: None,
                    sort_order: Some(0),
                    ..Default::default()
                },
                PersonRef {
                    name: "Joe Actor".into(),
                    role: None,
                    kind: PersonKind::Actor,
                    character: Some("Hero".into()),
                    sort_order: Some(1),
                    ..Default::default()
                },
            ],
        )
        .await
        .unwrap();
    stores
        .link_item_people(
            11,
            &[PersonRef {
                name: "Joe Actor".into(),
                role: None,
                kind: PersonKind::Actor,
                character: Some("Villain".into()),
                sort_order: Some(0),
                ..Default::default()
            }],
        )
        .await
        .unwrap();
    // Studios.
    stores
        .link_item_studios(10, &["Acme Studios".into()])
        .await
        .unwrap();
    stores
        .link_item_studios(11, &["Acme Studios".into()])
        .await
        .unwrap();
    // Tags.
    stores.link_item_tags(10, &["4k".into()]).await.unwrap();
    stores
        .link_item_tags(12, &["4k".into(), "remux".into()])
        .await
        .unwrap();
    // Collection / box set.
    let coll = stores.create_collection("Saga", &[12, 10]).await.unwrap();
    let _ = coll;
    // Library entity (typed) so ParentId=<library wire id> resolves via the
    // join, not just the path-prefix fallback.
    let lib_wire =
        pharos_server::api::jellyfin::items::library_id_for_root(std::path::Path::new(ROOT));
    stores
        .upsert_library("Movies", ROOT, LibraryKind::Movies, &lib_wire)
        .await
        .unwrap();
    stores.backfill_library_ids().await.unwrap();

    // User data: mark id 11 played, id 10 favourite, id 12 resumable.
    stores
        .set_user_data(
            uid,
            11,
            UserItemData {
                played: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    stores
        .set_user_data(
            uid,
            10,
            UserItemData {
                is_favorite: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    stores
        .set_user_data(
            uid,
            12,
            UserItemData {
                played: false,
                last_played_position_ticks: 500,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let state = web::Data::new(
        AppState::new(stores, "GOLDENSRV".into()).with_media_roots(vec![ROOT.into()]),
    );
    (state, token.0.expose().to_string(), uid)
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

/// Canonicalise a JSON body: parse + pretty-print with sorted keys so the
/// comparison is insensitive to field ordering but sensitive to every value.
fn canonical(body: &[u8]) -> String {
    let v: serde_json::Value = serde_json::from_slice(body).unwrap();
    let sorted = sort_value(v);
    serde_json::to_string_pretty(&sorted).unwrap()
}

fn sort_value(v: serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(map) => {
            let mut bt: std::collections::BTreeMap<String, serde_json::Value> =
                std::collections::BTreeMap::new();
            for (k, val) in map {
                // The server_id is a random per-process UUID, irrelevant to
                // the query semantics under test — pin it so the golden bytes
                // are stable across runs.
                if k == "ServerId" {
                    bt.insert(k, serde_json::Value::String("SERVERID".into()));
                    continue;
                }
                bt.insert(k, sort_value(val));
            }
            serde_json::to_value(bt).unwrap()
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(sort_value).collect())
        }
        other => other,
    }
}

fn golden_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden_items")
}

/// The full matrix of query shapes. Each `(name, uri-suffix)` is fetched on
/// both `/Items` and `/Users/{uid}/Items` (which must agree). The suffix is
/// the query string (leading `?` included when present).
fn shapes() -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = Vec::new();
    let mut add = |name: &str, q: &str| v.push((name.to_string(), q.to_string()));

    add("no_filter", "");
    add("include_movie", "?IncludeItemTypes=Movie");
    add("include_movie_episode", "?IncludeItemTypes=Movie,Episode");
    add("include_audio", "?IncludeItemTypes=Audio");
    add("exclude_episode", "?ExcludeItemTypes=Episode");
    add("media_types_video", "?MediaTypes=Video");
    add("media_types_audio", "?MediaTypes=Audio");
    add("search_delta", "?SearchTerm=Delta");
    add("search_track", "?SearchTerm=Track");
    add("name_starts_with_b", "?NameStartsWith=B");
    add("name_less_than_c", "?NameStartsWith=A&NameLessThan=C");
    add("sort_name_asc", "?SortBy=SortName&SortOrder=Ascending");
    add("sort_name_desc", "?SortBy=SortName&SortOrder=Descending");
    add("sort_datecreated", "?SortBy=DateCreated");
    add(
        "sort_datecreated_asc",
        "?SortBy=DateCreated&SortOrder=Ascending",
    );
    add(
        "sort_runtime_asc",
        "?SortBy=RuntimeTicks&SortOrder=Ascending",
    );
    add(
        "sort_runtime_desc",
        "?SortBy=RuntimeTicks&SortOrder=Descending",
    );
    add("sort_album", "?SortBy=Album&IncludeItemTypes=Audio");
    add(
        "sort_albumartist",
        "?SortBy=AlbumArtist&IncludeItemTypes=Audio",
    );
    add("sort_random_seed", "?SortBy=Random&SortSeed=12345");
    add(
        "sort_random_seed_page",
        "?SortBy=Random&SortSeed=12345&StartIndex=2&Limit=3",
    );
    add("filter_favorite", "?Filters=IsFavorite");
    add("filter_played", "?Filters=IsPlayed");
    add("filter_unplayed", "?Filters=IsUnplayed");
    add("filter_resumable", "?Filters=IsResumable");
    add("is_favorite_bool", "?IsFavorite=true");
    add("is_played_bool", "?IsPlayed=false");
    add("page_1_2", "?StartIndex=1&Limit=2");
    add("page_over", "?StartIndex=50&Limit=10");
    add("genres_filter_action", "?Genres=Action");
    add("tags_filter_4k", "?Tags=4k");
    add("tags_filter_4k_remux", "?Tags=4k,remux");
    add("has_subtitles_true", "?HasSubtitles=true");
    add("has_subtitles_false", "?HasSubtitles=false");
    add("is_4k_true", "?Is4K=true");
    add("is_hd_true", "?IsHd=true");
    add("min_width", "?MinWidth=1920");
    add("max_width", "?MaxWidth=1920");
    add("min_index_2", "?MinIndexNumber=2&IncludeItemTypes=Episode");
    add("max_index_1", "?MaxIndexNumber=1&IncludeItemTypes=Episode");
    add("ids_filter", "?Ids=10,12,99999");
    v
}

/// ParentId shapes need ids discovered at runtime (entity wire ids, synth
/// series/season/artist/album ids). Returned as `(name, full-uri)`.
fn parent_shapes() -> Vec<(String, String)> {
    use pharos_core::{
        collection_wire_id, genre_wire_id, person_wire_id, studio_wire_id, tag_wire_id,
    };
    let lib = pharos_server::api::jellyfin::items::library_id_for_root(std::path::Path::new(ROOT));
    // Series / season / artist / album synth ids via the DTO helpers.
    let series =
        pharos_jellyfin_api::dto::series_id_for_key(Some("/media/TV/Delta Show"), "Delta Show");
    let season =
        pharos_jellyfin_api::dto::season_id_for_key(Some("/media/TV/Delta Show"), "Delta Show", 1);
    let artist = pharos_jellyfin_api::dto::artist_id_for("Echo Artist");
    let album = pharos_jellyfin_api::dto::album_id_for("Echo Album");
    vec![
        (
            "parent_zero",
            "00000000000000000000000000000000".to_string(),
        ),
        ("parent_library", lib),
        ("parent_genre_action", genre_wire_id("Action")),
        ("parent_person_joe", person_wire_id("Joe Actor")),
        ("parent_studio_acme", studio_wire_id("Acme Studios")),
        ("parent_tag_4k", tag_wire_id("4k")),
        ("parent_collection_saga", collection_wire_id("Saga")),
        ("parent_series_delta", series),
        ("parent_season_delta_s1", season),
        ("parent_artist_echo", artist),
        ("parent_album_echo", album),
        (
            "parent_unknown",
            "ffffffffffffffffffffffffffffffff".to_string(),
        ),
    ]
    .into_iter()
    .map(|(n, id)| (n.to_string(), format!("?ParentId={id}")))
    .collect()
}

#[actix_web::test]
async fn items_query_byte_identical_to_golden() {
    let (state, token, uid) = seed().await;
    let uid_s = uid.0.simple().to_string();
    let app = test::init_service(build_app(state)).await;

    let regen = std::env::var("PHAROS_GOLDEN_REGEN").is_ok();
    let dir = golden_dir();
    if regen {
        std::fs::create_dir_all(&dir).unwrap();
    }

    let mut all: Vec<(String, String)> = shapes();
    all.extend(parent_shapes());

    let mut failures: Vec<String> = Vec::new();

    for (name, q) in &all {
        // Both the anonymous /Items and per-user /Users/{uid}/Items routes
        // run the identical pipeline; assert each against its own golden so
        // a regression on either surfaces.
        for (route, uri) in [
            ("items", format!("/Items{q}")),
            ("useritems", format!("/Users/{uid_s}/Items{q}")),
        ] {
            let req = test::TestRequest::get()
                .uri(&uri)
                .insert_header(("X-Emby-Token", token.as_str()))
                .to_request();
            let body = test::call_and_read_body(&app, req).await;
            let got = canonical(&body);
            let file = dir.join(format!("{route}__{name}.json"));
            if regen {
                std::fs::write(&file, got.as_bytes()).unwrap();
            } else {
                let want = std::fs::read_to_string(&file).unwrap_or_else(|_| {
                    panic!("missing golden {file:?}; run with PHAROS_GOLDEN_REGEN=1")
                });
                if want != got {
                    failures.push(format!(
                        "MISMATCH {route}__{name} (uri={uri})\n--- want ---\n{want}\n--- got ---\n{got}"
                    ));
                }
            }
        }
    }

    if regen {
        eprintln!("regenerated {} golden files in {dir:?}", all.len() * 2);
        return;
    }
    assert!(
        failures.is_empty(),
        "{} golden mismatches:\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}
