#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Which audio track a client is told to start on.
//!
//! The fixture is Aliens, verified against the real library: three Ukrainian
//! tracks ahead of an English DTS-HD MA, and the FIRST of the Ukrainian ones
//! is the container's `default`. Every rule in play is visible in that one
//! file:
//!
//! * "the first audio stream" — what pharos did — plays Ukrainian;
//! * a language preference alone does NOT fix it, because Jellyfin lets a
//!   default-flagged track win while `PlayDefaultAudioTrack` is on;
//! * turning that setting off is what makes English play;
//! * and a track the viewer actually chose beats all of it on the next
//!   resume, which is the part that made this a daily annoyance rather than
//!   a one-time settings trip.

use actix_web::{test, web, App};
use pharos_core::{
    AudioTrack, MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore, UserId,
    UserPolicy, UserRecord, UserStore,
};
use pharos_server::{
    api::jellyfin,
    auth::BuiltinAuth,
    middleware::LowercasePath,
    state::{AppState, Stores},
};

fn track(stream_index: u32, language: &str, is_default: bool) -> AudioTrack {
    AudioTrack {
        stream_index,
        codec: Some("ac3".into()),
        channels: Some(6),
        language: Some(language.into()),
        is_default,
        ..Default::default()
    }
}

/// Aliens' actual track layout.
fn aliens_tracks() -> Vec<AudioTrack> {
    vec![
        track(1, "ukr", true),
        track(2, "ukr", false),
        track(3, "ukr", false),
        track(4, "eng", false),
    ]
}

async fn seed(configuration: Option<&str>) -> (web::Data<AppState>, String) {
    seed_with_original_language(configuration, None).await
}

async fn seed_with_original_language(
    configuration: Option<&str>,
    original_language: Option<&str>,
) -> (web::Data<AppState>, String) {
    seed_full(configuration, original_language, None).await
}

/// `series_original_language` seeds a SERIES record and makes item 9 an
/// episode of it, so the inheritance path is exercised.
async fn seed_full(
    configuration: Option<&str>,
    original_language: Option<&str>,
    series_original_language: Option<&str>,
) -> (web::Data<AppState>, String) {
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
    if let Some(cfg) = configuration {
        use pharos_core::PreferenceStore;
        stores.set_user_configuration(uid, cfg).await.unwrap();
    }
    let token = stores.issue(uid, "t").await.unwrap();
    stores
        .put(MediaItem {
            id: 9,
            path: "/no/such.mkv".into(),
            title: "Aliens".into(),
            kind: if series_original_language.is_some() {
                MediaKind::Episode
            } else {
                MediaKind::Movie
            },
            series: series_original_language.map(|_| pharos_core::SeriesInfo {
                series_name: "Cowboy Bebop".into(),
                series_folder: None,
                season_number: Some(1),
                episode_number: Some(1),
                ..Default::default()
            }),
            probe: MediaProbe {
                duration_ms: Some(60_000),
                width: Some(1920),
                height: Some(1080),
                bitrate_bps: Some(4_000_000),
                audio_tracks: aliens_tracks(),
                ..Default::default()
            },
            metadata: pharos_core::MediaMetadata {
                original_language: original_language.map(str::to_string),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
    if let Some(lang) = series_original_language {
        use pharos_core::{SeriesMetadata, SeriesMetadataStore};
        stores
            .upsert_series_metadata(SeriesMetadata {
                series_key: "Cowboy Bebop".into(),
                series_name: "Cowboy Bebop".into(),
                original_language: Some(lang.into()),
                ..Default::default()
            })
            .await
            .unwrap();
    }
    let state = web::Data::new(AppState::new(stores, "t".into()));
    (state, token.0.expose().to_string())
}

async fn default_audio_index(state: &web::Data<AppState>, token: &str) -> Option<u32> {
    let app = test::init_service(
        App::new()
            .app_data(state.clone())
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    let req = test::TestRequest::post()
        .uri("/Items/9/PlaybackInfo")
        .insert_header(("X-Emby-Token", token))
        .insert_header(("content-type", "application/json"))
        .set_payload(r#"{"DeviceProfile":{}}"#)
        .to_request();
    let body = test::call_and_read_body(&app, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    v["MediaSources"][0]["DefaultAudioStreamIndex"]
        .as_u64()
        .map(|n| n as u32)
}

async fn report_progress(state: &web::Data<AppState>, token: &str, payload: &str) {
    let app = test::init_service(
        App::new()
            .app_data(state.clone())
            .wrap(LowercasePath)
            .configure(jellyfin::configure),
    )
    .await;
    let req = test::TestRequest::post()
        .uri("/Sessions/Playing/Progress")
        .insert_header(("X-Emby-Token", token))
        .insert_header(("content-type", "application/json"))
        .set_payload(payload.to_string())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success(), "progress report rejected");
}

/// An unconfigured user gets Jellyfin's defaults, under which the container's
/// default-flagged track decides — Ukrainian, as before this work. Pinned so
/// the parity claim is a claim about behaviour and not just about code.
#[actix_web::test]
async fn an_unconfigured_user_gets_the_containers_default_track() {
    let (state, token) = seed(None).await;
    assert_eq!(default_audio_index(&state, &token).await, Some(1));
}

/// The trap: preferring English is not enough on its own.
#[actix_web::test]
async fn a_language_preference_alone_does_not_beat_a_default_flagged_dub() {
    let (state, token) = seed(Some(
        r#"{"AudioLanguagePreference":"eng","PlayDefaultAudioTrack":true}"#,
    ))
    .await;
    assert_eq!(default_audio_index(&state, &token).await, Some(1));
}

/// Preference + "don't just play the default track" is the combination that
/// actually selects English.
#[actix_web::test]
async fn a_language_preference_selects_english_once_the_default_override_is_off() {
    let (state, token) = seed(Some(
        r#"{"AudioLanguagePreference":"eng","PlayDefaultAudioTrack":false}"#,
    ))
    .await;
    assert_eq!(default_audio_index(&state, &token).await, Some(4));
}

/// A two-letter tag in the file still matches the three-letter code the
/// settings UI posts.
#[actix_web::test]
async fn the_preference_matches_however_the_file_tags_the_language() {
    let (state, token) = seed(Some(
        r#"{"AudioLanguagePreference":"en","PlayDefaultAudioTrack":false}"#,
    ))
    .await;
    assert_eq!(default_audio_index(&state, &token).await, Some(4));
}

/// The one that matters day to day: having switched to English once, a
/// resume comes back to English — with the preferences left at Jellyfin's
/// defaults, which would otherwise select the Ukrainian default track.
#[actix_web::test]
async fn a_track_the_viewer_chose_is_restored_on_the_next_play() {
    let (state, token) = seed(None).await;
    assert_eq!(default_audio_index(&state, &token).await, Some(1));

    report_progress(
        &state,
        &token,
        r#"{"ItemId":"00000000000000000000000000000009","PositionTicks":1200000000,"AudioStreamIndex":4}"#,
    )
    .await;

    assert_eq!(default_audio_index(&state, &token).await, Some(4));
}

/// A progress report that says nothing about tracks must not wipe the choice
/// — they arrive every few seconds, and almost none of them mention tracks.
#[actix_web::test]
async fn a_later_report_without_track_fields_keeps_the_remembered_choice() {
    let (state, token) = seed(None).await;
    report_progress(
        &state,
        &token,
        r#"{"ItemId":"00000000000000000000000000000009","PositionTicks":1200000000,"AudioStreamIndex":4}"#,
    )
    .await;
    report_progress(
        &state,
        &token,
        r#"{"ItemId":"00000000000000000000000000000009","PositionTicks":2400000000}"#,
    )
    .await;
    assert_eq!(default_audio_index(&state, &token).await, Some(4));
}

/// A remembered index that no longer exists (the file was replaced with one
/// carrying fewer tracks) must fall back rather than name a missing track.
#[actix_web::test]
async fn a_stale_remembered_index_falls_back_to_the_preference() {
    let (state, token) = seed(None).await;
    report_progress(
        &state,
        &token,
        r#"{"ItemId":"00000000000000000000000000000009","PositionTicks":10,"AudioStreamIndex":99}"#,
    )
    .await;
    assert_eq!(default_audio_index(&state, &token).await, Some(1));
}

/// `OriginalLanguage` is the setting that expresses "each title in the
/// language it was made in" — the only way to want Japanese for anime and
/// English for everything else without maintaining per-library rules.
#[actix_web::test]
async fn original_language_ranks_by_the_titles_own_language() {
    // A Japanese title: the enriched original language selects a Japanese
    // track even though the user never named that language anywhere.
    let (state, token) = seed_with_original_language(
        Some(r#"{"AudioLanguagePreference":"OriginalLanguage","PlayDefaultAudioTrack":false}"#),
        Some("uk"),
    )
    .await;
    // The fixture's "original" language is Ukrainian here, so the preference
    // must land on a Ukrainian track rather than the English one.
    assert_eq!(default_audio_index(&state, &token).await, Some(1));

    // An English original picks English out of the same file.
    let (state, token) = seed_with_original_language(
        Some(r#"{"AudioLanguagePreference":"OriginalLanguage","PlayDefaultAudioTrack":false}"#),
        Some("en"),
    )
    .await;
    assert_eq!(default_audio_index(&state, &token).await, Some(4));
}

/// An item nothing has enriched yet records no original language; the
/// preference must degrade to the container's own order rather than to some
/// arbitrary language.
#[actix_web::test]
async fn original_language_degrades_when_the_item_has_none() {
    let (state, token) = seed(Some(
        r#"{"AudioLanguagePreference":"OriginalLanguage","PlayDefaultAudioTrack":false}"#,
    ))
    .await;
    assert_eq!(default_audio_index(&state, &token).await, Some(1));
}

/// An EPISODE inherits its show's original language. Providers report it on
/// the series record only, so without this the `OriginalLanguage` preference
/// does nothing for TV — measured live: 0 of 14 refreshed episodes carried
/// one, against 9 of 32 movies. Anime is episodes, so this IS the anime case.
#[actix_web::test]
async fn an_episode_inherits_its_shows_original_language() {
    let (state, token) = seed_full(
        Some(r#"{"AudioLanguagePreference":"OriginalLanguage","PlayDefaultAudioTrack":false}"#),
        None,
        Some("eng"),
    )
    .await;
    // The show is English, so the English track wins over the Ukrainian ones
    // even though the episode row itself records no language.
    assert_eq!(default_audio_index(&state, &token).await, Some(4));
}
