//! V187 / B244: every by-id route answers an item outside the bearer's
//! `EnabledFolders` as 404, through `services::access`, and each refusal
//! counts under `pharos_item_access_denied_total{reason}`.
//! Disarm check: with `check_item` returning `Ok(())` first, every hidden
//! row below answers non-404 (or 200) and the counter reads 0.

use actix_web::{http::Method, test};
use metrics_util::debugging::{DebugValue, DebuggingRecorder};
use pharos_core::{UserPolicy, UserStore as _};

use crate::common::{build_app, seed_rich, Fixture};

fn denied_total(snapshotter: &pharos_test_metrics::CumulativeSnapshotter, reason: &str) -> u64 {
    snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .filter(|(k, ..)| {
            let key = k.key();
            key.name() == "pharos_item_access_denied_total"
                && key
                    .labels()
                    .any(|l| l.key() == "reason" && l.value() == reason)
        })
        .map(|(.., v)| match v {
            DebugValue::Counter(n) => n,
            other => panic!("{other:?}"),
        })
        .sum()
}

/// One request per route class that takes an item id.
fn routes(f: &Fixture, id: u64) -> Vec<(Method, String)> {
    let uid = f.user_id.0.simple();
    vec![
        (Method::GET, format!("/Items/{id}")),
        (Method::GET, format!("/Users/{uid}/Items/{id}")),
        (Method::GET, format!("/Items/{id}/Ancestors")),
        (Method::GET, format!("/Items/{id}/Similar")),
        (Method::GET, format!("/Items/{id}/Collections")),
        (Method::GET, format!("/Items/{id}/PlaybackInfo")),
        (Method::GET, format!("/Items/{id}/Download")),
        (Method::GET, format!("/Items/{id}/InstantMix")),
        (Method::GET, format!("/Items/{id}/Waveform")),
        (Method::GET, format!("/Audio/{id}/Lyrics")),
        (Method::GET, format!("/MediaSegments/{id}")),
        (Method::GET, format!("/Videos/{id}/stream")),
        (Method::GET, format!("/Audio/{id}/universal")),
        (Method::GET, format!("/Videos/{id}/master.m3u8")),
        (Method::GET, format!("/Videos/{id}/hls1/main/0.ts")),
        (Method::GET, format!("/Videos/{id}/Trickplay/320/0.jpg")),
        (Method::POST, format!("/UserFavoriteItems/{id}")),
        (Method::POST, format!("/Users/{uid}/PlayedItems/{id}")),
    ]
}

async fn restrict_to_library_a(f: &Fixture) {
    let policy = UserPolicy {
        enable_all_folders: false,
        enabled_folders: vec![f.lib_a_wire.clone()],
        ..UserPolicy::default()
    };
    f.state.stores.set_policy(f.user_id, policy).await.unwrap();
}

#[actix_web::test]
async fn a_hidden_item_is_not_found_on_every_by_id_route() {
    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);

    let f = seed_rich().await;
    restrict_to_library_a(&f).await;
    let app = test::init_service(build_app(f.state.clone())).await;

    let mut failures = Vec::new();
    for (method, uri) in routes(&f, f.other_item_id) {
        let before = denied_total(&snapshotter, "library_not_allowed");
        let req = test::TestRequest::default()
            .method(method.clone())
            .uri(&uri)
            .insert_header(("X-Emby-Token", f.user_token.as_str()))
            .to_request();
        let status = test::call_service(&app, req).await.status().as_u16();
        let counted = denied_total(&snapshotter, "library_not_allowed") - before;
        if status != 404 || counted != 1 {
            failures.push(format!("{method} {uri}: {status}, counted {counted}"));
        }
    }
    assert!(
        failures.is_empty(),
        "hidden item not refused by the access check:\n{}",
        failures.join("\n")
    );
}

/// The same routes still reach an item in an allowed library: the check
/// withholds only what the policy hides.
#[actix_web::test]
async fn an_allowed_item_is_still_served() {
    let f = seed_rich().await;
    restrict_to_library_a(&f).await;
    let app = test::init_service(build_app(f.state.clone())).await;
    for uri in [
        format!("/Items/{}", f.rich_item_id),
        format!("/Items/{}/Ancestors", f.rich_item_id),
        format!("/Items/{}/PlaybackInfo", f.rich_item_id),
    ] {
        let req = test::TestRequest::get()
            .uri(&uri)
            .insert_header(("X-Emby-Token", f.user_token.as_str()))
            .to_request();
        let status = test::call_service(&app, req).await.status().as_u16();
        assert_eq!(status, 200, "{uri}");
    }
}

/// Without a token the stream route answers 401, not 404: auth comes first.
#[actix_web::test]
async fn a_stream_without_credentials_is_still_unauthorized() {
    let f = seed_rich().await;
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::get()
        .uri(&format!("/Videos/{}/stream", f.other_item_id))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 401);
}

/// A playlist built before the restriction lists only the items its owner
/// may still reach.
#[actix_web::test]
async fn a_playlist_lists_only_reachable_items() {
    let f = seed_rich().await;
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::post()
        .uri(&format!(
            "/Playlists?Name=Mix&Ids={},{}&MediaType=Video",
            f.rich_item_id, f.other_item_id
        ))
        .insert_header(("X-Emby-Token", f.user_token.as_str()))
        .to_request();
    let created: serde_json::Value = test::call_and_read_body_json(&app, req).await;
    let pid = created["Id"].as_str().unwrap().to_owned();

    restrict_to_library_a(&f).await;
    let req = test::TestRequest::get()
        .uri(&format!("/Playlists/{pid}/Items"))
        .insert_header(("X-Emby-Token", f.user_token.as_str()))
        .to_request();
    let page: serde_json::Value = test::call_and_read_body_json(&app, req).await;
    let ids: Vec<&str> = page["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 1, "{ids:?}");
    assert_eq!(page["TotalRecordCount"], 1);
}

fn get(token: &str, uri: &str) -> test::TestRequest {
    test::TestRequest::get()
        .uri(uri)
        .insert_header(("X-Emby-Token", token))
}

fn wire(id: u64) -> String {
    format!("{id:032x}")
}

fn item_ids(page: &serde_json::Value) -> Vec<String> {
    page["Items"]
        .as_array()
        .unwrap_or_else(|| panic!("no Items: {page}"))
        .iter()
        .map(|i| i["Id"].as_str().unwrap().to_owned())
        .collect()
}

/// Lists assembled outside a `MediaQuery` leave out what the policy hides:
/// the resume row and an allowed item's similar items.
#[actix_web::test]
async fn resume_and_similar_leave_out_hidden_items() {
    use pharos_core::{UserDataStore as _, UserItemData};
    let f = seed_rich().await;
    for id in [f.rich_item_id, f.other_item_id] {
        let data = UserItemData {
            last_played_position_ticks: 10_000_000,
            last_played_at: 1,
            ..UserItemData::default()
        };
        f.state
            .stores
            .set_user_data(f.user_id, id, data)
            .await
            .unwrap();
    }
    restrict_to_library_a(&f).await;
    let app = test::init_service(build_app(f.state.clone())).await;
    let hidden = wire(f.other_item_id);

    let resume: serde_json::Value =
        test::call_and_read_body_json(&app, get(&f.user_token, "/UserItems/Resume").to_request())
            .await;
    let ids = item_ids(&resume);
    assert_eq!(ids, vec![wire(f.rich_item_id)], "{resume}");
    assert_eq!(resume["TotalRecordCount"], 1);

    let similar: serde_json::Value = test::call_and_read_body_json(
        &app,
        get(&f.user_token, &format!("/Items/{}/Similar", f.rich_item_id)).to_request(),
    )
    .await;
    assert!(!item_ids(&similar).contains(&hidden), "{similar}");
}

/// A show with episodes in two libraries lists, to a user allowed one of
/// them, only that library's episodes and the seasons they make up.
#[actix_web::test]
async fn a_series_lists_only_reachable_episodes_and_seasons() {
    use pharos_core::{LibraryStore as _, MediaItem, MediaKind, MediaStore as _, SeriesInfo};
    let f = seed_rich().await;
    let episode = |id: u64, lib: &str, season: u32| MediaItem {
        id,
        path: format!("/{lib}/Show/S{season:02}E01.mkv").into(),
        title: format!("Episode {id}"),
        kind: MediaKind::Episode,
        series: Some(SeriesInfo {
            series_name: "Show".into(),
            season_number: Some(season),
            episode_number: Some(1),
            ..SeriesInfo::default()
        }),
        ..MediaItem::default()
    };
    f.state.stores.put(episode(301, "libA", 1)).await.unwrap();
    f.state.stores.put(episode(302, "libB", 2)).await.unwrap();
    f.state.stores.backfill_library_ids().await.unwrap();
    restrict_to_library_a(&f).await;
    let app = test::init_service(build_app(f.state.clone())).await;
    let series = pharos_jellyfin_api::dto::series_id_for_key(None, "Show");

    let episodes: serde_json::Value = test::call_and_read_body_json(
        &app,
        get(&f.user_token, &format!("/Shows/{series}/Episodes")).to_request(),
    )
    .await;
    assert_eq!(item_ids(&episodes), vec![wire(301)], "{episodes}");

    let seasons: serde_json::Value = test::call_and_read_body_json(
        &app,
        get(&f.user_token, &format!("/Shows/{series}/Seasons")).to_request(),
    )
    .await;
    let numbers: Vec<_> = seasons["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["IndexNumber"].clone())
        .collect();
    assert_eq!(numbers, vec![serde_json::json!(1)], "{seasons}");
}

/// Adding a hidden item to a playlist is refused like any other by-id use.
#[actix_web::test]
async fn adding_a_hidden_item_to_a_playlist_is_not_found() {
    let f = seed_rich().await;
    let app = test::init_service(build_app(f.state.clone())).await;
    let req = test::TestRequest::post()
        .uri(&format!(
            "/Playlists?Name=Mix&Ids={}&MediaType=Video",
            f.rich_item_id
        ))
        .insert_header(("X-Emby-Token", f.user_token.as_str()))
        .to_request();
    let created: serde_json::Value = test::call_and_read_body_json(&app, req).await;
    let pid = created["Id"].as_str().unwrap().to_owned();

    restrict_to_library_a(&f).await;
    let req = test::TestRequest::post()
        .uri(&format!("/Playlists/{pid}/Items?Ids={}", f.other_item_id))
        .insert_header(("X-Emby-Token", f.user_token.as_str()))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 404);
    let page: serde_json::Value = test::call_and_read_body_json(
        &app,
        get(&f.user_token, &format!("/Playlists/{pid}/Items")).to_request(),
    )
    .await;
    assert_eq!(item_ids(&page), vec![wire(f.rich_item_id)], "{page}");
}

/// Lists and aggregates built straight from the item cache (latest, next
/// up, artists, albums, music genres, counts, the album shortcut of
/// `/Items`) leave out items in libraries the user may not reach.
#[actix_web::test]
async fn cache_built_lists_leave_out_hidden_libraries() {
    use pharos_core::{
        LibraryStore as _, MediaItem, MediaKind, MediaProbe, MediaStore as _, SeriesInfo,
    };
    let f = seed_rich().await;
    f.state
        .stores
        .put(MediaItem {
            id: 302,
            path: "/libB/Hidden Show/S01E01.mkv".into(),
            title: "Hidden Episode".into(),
            kind: MediaKind::Episode,
            series: Some(SeriesInfo {
                series_name: "Hidden Show".into(),
                season_number: Some(1),
                episode_number: Some(1),
                ..SeriesInfo::default()
            }),
            ..MediaItem::default()
        })
        .await
        .unwrap();
    f.state
        .stores
        .put(MediaItem {
            id: 303,
            path: "/libB/hidden-track.flac".into(),
            title: "Hidden Track".into(),
            kind: MediaKind::Audio,
            probe: MediaProbe {
                artist: Some("Hidden Artist".into()),
                album_artist: Some("Hidden Artist".into()),
                album: Some("Hidden Album".into()),
                genre: Some("Hiddenwave".into()),
                ..MediaProbe::default()
            },
            ..MediaItem::default()
        })
        .await
        .unwrap();
    f.state.stores.backfill_library_ids().await.unwrap();
    restrict_to_library_a(&f).await;
    let app = test::init_service(build_app(f.state.clone())).await;

    let mut leaks = Vec::new();
    for uri in [
        "/Items/Latest",
        "/Shows/NextUp",
        "/Artists",
        "/Albums",
        "/MusicGenres",
        "/Items?IncludeItemTypes=MusicAlbum&Recursive=true",
    ] {
        let resp = test::call_service(&app, get(&f.user_token, uri).to_request()).await;
        assert_eq!(resp.status().as_u16(), 200, "{uri}");
        let body = String::from_utf8(test::read_body(resp).await.to_vec()).unwrap();
        for hidden in [
            "Other Movie",
            "Hidden Episode",
            "Hidden Show",
            "Hidden Artist",
            "Hidden Album",
            "Hiddenwave",
        ] {
            if body.contains(hidden) {
                leaks.push(format!("{uri} lists {hidden}"));
            }
        }
    }
    let counts: serde_json::Value =
        test::call_and_read_body_json(&app, get(&f.user_token, "/Items/Counts").to_request()).await;
    for (field, want) in [
        ("MovieCount", 1),
        ("EpisodeCount", 0),
        ("SeriesCount", 0),
        ("SongCount", 0),
        ("AlbumCount", 0),
        ("ArtistCount", 0),
    ] {
        if counts[field] != want {
            leaks.push(format!("/Items/Counts {field} = {}", counts[field]));
        }
    }
    assert!(
        leaks.is_empty(),
        "hidden items listed:\n{}",
        leaks.join("\n")
    );
}

/// Routes that serve an item's files to element URLs (images, subtitles,
/// fonts). V189: without a credential each answers 401 and counts
/// `unauthenticated`; with one, a hidden item is 404 like any by-id route.
fn file_routes(id: u64) -> Vec<String> {
    vec![
        format!("/Items/{id}/Images"),
        format!("/Items/{id}/Images/Primary"),
        format!("/Items/{id}/Images/Backdrop/0"),
        format!("/Items/{id}/Images/Chapter/0"),
        format!("/Videos/{id}/Subtitles/0/Stream.vtt"),
        format!("/Videos/{id}/{id}/Subtitles/0/Stream.js"),
        format!("/Videos/{id}/Subtitles/0/Subtitles.srt"),
        format!("/Videos/{id}/Subtitles/0/Stream.ass"),
        format!("/Videos/{id}/Attachments/0"),
    ]
}

#[actix_web::test]
async fn file_routes_refuse_anonymous_and_hidden_requests() {
    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);

    let f = seed_rich().await;
    restrict_to_library_a(&f).await;
    let app = test::init_service(build_app(f.state.clone())).await;

    let mut failures = Vec::new();
    for uri in file_routes(f.rich_item_id) {
        let before = denied_total(&snapshotter, "unauthenticated");
        let req = test::TestRequest::get().uri(&uri).to_request();
        let status = test::call_service(&app, req).await.status().as_u16();
        let counted = denied_total(&snapshotter, "unauthenticated") - before;
        if status != 401 || counted != 1 {
            failures.push(format!("anonymous {uri}: {status}, counted {counted}"));
        }
    }
    for uri in file_routes(f.other_item_id) {
        let before = denied_total(&snapshotter, "library_not_allowed");
        let req = test::TestRequest::get()
            .uri(&format!("{uri}?api_key={}", f.user_token))
            .to_request();
        let status = test::call_service(&app, req).await.status().as_u16();
        let counted = denied_total(&snapshotter, "library_not_allowed") - before;
        if status != 404 || counted != 1 {
            failures.push(format!("hidden {uri}: {status}, counted {counted}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// B258: `/Search/Hints` answers only what the bearer may see. A user
/// restricted to library B searching for library A's title, studio, tag or
/// cast gets no hint for any of them, and still finds library B's item.
/// Disarm check: with `search_hints` ignoring the policy again, every
/// hidden term below returns a hint.
#[actix_web::test]
async fn search_hints_answer_only_visible_items() {
    let f = seed_rich().await;
    let policy = UserPolicy {
        enable_all_folders: false,
        enabled_folders: vec![f.lib_b_wire.clone()],
        ..UserPolicy::default()
    };
    f.state.stores.set_policy(f.user_id, policy).await.unwrap();
    let app = test::init_service(build_app(f.state.clone())).await;
    let hints = |term: &str| {
        test::TestRequest::get()
            .uri(&format!("/Search/Hints?searchTerm={term}"))
            .insert_header(("X-Emby-Token", f.user_token.as_str()))
            .to_request()
    };

    let mut leaked = Vec::new();
    for term in ["Rich", "Pharos", "award", "Jane", "Action"] {
        let body: serde_json::Value = test::call_and_read_body_json(&app, hints(term)).await;
        let names: Vec<&str> = body["SearchHints"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|h| h["Name"].as_str())
            .collect();
        if !names.is_empty() {
            leaked.push(format!("{term}: {names:?}"));
        }
    }
    assert!(
        leaked.is_empty(),
        "hidden library A leaked:\n{}",
        leaked.join("\n")
    );

    let body: serde_json::Value = test::call_and_read_body_json(&app, hints("Other")).await;
    assert_eq!(body["SearchHints"][0]["Name"], "Other Movie", "{body}");
    let browse: serde_json::Value = test::call_and_read_body_json(&app, hints("")).await;
    assert!(!browse.to_string().contains("Rich Movie"), "{browse}");

    // Restricted to the library that holds them, the same names answer.
    restrict_to_library_a(&f).await;
    let mut missing = Vec::new();
    for term in ["Rich", "Pharos", "award", "Jane"] {
        let body: serde_json::Value = test::call_and_read_body_json(&app, hints(term)).await;
        if body["SearchHints"].as_array().unwrap().is_empty() {
            missing.push(term);
        }
    }
    assert!(missing.is_empty(), "visible names missing: {missing:?}");
}

fn entity_hidden_total(
    snapshotter: &pharos_test_metrics::CumulativeSnapshotter,
    kind: &str,
) -> u64 {
    snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .filter(|(k, ..)| {
            let key = k.key();
            key.name() == "pharos_entity_hidden_total"
                && key.labels().any(|l| l.key() == "kind" && l.value() == kind)
        })
        .map(|(.., v)| match v {
            DebugValue::Counter(n) => n,
            other => panic!("{other:?}"),
        })
        .sum()
}

/// B262: `/Genres`, `/Studios`, `/Tags` and `/Persons` list a genre, studio,
/// tag or person only through items the bearer may see, with counts over
/// those items, and `/Persons/{id}` answers a person with none as 404.
/// Disarm check: with the listings back on the store-wide rows, library A's
/// names answer to a user restricted to library B and nothing is counted.
#[actix_web::test]
async fn entity_lists_answer_only_through_visible_items() {
    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);

    let f = seed_rich().await;
    let app = test::init_service(build_app(f.state.clone())).await;
    let get = |uri: &str| {
        test::TestRequest::get()
            .uri(uri)
            .insert_header(("X-Emby-Token", f.user_token.as_str()))
            .to_request()
    };
    let lists = [
        ("/Genres", "genre", "Action"),
        ("/Studios", "studio", "Pharos Studios"),
        ("/Tags", "tag", "award-winner"),
        ("/Persons", "person", "Jane Star"),
    ];

    // Restricted to library A, which holds them: each answers, counted once.
    restrict_to_library_a(&f).await;
    let mut jane = String::new();
    for (uri, _, name) in lists {
        let body: serde_json::Value = test::call_and_read_body_json(&app, get(uri)).await;
        let row = body["Items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["Name"] == name)
            .unwrap_or_else(|| panic!("{uri} lost visible {name}: {body}"))
            .clone();
        assert_eq!(row["ChildCount"], 1, "{uri}: {row}");
        if uri == "/Persons" {
            jane = row["Id"].as_str().unwrap().to_string();
        }
    }
    let resp = test::call_service(&app, get(&format!("/Persons/{jane}"))).await;
    assert_eq!(resp.status(), 200);

    // Restricted to library B: none of library A's names answer.
    let policy = UserPolicy {
        enable_all_folders: false,
        enabled_folders: vec![f.lib_b_wire.clone()],
        ..UserPolicy::default()
    };
    f.state.stores.set_policy(f.user_id, policy).await.unwrap();
    let mut leaked = Vec::new();
    for (uri, kind, name) in lists {
        let body: serde_json::Value = test::call_and_read_body_json(&app, get(uri)).await;
        let names: Vec<&str> = body["Items"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|i| i["Name"].as_str())
            .collect();
        if !names.is_empty() {
            leaked.push(format!("{uri} ({name} hidden): {names:?}"));
        }
        if entity_hidden_total(&snapshotter, kind) == 0 {
            leaked.push(format!("{uri}: nothing counted under kind={kind}"));
        }
    }
    let resp = test::call_service(&app, get(&format!("/Persons/{jane}"))).await;
    if resp.status() != 404 {
        leaked.push(format!("/Persons/{{id}}: {}", resp.status()));
    }
    assert!(
        leaked.is_empty(),
        "library A leaked:\n{}",
        leaked.join("\n")
    );
}

/// B263: a genre or person reached by id answers only through items the
/// bearer may see (V200). `/Items/{genre id}` and a person's portrait answer
/// a hidden one as they answer an id that names nothing.
/// Disarm check: with the by-id routes skipping `visible_entity`, the genre
/// answers 200 with library A's count and the portrait redirects.
#[actix_web::test]
async fn an_entity_by_id_answers_only_through_visible_items() {
    use pharos_core::PersonStore as _;
    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);

    let f = seed_rich().await;
    f.state
        .stores
        .upsert_person(
            "Jane Star",
            None,
            None,
            Some("https://img.example/jane.jpg"),
        )
        .await
        .unwrap();
    let app = test::init_service(build_app(f.state.clone())).await;
    let get = |uri: &str| {
        test::TestRequest::get()
            .uri(uri)
            .insert_header(("X-Emby-Token", f.user_token.as_str()))
            .to_request()
    };
    let id_of = |body: &serde_json::Value, name: &str| {
        body["Items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["Name"] == name)
            .unwrap_or_else(|| panic!("{name} missing: {body}"))["Id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let genres: serde_json::Value = test::call_and_read_body_json(&app, get("/Genres")).await;
    let persons: serde_json::Value = test::call_and_read_body_json(&app, get("/Persons")).await;
    let (action, jane) = (id_of(&genres, "Action"), id_of(&persons, "Jane Star"));
    let genre_uri = format!("/Items/{action}");
    let portrait_uri = format!("/Items/{jane}/Images/Primary");

    // Unrestricted, then restricted to library A, which holds them: both
    // answer.
    let genre: serde_json::Value = test::call_and_read_body_json(&app, get(&genre_uri)).await;
    assert_eq!(genre["ChildCount"], 1, "{genre}");
    let resp = test::call_service(&app, get(&portrait_uri)).await;
    assert_eq!(resp.status(), 302);
    restrict_to_library_a(&f).await;
    let genre: serde_json::Value = test::call_and_read_body_json(&app, get(&genre_uri)).await;
    assert_eq!(genre["ChildCount"], 1, "{genre}");
    let resp = test::call_service(&app, get(&portrait_uri)).await;
    assert_eq!(resp.status(), 302);

    // Restricted to library B: both answer as an id naming nothing does,
    // each counted.
    let policy = UserPolicy {
        enable_all_folders: false,
        enabled_folders: vec![f.lib_b_wire.clone()],
        ..UserPolicy::default()
    };
    f.state.stores.set_policy(f.user_id, policy).await.unwrap();
    let mut leaked = Vec::new();
    let bogus = "f".repeat(32);
    for (uri, missing, kind) in [
        (&genre_uri, format!("/Items/{bogus}"), "genre"),
        (
            &portrait_uri,
            format!("/Items/{bogus}/Images/Primary"),
            "person",
        ),
    ] {
        let want = test::call_service(&app, get(&missing)).await.status();
        let before = entity_hidden_total(&snapshotter, kind);
        let status = test::call_service(&app, get(uri)).await.status();
        let counted = entity_hidden_total(&snapshotter, kind) - before;
        if status != want || counted != 1 {
            leaked.push(format!(
                "{uri}: {status} (missing: {want}), counted {counted}"
            ));
        }
    }
    assert!(
        leaked.is_empty(),
        "library A leaked:\n{}",
        leaked.join("\n")
    );
}

/// B264: a box set is shared by every user and has no library of its own,
/// so it answers only through members the bearer may see (V200): listed by
/// `/Collections`, `/Items?IncludeItemTypes=BoxSet` and
/// `/Items/{id}/Collections` with its count over those members, and
/// `/Items/{collection id}` answers one with none as an unknown id does.
/// Disarm check: with the box set routes back on the store-wide rows,
/// library A's box set lists for a user restricted to library B, "Mixed"
/// counts both members, and nothing is counted.
#[actix_web::test]
async fn a_box_set_answers_only_through_visible_members() {
    use pharos_core::CollectionStore as _;
    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);

    let f = seed_rich().await;
    let (rich, other) = (f.rich_item_id, f.other_item_id);
    let only_a = f
        .state
        .stores
        .create_collection("Only A", &[rich])
        .await
        .unwrap()
        .wire_id;
    f.state
        .stores
        .create_collection("Mixed", &[rich, other])
        .await
        .unwrap();
    let app = test::init_service(build_app(f.state.clone())).await;
    let get = |uri: &str| {
        test::TestRequest::get()
            .uri(uri)
            .insert_header(("X-Emby-Token", f.user_token.as_str()))
            .to_request()
    };
    let rows = |body: &serde_json::Value| -> Vec<(String, u64)> {
        body["Items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| {
                (
                    i["Name"].as_str().unwrap().to_string(),
                    i["ChildCount"].as_u64().unwrap(),
                )
            })
            .collect()
    };
    let listings = [
        "/Collections".to_string(),
        "/Items?IncludeItemTypes=BoxSet".to_string(),
    ];

    // Restricted to library A: both box sets list, Mixed with its one
    // visible member.
    restrict_to_library_a(&f).await;
    for uri in &listings {
        let body: serde_json::Value = test::call_and_read_body_json(&app, get(uri)).await;
        let mut got = rows(&body);
        got.sort();
        assert_eq!(
            got,
            vec![("Mixed".to_string(), 1), ("Only A".to_string(), 1)],
            "{uri}"
        );
    }
    let resp = test::call_service(&app, get(&format!("/Items/{only_a}"))).await;
    assert_eq!(resp.status(), 200);

    // Restricted to library B: Only A is gone everywhere, Mixed counts only
    // library B's member.
    let policy = UserPolicy {
        enable_all_folders: false,
        enabled_folders: vec![f.lib_b_wire.clone()],
        ..UserPolicy::default()
    };
    f.state.stores.set_policy(f.user_id, policy).await.unwrap();
    let mut leaked = Vec::new();
    for uri in listings
        .iter()
        .cloned()
        .chain([format!("/Items/{other}/Collections")])
    {
        let body: serde_json::Value = test::call_and_read_body_json(&app, get(&uri)).await;
        let got = rows(&body);
        if got != vec![("Mixed".to_string(), 1)] {
            leaked.push(format!("{uri}: {got:?}"));
        }
    }
    let bogus = "f".repeat(32);
    let want = test::call_service(&app, get(&format!("/Items/{bogus}")))
        .await
        .status();
    let status = test::call_service(&app, get(&format!("/Items/{only_a}")))
        .await
        .status();
    if status != want {
        leaked.push(format!("/Items/{{only A}}: {status} (missing: {want})"));
    }
    if entity_hidden_total(&snapshotter, "collection") == 0 {
        leaked.push("nothing counted under kind=collection".into());
    }
    assert!(
        leaked.is_empty(),
        "library A leaked:\n{}",
        leaked.join("\n")
    );
}

fn syncplay_hidden_total(
    snapshotter: &pharos_test_metrics::CumulativeSnapshotter,
    route: &str,
) -> u64 {
    snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .filter(|(k, ..)| {
            let key = k.key();
            key.name() == "pharos_syncplay_group_hidden_total"
                && key
                    .labels()
                    .any(|l| l.key() == "route" && l.value() == route)
        })
        .map(|(.., v)| match v {
            DebugValue::Counter(n) => n,
            other => panic!("{other:?}"),
        })
        .sum()
}

/// B265: a SyncPlay group is listed to, and joined by, only a user who may
/// see every item in its queue (V201): its name, its members and what it
/// plays are otherwise another library's. Disarm check: with `may_see_queue`
/// answering true, the library A group lists for a user restricted to
/// library B and neither route counts a refusal.
#[actix_web::test]
async fn a_syncplay_group_answers_only_a_user_who_may_see_its_queue() {
    use pharos_sync::group::GroupMsg;
    use pharos_sync::messages::MemberId;
    let recorder = DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);

    let f = seed_rich().await;
    let sinks = pharos_sync::MemberSinks::new();
    let registry = pharos_sync::GroupRegistry::spawn(std::sync::Arc::new(
        pharos_sync::LocalDelivery::new(sinks.clone()),
    ));
    // A group playing library A's item, kept alive by its leader.
    let group = registry.create().await.unwrap();
    let (tx, _leader_rx) = tokio::sync::mpsc::channel(64);
    let leader = MemberId::new();
    sinks.insert(leader, 1, tx);
    let (reply, joined) = tokio::sync::oneshot::channel();
    group
        .tx
        .send(GroupMsg::AddMember {
            member_id: leader,
            name: "admin".into(),
            reply,
        })
        .await
        .unwrap();
    joined.await.unwrap();
    group
        .tx
        .send(GroupMsg::SetNewQueue {
            sender: leader,
            item_ids: vec![pharos_jellyfin_api::dto::wire_item_id(f.rich_item_id)],
            item_runtimes_ms: vec![None],
            playing_index: 0,
            start_position_ms: 0,
        })
        .await
        .unwrap();
    assert_eq!(group.snapshot().await.unwrap().queue_item_ids.len(), 1);

    let app = test::init_service(
        build_app(f.state.clone())
            .app_data(actix_web::web::Data::new(registry))
            .app_data(actix_web::web::Data::new(sinks))
            .app_data(actix_web::web::Data::new(pharos_sync::SessionHub::new())),
    )
    .await;
    let list = || {
        test::TestRequest::get()
            .uri("/SyncPlay/List")
            .insert_header(("X-Emby-Token", f.user_token.as_str()))
            .to_request()
    };
    let group_id = group.group_id.to_string();

    restrict_to_library_a(&f).await;
    let body: serde_json::Value = test::call_and_read_body_json(&app, list()).await;
    assert_eq!(body[0]["GroupId"], group_id, "{body}");

    let policy = UserPolicy {
        enable_all_folders: false,
        enabled_folders: vec![f.lib_b_wire.clone()],
        ..UserPolicy::default()
    };
    f.state.stores.set_policy(f.user_id, policy).await.unwrap();
    let mut leaked = Vec::new();
    let body: serde_json::Value = test::call_and_read_body_json(&app, list()).await;
    if body.as_array().is_none_or(|a| !a.is_empty()) {
        leaked.push(format!("/SyncPlay/List: {body}"));
    }
    if syncplay_hidden_total(&snapshotter, "list") != 1 {
        leaked.push("list refusal not counted".into());
    }
    let join = test::TestRequest::post()
        .uri("/SyncPlay/Join")
        .insert_header(("X-Emby-Token", f.user_token.as_str()))
        .insert_header((
            "X-Emby-Authorization",
            "MediaBrowser Client=\"t\", Device=\"t\", DeviceId=\"d1\", Version=\"1\"",
        ))
        .set_json(serde_json::json!({ "GroupId": group_id }))
        .to_request();
    let status = test::call_service(&app, join).await.status();
    if syncplay_hidden_total(&snapshotter, "join") != 1 {
        leaked.push(format!("join ({status}) refusal not counted"));
    }
    assert!(
        leaked.is_empty(),
        "library A's group leaked:\n{}",
        leaked.join("\n")
    );
}

/// The type of the native socket's answer to a `Join` on `group_id`:
/// `joined`, or `error` when the group is refused.
async fn native_join_reply(
    server: &actix_test::TestServer,
    token: &str,
    group_id: pharos_sync::messages::GroupId,
) -> String {
    use futures_util::{SinkExt as _, StreamExt as _};
    let url = server.url("/sync/v1/ws").replacen("http", "ws", 1);
    let (_resp, mut ws) = awc::Client::new().ws(url).connect().await.unwrap();
    for msg in [
        serde_json::json!({
            "type": "hello", "token": token, "client": "t", "device_id": "d", "name": "n",
        }),
        serde_json::json!({ "type": "join", "group_id": group_id }),
    ] {
        ws.send(awc::ws::Message::Text(msg.to_string().into()))
            .await
            .unwrap();
    }
    let deadline = std::time::Duration::from_secs(5);
    while let Ok(Some(frame)) = tokio::time::timeout(deadline, ws.next()).await {
        if let Ok(awc::ws::Frame::Text(text)) = frame {
            let msg: serde_json::Value = serde_json::from_slice(&text).unwrap();
            let kind = msg["type"].as_str().unwrap_or_default();
            if kind == "joined" || kind == "error" {
                return kind.to_string();
            }
        }
    }
    "no answer".into()
}

/// B265 / V201: a `Join` on the native `/sync/v1/ws` admits only a user who
/// may see the group's queue; a refusal reads as an unknown group.
/// Disarm check: with `StoreTokenResolver::may_join` answering true, the user
/// restricted to library B receives `joined`.
#[actix_web::test]
async fn a_native_sync_join_needs_a_visible_queue() {
    use pharos_sync::group::GroupMsg;
    use pharos_sync::messages::MemberId;
    let f = seed_rich().await;
    let sinks = pharos_sync::MemberSinks::new();
    let registry = pharos_sync::GroupRegistry::spawn(std::sync::Arc::new(
        pharos_sync::LocalDelivery::new(sinks.clone()),
    ));
    // A group playing library A's item, kept alive by its leader.
    let group = registry.create().await.unwrap();
    let (tx, _leader_rx) = tokio::sync::mpsc::channel(64);
    let leader = MemberId::new();
    sinks.insert(leader, 1, tx);
    let (reply, joined) = tokio::sync::oneshot::channel();
    group
        .tx
        .send(GroupMsg::AddMember {
            member_id: leader,
            name: "admin".into(),
            reply,
        })
        .await
        .unwrap();
    joined.await.unwrap();
    group
        .tx
        .send(GroupMsg::SetNewQueue {
            sender: leader,
            item_ids: vec![pharos_jellyfin_api::dto::wire_item_id(f.rich_item_id)],
            item_runtimes_ms: vec![None],
            playing_index: 0,
            start_position_ms: 0,
        })
        .await
        .unwrap();
    assert_eq!(group.snapshot().await.unwrap().queue_item_ids.len(), 1);

    let state = f.state.clone();
    let server = actix_test::start(move || {
        actix_web::App::new()
            .app_data(state.clone())
            .app_data(actix_web::web::Data::new(registry.clone()))
            .app_data(actix_web::web::Data::new(sinks.clone()))
            .app_data(actix_web::web::Data::new(
                pharos_server::sync_resolver::build(state.clone().into_inner()),
            ))
            .configure(pharos_sync::ws::register)
    });

    let mut replies = Vec::new();
    let policy = UserPolicy {
        enable_all_folders: false,
        enabled_folders: vec![f.lib_b_wire.clone()],
        ..UserPolicy::default()
    };
    f.state.stores.set_policy(f.user_id, policy).await.unwrap();
    replies.push((
        "library B",
        native_join_reply(&server, &f.user_token, group.group_id).await,
    ));
    restrict_to_library_a(&f).await;
    replies.push((
        "library A",
        native_join_reply(&server, &f.user_token, group.group_id).await,
    ));
    replies.push((
        "admin",
        native_join_reply(&server, &f.admin_token, group.group_id).await,
    ));
    assert_eq!(
        replies,
        vec![
            ("library B", "error".to_string()),
            ("library A", "joined".to_string()),
            ("admin", "joined".to_string()),
        ]
    );
}
