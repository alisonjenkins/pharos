#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaStore, SecretString, TokenStore, UserId, UserPolicy, UserRecord,
    UserStore,
};
use pharos_server::{
    api::jellyfin, auth::BuiltinAuth, middleware::LowercasePath, state::AppState,
};
use pharos_store_sqlx::sqlite::SqliteStore;

async fn seed() -> (web::Data<AppState>, String, UserId) {
    let stores = SqliteStore::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("hunter2")).unwrap();
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "ali".into(),
            password_hash: hash,
            policy: UserPolicy { admin: true },
        })
        .await
        .unwrap();
    let token = stores.issue(uid, "test").await.unwrap();

    for (i, k) in [
        MediaKind::Movie,
        MediaKind::Audio,
        MediaKind::Episode,
        MediaKind::Movie,
    ]
    .iter()
    .enumerate()
    {
        stores
            .put(MediaItem {
                id: (100 + i) as u64,
                path: format!("/m/{i}.x").into(),
                title: format!("title-{i}"),
                kind: *k,
                ..Default::default()
            })
            .await
            .unwrap();
    }
    let state = web::Data::new(AppState::new(stores, "test".into()));
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

#[actix_web::test]
async fn list_items_requires_auth() {
    let (state, _t, _u) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get().uri("/Items").to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401);
}

#[actix_web::test]
async fn list_items_returns_all_with_total_count() {
    let (state, token, _u) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Items")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"], 4);
    assert_eq!(v["StartIndex"], 0);
    assert_eq!(v["Items"].as_array().unwrap().len(), 4);
    let first = &v["Items"][0];
    assert!(first.get("Id").is_some());
    assert!(first.get("Name").is_some());
    assert!(first.get("Type").is_some());
    assert!(first.get("ServerId").is_some());
}

#[actix_web::test]
async fn list_items_pagination() {
    let (state, token, _u) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Items?StartIndex=1&Limit=2")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"], 4);
    assert_eq!(v["StartIndex"], 1);
    assert_eq!(v["Items"].as_array().unwrap().len(), 2);
}

#[actix_web::test]
async fn get_item_by_id_returns_pascalcase_dto() {
    let (state, token, _u) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Items/100")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["Id"], "100");
    assert_eq!(v["Name"], "title-0");
    assert_eq!(v["Type"], "Movie");
}

#[actix_web::test]
async fn get_item_unknown_id_is_404() {
    let (state, token, _u) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Items/9999")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 404);
}

#[actix_web::test]
async fn list_user_items_rejects_mismatched_user() {
    let (state, token, _u) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Users/deadbeefdeadbeefdeadbeefdeadbeef/Items")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 403);
}

#[actix_web::test]
async fn list_user_items_accepts_matching_user() {
    let (state, token, uid) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri(&format!("/Users/{}/Items", uid.0.simple()))
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());
}

#[actix_web::test]
async fn get_user_item_matches_bearer_returns_dto() {
    let (state, token, uid) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri(&format!("/Users/{}/Items/100", uid.0.simple()))
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["Id"], "100");
    assert_eq!(v["Name"], "title-0");
}

#[actix_web::test]
async fn get_user_item_rejects_other_user_id_in_path() {
    let (state, token, _uid) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Users/deadbeefdeadbeefdeadbeefdeadbeef/Items/100")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 403);
}

#[actix_web::test]
async fn list_items_filters_by_search_term() {
    let (state, token, _uid) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Items?SearchTerm=title-2")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"], 1);
    assert_eq!(v["Items"][0]["Name"], "title-2");
}

#[actix_web::test]
async fn list_items_filters_by_include_item_types() {
    let (state, token, _uid) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Items?IncludeItemTypes=Audio")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"], 1);
    assert_eq!(v["Items"][0]["Type"], "Audio");
}

#[actix_web::test]
async fn list_items_filters_with_two_types() {
    let (state, token, _uid) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Items?IncludeItemTypes=Movie,Episode")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    // 2 movies + 1 episode in the seed.
    assert_eq!(v["TotalRecordCount"], 3);
}

#[actix_web::test]
async fn list_items_sorts_descending_when_requested() {
    let (state, token, _uid) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Items?SortOrder=Descending")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let arr = v["Items"].as_array().unwrap();
    let names: Vec<String> = arr
        .iter()
        .map(|i| i["Name"].as_str().unwrap().to_string())
        .collect();
    // Default is title- prefix; descending lexicographic puts -3 first.
    assert_eq!(names.first().map(|s| s.as_str()), Some("title-3"));
}

#[actix_web::test]
async fn virtual_folders_returns_synth_library() {
    let (state, token, _u) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Library/VirtualFolders")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["Name"], "All Media");
}

/// Probe metadata round-trips into /Items/{id} MediaSources so the
/// jellyfin-web htmlVideoPlayer sees real Size + RunTimeTicks + Bitrate
/// instead of pre-T29-followup hardcoded stubs. Caught when the dev env
/// hung on the BBB fixture: 5.2 MB delivered against an advertised
/// 107 KB / 200 kbps stub stalled MSE.
async fn seed_with_probe(probe: pharos_core::MediaProbe) -> (web::Data<AppState>, String) {
    let stores = SqliteStore::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("pw")).unwrap();
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
    stores
        .put(MediaItem {
            id: 999,
            path: "/m/probed.webm".into(),
            title: "Probed".into(),
            kind: MediaKind::Movie,
            probe,
            series: None,
        })
        .await
        .unwrap();
    let state = web::Data::new(AppState::new(stores, "test".into()));
    (state, token.0.expose().to_string())
}

#[actix_web::test]
async fn get_item_renders_real_probe_metadata_into_media_source() {
    let probe = pharos_core::MediaProbe {
        size_bytes: Some(5_243_523),
        duration_ms: Some(10_000),
        container: Some("matroska,webm".into()),
        bitrate_bps: Some(4_194_018),
        video_codec: Some("vp9".into()),
        audio_codec: Some("opus".into()),
        width: Some(1920),
        height: Some(1080),
        frame_rate_mille: Some(23_976),
        audio_channels: Some(2),
        sample_rate: Some(48000),
        ..Default::default()
    };
    let (state, token) = seed_with_probe(probe).await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Items/999")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let src = &v["MediaSources"][0];
    assert_eq!(src["Size"], 5_243_523_u64, "size from probe");
    assert_eq!(src["Bitrate"], 4_194_018_u64, "bitrate from probe");
    assert_eq!(
        src["RunTimeTicks"], 100_000_000_u64,
        "10 s × 10_000_000 ticks/s"
    );
    assert_eq!(src["Container"], "webm");
    let streams = src["MediaStreams"].as_array().unwrap();
    let video = &streams[0];
    assert_eq!(video["Type"], "Video");
    assert_eq!(video["Codec"], "vp9");
    assert_eq!(video["Width"], 1920);
    assert_eq!(video["Height"], 1080);
    let audio = &streams[1];
    assert_eq!(audio["Type"], "Audio");
    assert_eq!(audio["Codec"], "opus");
    assert_eq!(audio["Channels"], 2);
    assert_eq!(audio["SampleRate"], 48000);
}

#[actix_web::test]
async fn get_item_omits_size_when_probe_absent() {
    // No probe data → no fabricated Size. Clients treat missing Size as
    // unknown rather than the old 107356 stub that lied about the file.
    let (state, token) = seed_with_probe(pharos_core::MediaProbe::default()).await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Items/999")
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let src = &v["MediaSources"][0];
    assert!(src.get("Size").is_none(), "Size omitted, got {src:?}");
    assert!(
        src.get("Bitrate").is_none(),
        "Bitrate omitted, got {src:?}"
    );
}

#[actix_web::test]
async fn playback_info_pulls_real_codec_and_size_from_probe() {
    let probe = pharos_core::MediaProbe {
        size_bytes: Some(5_243_523),
        duration_ms: Some(10_000),
        container: Some("matroska,webm".into()),
        bitrate_bps: Some(4_194_018),
        video_codec: Some("vp9".into()),
        audio_codec: Some("opus".into()),
        width: Some(1920),
        height: Some(1080),
        frame_rate_mille: Some(23_976),
        audio_channels: Some(2),
        sample_rate: Some(48000),
        ..Default::default()
    };
    let (state, token) = seed_with_probe(probe).await;
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::post()
        .uri("/Items/999/PlaybackInfo")
        .insert_header(("X-Emby-Token", token.as_str()))
        .insert_header(("content-type", "application/json"))
        .set_payload("{}")
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let src = &v["MediaSources"][0];
    assert_eq!(src["Container"], "webm");
    assert_eq!(src["Size"], 5_243_523_u64);
    assert_eq!(src["Bitrate"], 4_194_018_u64);
    assert_eq!(src["RunTimeTicks"], 100_000_000_u64);
    let streams = src["MediaStreams"].as_array().unwrap();
    assert_eq!(streams[0]["Codec"], "vp9");
    assert_eq!(streams[1]["Codec"], "opus");
}

#[actix_web::test]
async fn user_views_returns_one_collection_per_media_root() {
    // Seed two roots; expect two CollectionFolder entries with stable
    // ids derived from each root path (T-fix-7 per-root libraries).
    let stores = SqliteStore::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("pw")).unwrap();
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
    stores
        .put(MediaItem {
            id: 1,
            path: "/media/Movies/big.mkv".into(),
            title: "big".into(),
            kind: MediaKind::Movie,
            ..Default::default()
        })
        .await
        .unwrap();
    stores
        .put(MediaItem {
            id: 2,
            path: "/media/TV/show.mkv".into(),
            title: "show".into(),
            kind: MediaKind::Movie,
            ..Default::default()
        })
        .await
        .unwrap();
    let state = web::Data::new(
        AppState::new(stores, "srv".into())
            .with_media_roots(vec!["/media/Movies".into(), "/media/TV".into()]),
    );
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri(&format!("/Users/{}/Views", uid.0.simple()))
        .insert_header(("X-Emby-Token", token.0.expose()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let items = v["Items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    let names: Vec<&str> = items.iter().map(|x| x["Name"].as_str().unwrap()).collect();
    assert!(names.contains(&"Movies"));
    assert!(names.contains(&"TV"));
    // Each id must be 32 hex chars (per UI assumptions).
    for it in items {
        let id = it["Id"].as_str().unwrap();
        assert_eq!(id.len(), 32, "id {id} not 32 hex chars");
    }
}

#[actix_web::test]
async fn get_item_by_library_id_returns_collection_folder() {
    // Clicking into a library in jellyfin-web fetches
    // `/Users/{u}/Items/{libraryId}` first; the per-root library id
    // is 32-hex so the old u64::parse path 400'd and the view hung.
    let stores = SqliteStore::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("pw")).unwrap();
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
        AppState::new(stores, "srv".into())
            .with_media_roots(vec!["/media/Movies".into()]),
    );
    let app = test::init_service(build_app(state.clone())).await;
    // Discover library id from /Views.
    let req = test::TestRequest::get()
        .uri(&format!("/Users/{}/Views", uid.0.simple()))
        .insert_header(("X-Emby-Token", token.0.expose()))
        .to_request();
    let v: serde_json::Value =
        serde_json::from_slice(&test::call_and_read_body(&app, req).await).unwrap();
    let lib_id = v["Items"][0]["Id"].as_str().unwrap().to_string();
    // /Items/{lib_id} must return CollectionFolder, not 400.
    let req = test::TestRequest::get()
        .uri(&format!("/Items/{lib_id}"))
        .insert_header(("X-Emby-Token", token.0.expose()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let body = test::read_body(resp).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["Id"], lib_id);
    assert_eq!(v["Type"], "CollectionFolder");
    assert_eq!(v["IsFolder"], true);
}

#[actix_web::test]
async fn list_items_filters_by_parent_id_to_one_library() {
    let stores = SqliteStore::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("pw")).unwrap();
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
    stores
        .put(MediaItem {
            id: 1,
            path: "/media/Movies/big.mkv".into(),
            title: "movie-one".into(),
            kind: MediaKind::Movie,
            ..Default::default()
        })
        .await
        .unwrap();
    stores
        .put(MediaItem {
            id: 2,
            path: "/media/TV/show/s01e01.mkv".into(),
            title: "ep-one".into(),
            kind: MediaKind::Episode,
            ..Default::default()
        })
        .await
        .unwrap();
    let state = web::Data::new(
        AppState::new(stores, "srv".into())
            .with_media_roots(vec!["/media/Movies".into(), "/media/TV".into()]),
    );
    // Discover the Movies library id from /Views, then filter by it.
    let app = test::init_service(build_app(state.clone())).await;
    let req = test::TestRequest::get()
        .uri(&format!("/Users/{}/Views", uid.0.simple()))
        .insert_header(("X-Emby-Token", token.0.expose()))
        .to_request();
    let v: serde_json::Value =
        serde_json::from_slice(&test::call_and_read_body(&app, req).await).unwrap();
    let movies_id = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["Name"] == "Movies")
        .unwrap()["Id"]
        .as_str()
        .unwrap()
        .to_string();
    let req = test::TestRequest::get()
        .uri(&format!("/Items?ParentId={movies_id}"))
        .insert_header(("X-Emby-Token", token.0.expose()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let items = v["Items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{v:?}");
    assert_eq!(items[0]["Name"], "movie-one");
}

#[actix_web::test]
async fn episode_dto_carries_series_id_and_season_id() {
    let stores = SqliteStore::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("pw")).unwrap();
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
    stores
        .put(MediaItem {
            id: 500,
            path: "/m/TV/My Show/Season 2/My.Show.S02E07.mkv".into(),
            title: "Episode 7".into(),
            kind: MediaKind::Episode,
            series: Some(pharos_core::SeriesInfo {
                series_name: "My Show".into(),
                season_number: Some(2),
                episode_number: Some(7),
            }),
            ..Default::default()
        })
        .await
        .unwrap();
    let state = web::Data::new(AppState::new(stores, "srv".into()));
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::get()
        .uri("/Items/500")
        .insert_header(("X-Emby-Token", token.0.expose()))
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["Type"], "Episode");
    assert_eq!(v["SeriesName"], "My Show");
    assert_eq!(v["ParentIndexNumber"], 2);
    assert_eq!(v["IndexNumber"], 7);
    let series_id = v["SeriesId"].as_str().unwrap();
    let season_id = v["SeasonId"].as_str().unwrap();
    assert_eq!(series_id.len(), 32);
    assert_eq!(season_id.len(), 32);
    assert_ne!(series_id, season_id);
}

#[actix_web::test]
async fn get_item_by_series_id_returns_series_dto() {
    let stores = SqliteStore::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("pw")).unwrap();
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
    stores
        .put(MediaItem {
            id: 500,
            path: "/m/TV/Other Show/Season 1/file.s01e01.mkv".into(),
            title: "Ep".into(),
            kind: MediaKind::Episode,
            series: Some(pharos_core::SeriesInfo {
                series_name: "Other Show".into(),
                season_number: Some(1),
                episode_number: Some(1),
            }),
            ..Default::default()
        })
        .await
        .unwrap();
    let state = web::Data::new(AppState::new(stores, "srv".into()));
    let app = test::init_service(build_app(state)).await;
    // Discover series id by reading the episode first.
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Items/500")
            .insert_header(("X-Emby-Token", token.0.expose()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let series_id = v["SeriesId"].as_str().unwrap().to_string();
    let season_id = v["SeasonId"].as_str().unwrap().to_string();
    // Now resolve the synth Series DTO.
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri(&format!("/Items/{series_id}"))
            .insert_header(("X-Emby-Token", token.0.expose()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["Id"], series_id);
    assert_eq!(v["Name"], "Other Show");
    assert_eq!(v["Type"], "Series");
    assert_eq!(v["IsFolder"], true);
    // Resolve the synth Season DTO.
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri(&format!("/Items/{season_id}"))
            .insert_header(("X-Emby-Token", token.0.expose()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["Id"], season_id);
    assert_eq!(v["Type"], "Season");
    assert_eq!(v["SeriesName"], "Other Show");
    assert_eq!(v["IndexNumber"], 1);
}

#[actix_web::test]
async fn list_items_filters_by_series_id() {
    let stores = SqliteStore::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("pw")).unwrap();
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
    for (id, ep_num, show) in [(1u64, 1u32, "Show A"), (2, 2, "Show A"), (3, 1, "Show B")] {
        stores
            .put(MediaItem {
                id,
                path: format!("/m/TV/{show}/Season 1/s01e0{ep_num}.mkv").into(),
                title: format!("{show} E{ep_num}"),
                kind: MediaKind::Episode,
                series: Some(pharos_core::SeriesInfo {
                    series_name: show.to_string(),
                    season_number: Some(1),
                    episode_number: Some(ep_num),
                }),
                ..Default::default()
            })
            .await
            .unwrap();
    }
    let state = web::Data::new(AppState::new(stores, "srv".into()));
    let app = test::init_service(build_app(state)).await;
    // Read one episode to fish out the SeriesId for "Show A".
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Items/1")
            .insert_header(("X-Emby-Token", token.0.expose()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let show_a_id = v["SeriesId"].as_str().unwrap().to_string();
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri(&format!("/Items?ParentId={show_a_id}"))
            .insert_header(("X-Emby-Token", token.0.expose()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let items = v["Items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "expected 2 Show A episodes, got {v:?}");
    for it in items {
        assert!(
            it["Name"].as_str().unwrap().starts_with("Show A"),
            "{it:?}"
        );
    }
}

#[actix_web::test]
async fn playback_info_lists_sidecar_subtitle_when_present() {
    use std::io::Write;
    let stores = SqliteStore::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("pw")).unwrap();
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
    let td = tempfile::TempDir::new().unwrap();
    let video = td.path().join("show.mkv");
    std::fs::write(&video, b"x").unwrap();
    let mut sidecar = std::fs::File::create(td.path().join("show.eng.vtt")).unwrap();
    sidecar
        .write_all(b"WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nhi\n")
        .unwrap();
    stores
        .put(MediaItem {
            id: 4242,
            path: video,
            title: "show".into(),
            kind: MediaKind::Movie,
            ..Default::default()
        })
        .await
        .unwrap();
    let state = web::Data::new(AppState::new(stores, "srv".into()));
    let app = test::init_service(build_app(state)).await;
    let req = test::TestRequest::post()
        .uri("/Items/4242/PlaybackInfo")
        .insert_header(("X-Emby-Token", token.0.expose()))
        .insert_header(("content-type", "application/json"))
        .set_payload("{}")
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let streams = v["MediaSources"][0]["MediaStreams"].as_array().unwrap();
    let sub = streams
        .iter()
        .find(|s| s["Type"] == "Subtitle")
        .expect("subtitle stream synthesised from sidecar");
    assert_eq!(sub["IsExternal"], true);
    assert!(sub["DeliveryUrl"].as_str().unwrap().contains("/Subtitles/"));
}

#[actix_web::test]
async fn shows_next_up_returns_lowest_unwatched_per_series() {
    let stores = SqliteStore::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("pw")).unwrap();
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
    // Show A: S01E01 (played), S01E02 (unwatched), S01E03 (unwatched)
    // Show B: S01E01 (unwatched), S01E02 (unwatched)
    for (id, show, season, ep) in [
        (1u64, "Show A", 1u32, 1u32),
        (2, "Show A", 1, 2),
        (3, "Show A", 1, 3),
        (4, "Show B", 1, 1),
        (5, "Show B", 1, 2),
    ] {
        stores
            .put(MediaItem {
                id,
                path: format!("/m/{show}/S01E0{ep}.mkv").into(),
                title: format!("E{ep}"),
                kind: MediaKind::Episode,
                series: Some(pharos_core::SeriesInfo {
                    series_name: show.into(),
                    season_number: Some(season),
                    episode_number: Some(ep),
                }),
                ..Default::default()
            })
            .await
            .unwrap();
    }
    // Mark Show A E01 played.
    use pharos_core::UserDataStore;
    let mut data = pharos_core::UserItemData::default();
    data.played = true;
    stores.set_user_data(uid, 1, data).await.unwrap();

    let state = web::Data::new(AppState::new(stores, "srv".into()));
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Shows/NextUp")
            .insert_header(("X-Emby-Token", token.0.expose()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let items = v["Items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{v:?}");
    // Show A: next up is E2 (since E1 played).
    // Show B: next up is E1 (nothing played).
    let by_show: std::collections::HashMap<&str, u64> = items
        .iter()
        .map(|it| {
            (
                it["SeriesName"].as_str().unwrap(),
                it["IndexNumber"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(by_show.get("Show A"), Some(&2));
    assert_eq!(by_show.get("Show B"), Some(&1));
}

#[actix_web::test]
async fn audio_item_surfaces_artist_album_genre_from_probe() {
    let stores = SqliteStore::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("pw")).unwrap();
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
    let probe = pharos_core::MediaProbe {
        audio_codec: Some("mp3".into()),
        artist: Some("Kevin MacLeod".into()),
        album: Some("Carefree".into()),
        album_artist: Some("Kevin MacLeod".into()),
        genre: Some("Royalty Free".into()),
        ..Default::default()
    };
    stores
        .put(MediaItem {
            id: 88,
            path: "/m/carefree.mp3".into(),
            title: "Carefree".into(),
            kind: MediaKind::Audio,
            probe,
            ..Default::default()
        })
        .await
        .unwrap();
    let state = web::Data::new(AppState::new(stores, "srv".into()));
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Items/88")
            .insert_header(("X-Emby-Token", token.0.expose()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["Artists"][0], "Kevin MacLeod");
    assert_eq!(v["ArtistItems"][0]["Name"], "Kevin MacLeod");
    assert_eq!(v["ArtistItems"][0]["Id"].as_str().unwrap().len(), 32);
    assert_eq!(v["AlbumArtists"][0]["Name"], "Kevin MacLeod");
    assert_eq!(v["Album"], "Carefree");
    assert_eq!(v["AlbumId"].as_str().unwrap().len(), 32);
    assert_eq!(v["Genres"][0], "Royalty Free");
}

#[actix_web::test]
async fn genres_endpoint_aggregates_distinct_genre_tags() {
    let stores = SqliteStore::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("pw")).unwrap();
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
    for (id, g) in [(1u64, "Jazz"), (2, "Jazz"), (3, "Rock"), (4, "Classical")] {
        stores
            .put(MediaItem {
                id,
                path: format!("/m/{id}.mp3").into(),
                title: format!("Track {id}"),
                kind: MediaKind::Audio,
                probe: pharos_core::MediaProbe {
                    genre: Some(g.into()),
                    ..Default::default()
                },
                ..Default::default()
            })
            .await
            .unwrap();
    }
    let state = web::Data::new(AppState::new(stores, "srv".into()));
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Genres")
            .insert_header(("X-Emby-Token", token.0.expose()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"], 3, "{v:?}");
    let names: Vec<&str> = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap())
        .collect();
    // Sorted ascending.
    assert_eq!(names, vec!["Classical", "Jazz", "Rock"]);
    for it in v["Items"].as_array().unwrap() {
        assert_eq!(it["Type"], "Genre");
        assert_eq!(it["IsFolder"], true);
    }
}

#[actix_web::test]
async fn artists_albums_genres_route_into_filtered_tracks() {
    let stores = SqliteStore::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("pw")).unwrap();
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
    for (id, artist, album, genre) in [
        (1u64, "Kevin MacLeod", "Carefree", "Royalty Free"),
        (2, "Kevin MacLeod", "Carefree", "Royalty Free"),
        (3, "Kevin MacLeod", "Other", "Royalty Free"),
        (4, "Other Artist", "Stuff", "Rock"),
    ] {
        stores
            .put(MediaItem {
                id,
                path: format!("/m/{id}.mp3").into(),
                title: format!("Track {id}"),
                kind: MediaKind::Audio,
                probe: pharos_core::MediaProbe {
                    artist: Some(artist.into()),
                    album_artist: Some(artist.into()),
                    album: Some(album.into()),
                    genre: Some(genre.into()),
                    ..Default::default()
                },
                ..Default::default()
            })
            .await
            .unwrap();
    }
    let state = web::Data::new(AppState::new(stores, "srv".into()));
    let app = test::init_service(build_app(state)).await;

    // /Artists yields 2 distinct artists.
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Artists")
            .insert_header(("X-Emby-Token", token.0.expose()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"], 2);
    // Pluck Kevin MacLeod's id, scope /Items by it.
    let km = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["Name"] == "Kevin MacLeod")
        .unwrap();
    let km_id = km["Id"].as_str().unwrap();
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri(&format!("/Items?ParentId={km_id}"))
            .insert_header(("X-Emby-Token", token.0.expose()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"], 3, "{v:?}");

    // /Albums yields 3 distinct albums.
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Albums")
            .insert_header(("X-Emby-Token", token.0.expose()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"], 3);
    let carefree = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["Name"] == "Carefree")
        .unwrap();
    let carefree_id = carefree["Id"].as_str().unwrap();
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri(&format!("/Items?ParentId={carefree_id}"))
            .insert_header(("X-Emby-Token", token.0.expose()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["TotalRecordCount"], 2);
}

#[actix_web::test]
async fn sort_by_runtime_ticks_orders_by_duration() {
    let stores = SqliteStore::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("pw")).unwrap();
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
    for (id, title, dur_ms) in [(1u64, "Long", 30_000u64), (2, "Mid", 10_000), (3, "Short", 3_000)]
    {
        stores
            .put(MediaItem {
                id,
                path: format!("/m/{id}.mp4").into(),
                title: title.into(),
                kind: MediaKind::Movie,
                probe: pharos_core::MediaProbe {
                    duration_ms: Some(dur_ms),
                    ..Default::default()
                },
                ..Default::default()
            })
            .await
            .unwrap();
    }
    let state = web::Data::new(AppState::new(stores, "srv".into()));
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Items?SortBy=RuntimeTicks&SortOrder=Ascending")
            .insert_header(("X-Emby-Token", token.0.expose()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let names: Vec<&str> = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["Short", "Mid", "Long"]);
}

#[actix_web::test]
async fn sort_by_albumartist_groups_tracks() {
    let stores = SqliteStore::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("pw")).unwrap();
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
    for (id, title, artist) in [
        (1u64, "Z song", "Beta"),
        (2, "A song", "Alpha"),
        (3, "M song", "Alpha"),
    ] {
        stores
            .put(MediaItem {
                id,
                path: format!("/m/{id}.mp3").into(),
                title: title.into(),
                kind: MediaKind::Audio,
                probe: pharos_core::MediaProbe {
                    album_artist: Some(artist.into()),
                    ..Default::default()
                },
                ..Default::default()
            })
            .await
            .unwrap();
    }
    let state = web::Data::new(AppState::new(stores, "srv".into()));
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Items?SortBy=AlbumArtist&SortOrder=Ascending")
            .insert_header(("X-Emby-Token", token.0.expose()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let names: Vec<&str> = v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap())
        .collect();
    // Alpha tracks first (A song, M song), then Beta (Z song).
    assert_eq!(names, vec!["A song", "M song", "Z song"]);
}
