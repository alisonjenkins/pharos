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
