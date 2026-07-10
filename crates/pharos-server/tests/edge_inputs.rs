//! Edge-input hardening — drive every common Jellyfin endpoint with
//! adversarial parameters that real fuzzers + bored users find: huge
//! Limit, negative-overflow StartIndex, empty arrays in DeviceProfile,
//! non-utf8 ItemIds, etc. Companion to `route_smoke` (default
//! inputs) + `negotiator_proptest` (combinatorial fuzz on the
//! negotiator).
//!
//! Asserts: no 5xx + the JSON envelope shape stays well-formed.
//! Anything 4xx is acceptable (validation, auth) — only crashes /
//! malformed responses fail.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore};
use pharos_server::{api::jellyfin, auth::BuiltinAuth, middleware::LowercasePath, state::AppState};
use pharos_store_sqlx::sqlite::SqliteStore;
use pharos_sync::GroupRegistry;

async fn seed() -> (
    web::Data<AppState>,
    web::Data<GroupRegistry>,
    String,
    UserId,
) {
    let stores = SqliteStore::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("p")).unwrap();
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "u".into(),
            password_hash: hash,
            policy: UserPolicy { admin: true },
        })
        .await
        .unwrap();
    let token = stores.issue(uid, "t").await.unwrap();
    (
        web::Data::new(AppState::new(stores, "edge".into())),
        web::Data::new(GroupRegistry::spawn()),
        token.0.expose().to_string(),
        uid,
    )
}

fn build_app(
    state: web::Data<AppState>,
    reg: web::Data<GroupRegistry>,
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
        .app_data(reg)
        .app_data(web::Data::new(pharos_sync::SessionHub::new()))
        .wrap(LowercasePath)
        .configure(jellyfin::configure)
}

/// `/Items?Limit=N` accepts absurdly large N without 5xx (clamped or
/// honoured but never crashing).
#[actix_web::test]
async fn items_huge_limit_does_not_500() {
    let (state, reg, token, _) = seed().await;
    let app = test::init_service(build_app(state, reg)).await;
    for limit in [u32::MAX, 1_000_000, 999] {
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/Items?Limit={limit}"))
                .insert_header(("X-Emby-Token", token.as_str()))
                .to_request(),
        )
        .await;
        assert!(resp.status().as_u16() < 500, "Limit={limit} → {resp:?}");
    }
}

/// StartIndex beyond the result set returns an empty Items array,
/// not a 5xx + not a Page-out-of-bounds.
#[actix_web::test]
async fn items_start_index_beyond_set_is_empty() {
    let (state, reg, token, _) = seed().await;
    let app = test::init_service(build_app(state, reg)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Items?StartIndex=999999&Limit=10")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v["Items"].as_array().unwrap().is_empty());
    assert_eq!(v["StartIndex"], 999999);
}

/// Non-numeric item ids in /Items/{id} return 4xx (404 or 400) but
/// never 5xx.
#[actix_web::test]
async fn items_non_numeric_id_returns_4xx_not_5xx() {
    let (state, reg, token, _) = seed().await;
    let app = test::init_service(build_app(state, reg)).await;
    for bad in &[
        "not-a-number",
        "0xdeadbeef",
        "1.5",
        "-1",
        // Reserved/synth-id shape — short-circuits to CollectionFolder
        // for the all-media placeholder; everything else should 400.
        "deadbeefdeadbeefdeadbeefdead",
    ] {
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/Items/{bad}"))
                .insert_header(("X-Emby-Token", token.as_str()))
                .to_request(),
        )
        .await;
        let status = resp.status().as_u16();
        assert!(status < 500, "/Items/{bad} → {status}");
    }
}

/// /Items/{id}/PlaybackInfo with empty DeviceProfile must still
/// emit a valid Decision (fallback transcode). Empty arrays
/// inside DeviceProfile must not panic.
#[actix_web::test]
async fn playback_info_with_empty_profile_branches_succeeds() {
    let (state, reg, token, _) = seed().await;
    // Seed a real item so the handler has something to act on.
    use pharos_core::{MediaItem, MediaKind, MediaStore};
    state
        .stores
        .put(MediaItem {
            id: 1,
            path: "/m/x.webm".into(),
            title: "x".into(),
            kind: MediaKind::Movie,
            ..Default::default()
        })
        .await
        .unwrap();
    let app = test::init_service(build_app(state, reg)).await;
    for body in &[
        // Truly empty.
        "{}",
        // Every field present but with empty arrays.
        r#"{"DeviceProfile":{
              "DirectPlayProfiles":[],"TranscodingProfiles":[],
              "MaxStreamingBitrate":null,"MaxStaticBitrate":null
        }}"#,
        // Adversarial: cap=0.
        r#"{"DeviceProfile":{"MaxStreamingBitrate":0}}"#,
    ] {
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/Items/1/PlaybackInfo")
                .insert_header(("X-Emby-Token", token.as_str()))
                .insert_header(("content-type", "application/json"))
                .set_payload(*body)
                .to_request(),
        )
        .await;
        assert!(resp.status().is_success(), "body={body} → {resp:?}");
    }
}

/// /Search/Hints with empty + whitespace + very long search terms.
#[actix_web::test]
async fn search_hints_edge_inputs_do_not_500() {
    let (state, reg, token, _) = seed().await;
    let app = test::init_service(build_app(state, reg)).await;
    let long = "a".repeat(1024);
    for term in &[
        "",
        " ",
        "  \t  ",
        // Quotation marks aren't special to us but real users type them.
        "\"foo\"",
        // SQL injection canary — we don't issue user SQL but check
        // anyway since the title filter is server-side.
        "'; DROP TABLE media_items; --",
        long.as_str(),
    ] {
        let encoded = url_encode(term);
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/Search/Hints?searchTerm={encoded}"))
                .insert_header(("X-Emby-Token", token.as_str()))
                .to_request(),
        )
        .await;
        assert!(resp.status().as_u16() < 500, "term={term:?} → {resp:?}");
    }
}

/// /DisplayPreferences GET with a long dp_id — we use it as a primary-
/// key column; the bind shouldn't break sqlite.
#[actix_web::test]
async fn display_preferences_long_id_does_not_5xx() {
    let (state, reg, token, _) = seed().await;
    let app = test::init_service(build_app(state, reg)).await;
    let big = "x".repeat(256);
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/DisplayPreferences/{big}"))
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success(), "{resp:?}");
}

/// Empty body to POST endpoints that expect JSON — handler should
/// 400 (bad request) not 5xx.
#[actix_web::test]
async fn empty_body_on_post_endpoints_returns_4xx() {
    let (state, reg, token, _) = seed().await;
    let app = test::init_service(build_app(state, reg)).await;
    for path in &[
        "/Sessions/Playing",
        "/Sessions/Playing/Progress",
        "/Sessions/Playing/Stopped",
        "/Sessions/Capabilities",
    ] {
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri(path)
                .insert_header(("X-Emby-Token", token.as_str()))
                .insert_header(("content-type", "application/json"))
                .set_payload("")
                .to_request(),
        )
        .await;
        let status = resp.status().as_u16();
        assert!(status < 500, "{path} empty body → {status}");
    }
}

/// Minimal percent-encoder for search-term test inputs.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `Fields=…` is jellyfin-web's per-call DTO field selector. Pharos
/// always emits the full DTO; the param must be ignored gracefully
/// (no 4xx parse rejection, no 5xx).
#[actix_web::test]
async fn fields_query_param_is_silently_accepted() {
    let (state, reg, token, _) = seed().await;
    use pharos_core::{MediaItem, MediaKind, MediaStore};
    state
        .stores
        .put(MediaItem {
            id: 7,
            path: "/m/x.mkv".into(),
            title: "x".into(),
            kind: MediaKind::Movie,
            ..Default::default()
        })
        .await
        .unwrap();
    let app = test::init_service(build_app(state, reg)).await;
    let fields =
        "PrimaryImageAspectRatio,DateCreated,Path,MediaSourceCount,Overview,People,Tags,SortName";
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!(
                "/Items?Limit=5&Fields={}&EnableImageTypes=Primary%2CBackdrop",
                fields
            ))
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success(), "{resp:?}");
}

/// Items with non-ASCII / unicode paths + titles round-trip through
/// the store + DTO without mangling. Caught when a vorbis-tagged
/// Greek-name track surfaced as `???` in a previous Jellyfin
/// fork's UI.
#[actix_web::test]
async fn unicode_paths_and_titles_round_trip_intact() {
    let (state, reg, token, _) = seed().await;
    use pharos_core::{MediaItem, MediaKind, MediaStore};
    let title = "日本語のタイトル — Ελληνικά";
    state
        .stores
        .put(MediaItem {
            id: 9,
            path: "/m/日本語/Ελληνικά.mp3".into(),
            title: title.into(),
            kind: MediaKind::Audio,
            ..Default::default()
        })
        .await
        .unwrap();
    let app = test::init_service(build_app(state, reg)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Items/9")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["Name"], title);
}

/// `Recursive=true` + `EnableTotalRecordCount` and other ad-hoc
/// pagination toggles silently accepted. Pharos doesn't model
/// folders so always-recursive is correct.
#[actix_web::test]
async fn items_recursive_and_pagination_flags_silently_accepted() {
    let (state, reg, token, _) = seed().await;
    let app = test::init_service(build_app(state, reg)).await;
    for q in &[
        "Recursive=true",
        "Recursive=false",
        "EnableTotalRecordCount=false",
        "EnableImages=true&ImageTypeLimit=1",
        "Recursive=true&IncludeItemTypes=Movie,Episode&SortBy=DateCreated&SortOrder=Descending&Limit=24",
    ] {
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/Items?{q}"))
                .insert_header(("X-Emby-Token", token.as_str()))
                .to_request(),
        )
        .await;
        assert!(resp.status().is_success(), "q={q} → {resp:?}");
    }
}
