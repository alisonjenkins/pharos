//! T86 — `GET /MediaSegments/{itemId}` content + layering guard.
//!
//! `wire_baseline` only pins the envelope keys. This drives the real
//! Skip Intro / Skip Outro path end-to-end: chapter-derived segments
//! layered over auto-detected ones (chapters win a shared Type), the
//! DTO `Id` is a valid UUID (B69), StartTicks/EndTicks are ms×10⁴, and
//! `includeSegmentTypes` narrows the response like Jellyfin's endpoint.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{
    DetectedSegment, MediaChapter, MediaItem, MediaKind, MediaSegmentKind, MediaSegmentStore,
    MediaStore, SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore,
    SEGMENT_SCHEMA_VERSION,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};
use serde_json::Value;

/// Seed one movie carrying an "Opening" chapter (→ Intro) plus two
/// auto-detected segments: an Intro (must be SUPPRESSED — the chapter
/// covers that Type) and an Outro (must SURVIVE).
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

    let mut item = MediaItem {
        id: 1,
        path: "/m/sample.mkv".into(),
        title: "Sample".into(),
        kind: MediaKind::Movie,
        ..Default::default()
    };
    item.probe.chapters = vec![MediaChapter {
        start_ms: 0,
        end_ms: 30_000,
        title: "Opening".into(),
    }];
    stores.put(item).await.unwrap();

    // Auto-detected set the backfill would have persisted.
    stores
        .set_media_segments(
            1,
            &[
                DetectedSegment {
                    kind: MediaSegmentKind::Intro, // duplicate Type — chapter wins
                    start_ms: 5_000,
                    end_ms: 40_000,
                    detector: "chromaprint".into(),
                    confidence: 0.9,
                },
                DetectedSegment {
                    kind: MediaSegmentKind::Outro,
                    start_ms: 1_000_000,
                    end_ms: 1_050_000,
                    detector: "chromaprint".into(),
                    confidence: 0.8,
                },
            ],
            SEGMENT_SCHEMA_VERSION,
        )
        .await
        .unwrap();

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

/// GET `uri` and return its `Items` array. A macro (not a fn) so it borrows the
/// concrete `init_service` app without naming its opaque `Service` bound.
macro_rules! items_at {
    ($app:expr, $uri:expr, $token:expr) => {{
        let body = test::call_and_read_body(
            &$app,
            test::TestRequest::get()
                .uri($uri)
                .insert_header(("X-Emby-Token", $token))
                .to_request(),
        )
        .await;
        let v: Value = serde_json::from_slice(&body).unwrap();
        v["Items"].as_array().cloned().unwrap_or_default()
    }};
}

fn types(items: &[Value]) -> Vec<String> {
    items
        .iter()
        .map(|i| i["Type"].as_str().unwrap().to_string())
        .collect()
}

#[actix_web::test]
async fn detected_and_chapter_segments_layer_with_chapter_winning() {
    let (state, token) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let items = items_at!(app, "/MediaSegments/1", token.as_str());

    let t = types(&items);
    // Chapter Intro + detected Outro — and EXACTLY ONE Intro (the detected
    // Intro is suppressed because the chapter already supplied that Type).
    assert!(t.contains(&"Intro".to_string()), "Intro present: {t:?}");
    assert!(t.contains(&"Outro".to_string()), "Outro present: {t:?}");
    assert_eq!(
        t.iter().filter(|x| *x == "Intro").count(),
        1,
        "chapter wins the shared Intro Type: {t:?}"
    );

    // The chapter Intro carries ms×10⁴ ticks (0 .. 30_000ms → 0 .. 3e8).
    let intro = items.iter().find(|i| i["Type"] == "Intro").unwrap();
    assert_eq!(intro["StartTicks"].as_u64().unwrap(), 0);
    assert_eq!(intro["EndTicks"].as_u64().unwrap(), 300_000_000);

    // Detected Outro survived with its own ticks (1_000_000ms → 1e10).
    let outro = items.iter().find(|i| i["Type"] == "Outro").unwrap();
    assert_eq!(outro["StartTicks"].as_u64().unwrap(), 10_000_000_000);
    assert_eq!(outro["EndTicks"].as_u64().unwrap(), 10_500_000_000);

    // Every Id is a 32-hex UUID (simple form) — the strict kotlin SDK
    // rejects a non-UUID MediaSegment.Id mid-playback (B69).
    for i in &items {
        let id = i["Id"].as_str().unwrap();
        assert_eq!(id.len(), 32, "uuid-simple id: {id}");
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()), "hex id: {id}");
    }
}

#[actix_web::test]
async fn include_segment_types_narrows_the_response() {
    let (state, token) = seed().await;
    let app = test::init_service(build_app(state)).await;

    // Only Intro requested → Outro filtered out.
    let only_intro = items_at!(
        app,
        "/MediaSegments/1?includeSegmentTypes=Intro",
        token.as_str()
    );
    assert_eq!(types(&only_intro), vec!["Intro".to_string()]);

    // Only Outro requested → Intro filtered out.
    let only_outro = items_at!(
        app,
        "/MediaSegments/1?includeSegmentTypes=Outro",
        token.as_str()
    );
    assert_eq!(types(&only_outro), vec!["Outro".to_string()]);

    // Comma list requesting both → both survive.
    let both = items_at!(
        app,
        "/MediaSegments/1?includeSegmentTypes=Intro,Outro",
        token.as_str()
    );
    let mut got = types(&both);
    got.sort();
    assert_eq!(got, vec!["Intro".to_string(), "Outro".to_string()]);

    // Blank param behaves like absent → everything.
    let blank = items_at!(app, "/MediaSegments/1?includeSegmentTypes=", token.as_str());
    assert_eq!(
        blank.len(),
        2,
        "blank filter returns all: {:?}",
        types(&blank)
    );
}
