//! Wire-shape baseline test — pins the top-level JSON keys of the
//! Jellyfin endpoints clients consume.
//!
//! Differentiates from `route_smoke` (no 5xx) + `wire_invariants`
//! (cross-field semantics): this checks the *shape* of the response
//! against a baked-in expected key-set per endpoint. A
//! "TotalRecordCount" silently renamed to "totalRecordCount" would
//! slip past every other test layer — clients still parse `Items`
//! (the iteration path) and only later fail at "TotalRecordCount is
//! NaN" on a different code path. This one would fail loudly at the
//! schema layer.
//!
//! Expected key-sets sourced from jellyfin-web's
//! `node_modules/@jellyfin/sdk` type definitions for the same
//! endpoints. Not exhaustive (Jellyfin's DTOs have dozens of fields
//! many clients ignore) — checks the keys jellyfin-web demonstrably
//! reads (see crawl.spec.ts paths).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaStore, SecretString, TokenStore, UserId, UserPolicy, UserRecord,
    UserStore,
};
use pharos_server::{api::jellyfin, auth::BuiltinAuth, middleware::LowercasePath, state::AppState};
use pharos_store_sqlx::sqlite::SqliteStore;

async fn seed() -> (web::Data<AppState>, String, UserId) {
    let stores = SqliteStore::connect("sqlite::memory:").await.unwrap();
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
        .put(MediaItem {
            id: 1,
            path: "/m/sample.mkv".into(),
            title: "Sample".into(),
            kind: MediaKind::Movie,
            ..Default::default()
        })
        .await
        .unwrap();
    let token = stores.issue(uid, "t").await.unwrap();
    let state = web::Data::new(AppState::new(stores, "srv".into()));
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

/// Assert every key in `must_contain` appears in `obj`. Extra keys
/// in `obj` are fine — clients ignore unknown fields.
fn assert_keys_present(value: &serde_json::Value, must_contain: &[&str], label: &str) {
    let obj = value
        .as_object()
        .unwrap_or_else(|| panic!("{label}: response is not a JSON object: {value}"));
    let actual: Vec<&str> = obj.keys().map(|s| s.as_str()).collect();
    for k in must_contain {
        assert!(
            obj.contains_key(*k),
            "{label} missing required key `{k}`; got keys: {actual:?}"
        );
    }
}

#[actix_web::test]
async fn baseline_system_info_keys() {
    let (state, token, _) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/System/Info")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_keys_present(
        &v,
        &[
            "Id",
            "ServerName",
            "Version",
            "ProductName",
            "OperatingSystem",
            "StartupWizardCompleted",
            "WebSocketPortNumber",
        ],
        "/System/Info",
    );
}

#[actix_web::test]
async fn baseline_users_me_keys() {
    let (state, token, _) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Users/Me")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_keys_present(
        &v,
        &[
            "Id",
            "Name",
            "ServerId",
            "HasPassword",
            "Policy",
            "Configuration",
        ],
        "/Users/Me",
    );
    // Configuration sub-object has its own contract.
    assert_keys_present(
        &v["Configuration"],
        &[
            "AudioLanguagePreference",
            "SubtitleMode",
            "PlayDefaultAudioTrack",
            "RememberSubtitleSelections",
            "EnableNextEpisodeAutoPlay",
        ],
        "/Users/Me Configuration",
    );
}

#[actix_web::test]
async fn baseline_items_envelope_keys() {
    let (state, token, _) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Items?Limit=5")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_keys_present(
        &v,
        &["Items", "TotalRecordCount", "StartIndex"],
        "/Items envelope",
    );
    let item = &v["Items"][0];
    assert_keys_present(
        item,
        &[
            "Id",
            "Name",
            "ServerId",
            "Type",
            "MediaType",
            "IsFolder",
            "UserData",
            "MediaSources",
            "ImageTags",
        ],
        "/Items[0]",
    );
}

#[actix_web::test]
async fn baseline_playback_info_envelope_keys() {
    let (state, token, _) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::post()
            .uri("/Items/1/PlaybackInfo")
            .insert_header(("X-Emby-Token", token.as_str()))
            .insert_header(("content-type", "application/json"))
            .set_payload("{}")
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_keys_present(&v, &["MediaSources", "PlaySessionId"], "PlaybackInfo");
    let src = &v["MediaSources"][0];
    assert_keys_present(
        src,
        &[
            "Id",
            "Container",
            "Protocol",
            "Type",
            "SupportsDirectPlay",
            "SupportsDirectStream",
            "SupportsTranscoding",
            "MediaStreams",
        ],
        "PlaybackInfo MediaSources[0]",
    );
}

#[actix_web::test]
async fn baseline_views_envelope_keys() {
    let (state, token, uid) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri(&format!("/Users/{}/Views", uid.0.simple()))
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_keys_present(
        &v,
        &["Items", "TotalRecordCount", "StartIndex"],
        "/Users/{u}/Views",
    );
    let item = &v["Items"][0];
    assert_keys_present(
        item,
        &[
            "Id",
            "Name",
            "ServerId",
            "Type",
            "CollectionType",
            "IsFolder",
        ],
        "/Users/{u}/Views[0]",
    );
}

#[actix_web::test]
async fn baseline_items_counts_keys() {
    let (state, token, _) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Items/Counts")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_keys_present(
        &v,
        &[
            "MovieCount",
            "SeriesCount",
            "EpisodeCount",
            "ArtistCount",
            "SongCount",
            "AlbumCount",
            "ItemCount",
        ],
        "/Items/Counts",
    );
}

#[actix_web::test]
async fn baseline_genres_envelope_keys() {
    let (state, token, _) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Genres")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_keys_present(
        &v,
        &["Items", "TotalRecordCount", "StartIndex"],
        "/Genres envelope",
    );
}

#[actix_web::test]
async fn baseline_artists_envelope_keys() {
    let (state, token, _) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Artists")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_keys_present(
        &v,
        &["Items", "TotalRecordCount", "StartIndex"],
        "/Artists envelope",
    );
}

#[actix_web::test]
async fn baseline_shows_nextup_envelope_keys() {
    let (state, token, _) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Shows/NextUp")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_keys_present(
        &v,
        &["Items", "TotalRecordCount", "StartIndex"],
        "/Shows/NextUp envelope",
    );
}

#[actix_web::test]
async fn baseline_authenticate_envelope_keys() {
    let (state, _, _) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::post()
            .uri("/Users/AuthenticateByName")
            .insert_header((
                "X-Emby-Authorization",
                r#"MediaBrowser Client="x", Device="x", DeviceId="x", Version="1""#,
            ))
            .insert_header(("content-type", "application/json"))
            .set_payload(r#"{"Username":"u","Pw":"p"}"#)
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_keys_present(
        &v,
        &["User", "SessionInfo", "AccessToken", "ServerId"],
        "AuthenticateByName",
    );
    assert_keys_present(
        &v["SessionInfo"],
        &[
            "Id",
            "UserId",
            "UserName",
            "DeviceId",
            "DeviceName",
            "Client",
            "ApplicationVersion",
            "ServerId",
        ],
        "AuthenticateByName SessionInfo",
    );
}

#[actix_web::test]
async fn baseline_localization_cultures_keys() {
    let (state, token, _) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Localization/Cultures")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let arr = v.as_array().expect("Cultures: top-level array");
    assert!(!arr.is_empty(), "Cultures: at least one entry");
    assert_keys_present(
        &arr[0],
        &[
            "Name",
            "DisplayName",
            "TwoLetterISOLanguageName",
            "ThreeLetterISOLanguageName",
            "ThreeLetterISOLanguageNames",
        ],
        "/Localization/Cultures[0]",
    );
}

#[actix_web::test]
async fn baseline_localization_countries_keys() {
    let (state, token, _) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Localization/Countries")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let arr = v.as_array().expect("Countries: top-level array");
    assert!(!arr.is_empty(), "Countries: at least one entry");
    assert_keys_present(
        &arr[0],
        &[
            "Name",
            "DisplayName",
            "TwoLetterISORegionName",
            "ThreeLetterISORegionName",
        ],
        "/Localization/Countries[0]",
    );
}

#[actix_web::test]
async fn baseline_devices_envelope_keys() {
    // /Devices is admin-gated; non-admin token returns 403.
    // Promote our seeded user to admin first.
    let (state, token, uid) = seed().await;
    state
        .stores
        .set_policy(uid, UserPolicy { admin: true })
        .await
        .unwrap();
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Devices")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_keys_present(
        &v,
        &["Items", "TotalRecordCount", "StartIndex"],
        "/Devices envelope",
    );
    let items = v["Items"].as_array().expect("Items array");
    assert!(!items.is_empty(), "seeded token must surface as a device");
    assert_keys_present(
        &items[0],
        &[
            "Id",
            "Name",
            "AppName",
            "AppVersion",
            "LastUserId",
            "LastUserName",
            "DateLastActivity",
        ],
        "/Devices Items[0]",
    );
}

#[actix_web::test]
async fn baseline_mediasegments_envelope_keys() {
    let (state, token, _) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/MediaSegments/1")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_keys_present(
        &v,
        &["Items", "TotalRecordCount", "StartIndex"],
        "/MediaSegments envelope",
    );
}
