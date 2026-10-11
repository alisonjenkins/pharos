//! /Items?AudioLanguages=&SubtitleLanguages= and /Items/Filters2 language facets.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use actix_web::{test, web, App};
use pharos_core::{
    AudioTrack, MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, SubtitleTrack,
    TokenStore, UserId, UserPolicy, UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

fn audio(lang: &str) -> AudioTrack {
    AudioTrack {
        stream_index: 1,
        language: Some(lang.into()),
        ..Default::default()
    }
}

fn sub(lang: &str) -> SubtitleTrack {
    SubtitleTrack {
        stream_index: 2,
        language: Some(lang.into()),
        codec: Some("subrip".into()),
        title: None,
        is_default: false,
        is_forced: false,
        is_hearing_impaired: false,
    }
}

/// A: audio eng+jpn, subs eng. B: audio fre, subs ger+eng. C: audio eng, no subs.
/// D: no tracks.
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
    let rows: [(u64, &str, Vec<AudioTrack>, Vec<SubtitleTrack>); 5] = [
        (1, "A", vec![audio("eng"), audio("jpn")], vec![sub("eng")]),
        (2, "B", vec![audio("fre")], vec![sub("ger"), sub("eng")]),
        (3, "C", vec![audio("ENG")], vec![]),
        (4, "D", vec![], vec![]),
        // Same languages as B, tagged with the 639-2/T spellings.
        (5, "E", vec![audio("fra")], vec![sub("deu")]),
    ];
    for (id, title, audio_tracks, subtitle_tracks) in rows {
        stores
            .put(MediaItem {
                id,
                path: format!("/m/{id}.mkv").into(),
                title: title.into(),
                kind: MediaKind::Movie,
                probe: MediaProbe {
                    audio_tracks,
                    subtitle_tracks,
                    ..Default::default()
                },
                ..Default::default()
            })
            .await
            .unwrap();
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

async fn names(state: &web::Data<AppState>, token: &str, qs: &str) -> Vec<String> {
    let app = test::init_service(build_app(state.clone())).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri(&format!("/Items?{qs}&Limit=100&SortBy=SortName"))
            .insert_header(("X-Emby-Token", token))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    v["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["Name"].as_str().unwrap_or("").to_string())
        .collect()
}

#[actix_web::test]
async fn audio_languages_matches_any_listed_language() {
    let (state, token) = seed().await;
    assert_eq!(
        names(&state, &token, "AudioLanguages=eng").await,
        ["A", "C"]
    );
    assert_eq!(
        names(&state, &token, "AudioLanguages=fre,jpn").await,
        ["A", "B", "E"]
    );
}

/// B and T spellings of one language are the same language, on both sides.
#[actix_web::test]
async fn bibliographic_and_terminologic_codes_match_each_other() {
    let (state, token) = seed().await;
    for q in [
        "AudioLanguages=fre",
        "AudioLanguages=fra",
        "AudioLanguages=FR",
    ] {
        assert_eq!(names(&state, &token, q).await, ["B", "E"], "{q}");
    }
    for q in ["SubtitleLanguages=ger", "SubtitleLanguages=deu"] {
        assert_eq!(names(&state, &token, q).await, ["B", "E"], "{q}");
    }
}

#[actix_web::test]
async fn repeated_language_keys_are_ored() {
    let (state, token) = seed().await;
    assert_eq!(
        names(&state, &token, "audioLanguages=fre&audioLanguages=jpn").await,
        ["A", "B", "E"]
    );
}

#[actix_web::test]
async fn audio_and_subtitle_languages_are_anded() {
    let (state, token) = seed().await;
    assert_eq!(
        names(
            &state,
            &token,
            "AudioLanguages=eng,fre&SubtitleLanguages=ger"
        )
        .await,
        ["B", "E"]
    );
    assert_eq!(
        names(&state, &token, "SubtitleLanguages=eng").await,
        ["A", "B"]
    );
    assert_eq!(
        names(&state, &token, "SubtitleLanguages=ger").await,
        ["B", "E"]
    );
}

#[actix_web::test]
async fn unknown_language_matches_nothing_and_absent_matches_all() {
    let (state, token) = seed().await;
    assert!(names(&state, &token, "AudioLanguages=zzz").await.is_empty());
    assert_eq!(names(&state, &token, "").await.len(), 5);
}

#[actix_web::test]
async fn filters2_lists_distinct_languages_as_name_value_pairs() {
    let (state, token) = seed().await;
    let app = test::init_service(build_app(state)).await;
    let body = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri("/Items/Filters2")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request(),
    )
    .await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let pairs = |key: &str| -> Vec<(String, String)> {
        v[key]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| {
                (
                    p["Name"].as_str().unwrap().to_string(),
                    p["Value"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    };
    let p = |n: &str, v: &str| (n.to_string(), v.to_string());
    assert_eq!(
        pairs("AudioLanguages"),
        [
            p("English", "eng"),
            p("French", "fre"),
            p("Japanese", "jpn")
        ]
    );
    assert_eq!(
        pairs("SubtitleLanguages"),
        [p("English", "eng"), p("German", "ger")]
    );
}
