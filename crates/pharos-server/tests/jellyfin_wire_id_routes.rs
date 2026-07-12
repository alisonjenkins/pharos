#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Every id-bearing route must accept ALL THREE wire forms of an item id:
//!
//! - the canonical dashless 32-hex GUID (what pharos emits since B15),
//! - the dashed GUID variant (some SDKs re-serialize UUIDs dashed),
//! - the legacy plain-decimal form (open sessions from before B15).
//!
//! Regression: the B15 sweep missed the five HLS playback handlers (bound as
//! `id_num`/`media_id`, dodging the sweep regex), so PlaybackInfo handed out
//! hex-id TranscodingUrls that the segment/playlist routes then 400'd —
//! ALL video playback broke while every test stayed green, because tests
//! called routes with decimal ids directly and nothing followed the
//! server-emitted URL back into the server (B16).
//!
//! Adding a new id-bearing route? Add ONE line to `ROUTE_MATRIX` below.

use actix_web::{test, web, App};
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore, UserId, UserPolicy,
    UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

const ITEM_ID: u64 = 77;

/// `{id}` is replaced with the item id under test; `{uid}` with the seeded
/// user's id. The assertion only isolates ID PARSING: any status is fine
/// EXCEPT a 400 whose body blames the id ("invalid id" / "invalid item id") —
/// missing media files / caches legitimately 404/500 in this harness.
const ROUTE_MATRIX: &[(&str, &str)] = &[
    ("GET", "/Items/{id}"),
    ("POST", "/Items/{id}/PlaybackInfo"),
    ("GET", "/Items/{id}/Images/Primary"),
    ("GET", "/Items/{id}/Similar"),
    ("GET", "/videos/{id}/master.m3u8"),
    ("GET", "/videos/{id}/hls1/main/0.ts"),
    ("GET", "/videos/{id}/vp9/master.m3u8"),
    ("GET", "/videos/{id}/vp9/main.m3u8"),
    ("GET", "/videos/{id}/vp9/init.mp4"),
    ("GET", "/videos/{id}/vp9/0.m4s"),
    ("GET", "/videos/{id}/stream"),
    ("GET", "/Videos/{id}/{id}/Subtitles/0/Stream.vtt"),
    ("GET", "/Videos/{id}/{id}/Attachments/0"),
    ("GET", "/Items/{id}/Trickplay/320/0.jpg"),
    ("POST", "/Users/{uid}/PlayedItems/{id}"),
    ("DELETE", "/Users/{uid}/PlayedItems/{id}"),
    ("POST", "/Items/{id}/Tags/Add"),
];

async fn seed() -> (web::Data<AppState>, String, String) {
    let stores = Stores::connect("sqlite::memory:").await.unwrap();
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
    stores
        .put(MediaItem {
            id: ITEM_ID,
            path: "/m/77.mkv".into(),
            title: "WireId".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(60_000),
                width: Some(1920),
                height: Some(1080),
                bitrate_bps: Some(4_000_000),
                container: Some("matroska,webm".into()),
                video_codec: Some("h264".into()),
                audio_codec: Some("aac".into()),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
    let state = web::Data::new(AppState::new(stores, "t".into()));
    (
        state,
        token.0.expose().to_string(),
        uid.0.simple().to_string(),
    )
}

fn dashed(hex32: &str) -> String {
    format!(
        "{}-{}-{}-{}-{}",
        &hex32[0..8],
        &hex32[8..12],
        &hex32[12..16],
        &hex32[16..20],
        &hex32[20..32]
    )
}

async fn run_matrix(id_form: &str, label: &str) {
    let (state, token, uid) = seed().await;
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    for (method, template) in ROUTE_MATRIX {
        let uri = template.replace("{id}", id_form).replace("{uid}", &uid);
        let req = match *method {
            "GET" => test::TestRequest::get(),
            "POST" => test::TestRequest::post(),
            "DELETE" => test::TestRequest::delete(),
            other => panic!("unhandled method {other}"),
        }
        .uri(&uri)
        .insert_header(("X-Emby-Token", token.as_str()))
        .insert_header(("content-type", "application/json"))
        .set_payload("{}")
        .to_request();
        let resp = test::call_service(&app, req).await;
        let status = resp.status();
        let body = String::from_utf8_lossy(&test::read_body(resp).await).to_string();
        assert!(
            !(status == 400 && body.to_ascii_lowercase().contains("invalid")),
            "{label} id form rejected: {method} {uri} -> {status} {body}"
        );
    }
}

#[actix_web::test]
async fn all_id_routes_accept_canonical_hex() {
    run_matrix(&format!("{ITEM_ID:032x}"), "canonical-hex").await;
}

#[actix_web::test]
async fn all_id_routes_accept_legacy_decimal() {
    run_matrix(&ITEM_ID.to_string(), "legacy-decimal").await;
}

#[actix_web::test]
async fn all_id_routes_accept_dashed_guid() {
    run_matrix(&dashed(&format!("{ITEM_ID:032x}")), "dashed-guid").await;
}

/// The test that would have caught B16: follow the URL the SERVER emits back
/// into the server — PlaybackInfo's TranscodingUrl (hex id since B15) must be
/// accepted by the playlist route it points at.
#[actix_web::test]
async fn transcoding_url_round_trips_through_the_hls_routes() {
    let (state, token, _uid) = seed().await;
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    // A browser-shaped profile that can't direct-play h264/mkv → transcode.
    let req = test::TestRequest::post()
        .uri(&format!("/Items/{ITEM_ID:032x}/PlaybackInfo"))
        .insert_header(("X-Emby-Token", token.as_str()))
        .insert_header(("content-type", "application/json"))
        .insert_header((
            "user-agent",
            "Mozilla/5.0 (X11; Linux x86_64; rv:152.0) Gecko/20100101 Firefox/152.0",
        ))
        .set_payload(
            r#"{"DeviceProfile":{"DirectPlayProfiles":[
                {"Container":"webm","Type":"Video","VideoCodec":"vp9","AudioCodec":"opus"}
            ]}}"#,
        )
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let url = v["MediaSources"][0]["TranscodingUrl"]
        .as_str()
        .expect("browser profile must get a TranscodingUrl")
        .to_string();
    assert!(
        url.contains(&format!("{ITEM_ID:032x}")),
        "TranscodingUrl must embed the canonical hex id: {url}"
    );
    // Follow the server-emitted URL back into the server.
    let req = test::TestRequest::get()
        .uri(&url)
        .insert_header(("X-Emby-Token", token.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    let status = resp.status();
    let body = String::from_utf8_lossy(&test::read_body(resp).await).to_string();
    assert!(
        !(status == 400 && body.to_ascii_lowercase().contains("invalid")),
        "server-emitted TranscodingUrl rejected by its own route: {status} {body}"
    );
}

/// People photos: jellyfin-web's favorites tab + cast lists fetch
/// /Items/{personWireId}/Images/Primary. The scanner records a scraped
/// thumb_url for most people — serve it as a 302 (the Live-TV channel-logo
/// pattern); a person without one 404s and the client draws its initials
/// placeholder.
#[actix_web::test]
async fn person_primary_image_redirects_to_scraped_thumb() {
    use pharos_core::PersonStore;
    let (state, _token, _uid) = seed().await;
    let with_thumb = state
        .stores
        .upsert_person(
            "Jane Actor",
            None,
            None,
            Some("https://img.example/jane.jpg"),
        )
        .await
        .unwrap();
    let _ = with_thumb;
    let bare = state
        .stores
        .upsert_person("No Photo", None, None, None)
        .await
        .unwrap();
    let _ = bare;
    let localpath = state
        .stores
        .upsert_person(
            "Legacy Path",
            None,
            None,
            Some("/config/data/metadata/People/L/Legacy Path/folder.jpg"),
        )
        .await
        .unwrap();
    let _ = localpath;
    let jane_wire = pharos_core::person_wire_id("Jane Actor");
    let bare_wire = pharos_core::person_wire_id("No Photo");
    let local_wire = pharos_core::person_wire_id("Legacy Path");
    let app = test::init_service(
        App::new()
            .app_data(state)
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;

    let req = test::TestRequest::get()
        .uri(&format!("/Items/{jane_wire}/Images/Primary"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 302, "person with thumb redirects");
    assert_eq!(
        resp.headers().get("location").unwrap(),
        "https://img.example/jane.jpg"
    );

    let req = test::TestRequest::get()
        .uri(&format!("/Items/{bare_wire}/Images/Primary"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        404,
        "person without thumb 404s (placeholder)"
    );

    // A LEGACY local-path thumb (scraped from an old Jellyfin install's
    // metadata dir — this library has 853 of them and zero http ones) must
    // NOT redirect: a browser resolves it relative to pharos and 404s
    // uglier. Serve the placeholder 404 instead.
    let req = test::TestRequest::get()
        .uri(&format!("/Items/{local_wire}/Images/Primary"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 404, "local-path thumb must not redirect");
}
